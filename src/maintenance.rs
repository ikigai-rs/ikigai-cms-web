//! Graph maintenance: the **link-check pass** as a resource. Sourcing `urn:cms:linkcheck` runs
//! the whole pass — list every bookmark, check each by resolving `urn:httpHead`+`Exists` through
//! the kernel (`ikigai-http`, honest status), reconcile a persisted JSON status cache, and write a
//! `dead-links.org` review — returning a one-line summary. Both the `cms-linkcheck` bin and the
//! `cms-server` `urn:time` schedule just *source* it, so there is one implementation.
//!
//! `ikigai-http` reports status honestly; the *policy* about what's "dead" lives here (the
//! caller): `"true"` = alive, a transient error = unreachable, a permanent one (a 400/403 the server
//! *answered* with) = alive. A HEAD `"false"` (404/410) is NOT trusted alone — many live servers
//! 404 a HEAD but serve a GET — so it is confirmed with a real `urn:httpGet`; only a GET that *also*
//! 404s is `gone`. Each check passes `max_age`, so in the long-lived server a URL checked within the
//! week is a cache hit.
//!
//! The checks run as **parked futures** bounded at [`CONCURRENCY`] in flight — no thread pool. The
//! transport captures a tokio [`Handle`] and spawns each request onto it, so the fan-out works
//! even when the pass is driven synchronously off the `urn:time` timer thread (which has no tokio
//! reactor of its own).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;

use ikigai_core::{
    ArgRef, Description, Endpoint, EndpointSpace, Error, Exact, Fallback, Invocation, Iri, Kernel,
    ReprType, Representation, Request, Result, Space, SystemClock, Verb,
};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};

/// A fresh `ok` result is trusted for a week (also the `max_age` passed to each check); broken
/// URLs are re-checked every run.
const WEEK_SECS: u64 = 7 * 24 * 60 * 60;
/// Checks in flight at once — a politeness/backpressure cap on parked futures, NOT a thread count.
/// Kept low on purpose: a burst of concurrent fresh connections across many hosts exhausts DNS/
/// sockets and manufactures spurious connection failures (a live site checked in the burst fails,
/// yet succeeds on a calm sequential recheck). Link-checking is a nightly background pass — slow and
/// gentle beats fast and wrong.
const CONCURRENCY: usize = 4;
/// A transient (couldn't-connect) failure is retried once after this delay. It's long enough to land
/// *after* a rate-limit window or a DNS hiccup rather than inside it — a 3s retry tended to fail for
/// the same reason the first try did.
const RETRY_BACKOFF: Duration = Duration::from_secs(10);
/// Persist the status cache every this-many completed checks, so an interrupted pass keeps its
/// progress (and a re-run resumes) instead of losing everything (the cache was previously written
/// only at the very end).
const CHECKPOINT_EVERY: usize = 100;
/// A `running: true` meta whose heartbeat is older than this is treated as a dead/interrupted pass,
/// not a live one — a hard-killed pass can't clear its own `running` flag, so the reader ages it out.
const STALE_META_SECS: u64 = 90;

// ---- the reqwest transport (async, self-sufficient off the timer thread) -----------------------

/// A reqwest-backed async HTTP transport. It reports the response **faithfully** (status +
/// headers) — no classification here. It captures the tokio runtime [`Handle`] at construction and
/// **spawns** each request onto it, so the request runs on the runtime's reactor even when the
/// driving executor is a bare `block_on` on the `urn:time` timer's `std::thread` (which is not a
/// tokio context). Must be built inside a tokio runtime.
pub struct ReqwestTransport {
    client: reqwest::Client,
    handle: Handle,
}

impl ReqwestTransport {
    /// A client with a per-request timeout and a polite user-agent (redirects followed by
    /// default). Panics if not called within a tokio runtime — it needs a handle to spawn onto.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            // Patient on purpose: a genuinely dead host fails fast (DNS/refused in <1s regardless of
            // this), so the timeout only matters for slow-but-alive sites (old edu/personal servers).
            // 20s gives them room to answer instead of being false-flagged unreachable.
            .timeout(Duration::from_secs(20))
            .user_agent("ikigai-cms-linkcheck")
            // Every bookmark is a different host, so a per-host idle keep-alive pool is useless and
            // harmful: it accumulates hundreds of open connections and starves DNS/sockets, which
            // shows up as spurious "error sending request" once the pass has run for a while. Don't
            // keep idle connections — connect fresh per check.
            .pool_max_idle_per_host(0)
            .build()
            .expect("build reqwest client");
        ReqwestTransport {
            client,
            handle: Handle::current(),
        }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
        let client = self.client.clone();
        // Spawn onto the captured runtime, then await the join handle. This decouples the reqwest
        // work (which needs a tokio reactor) from whatever executor is driving us — the server's
        // async loop, or a plain block_on on the timer thread.
        let joined = self
            .handle
            .spawn(async move { do_request(&client, req).await })
            .await;
        match joined {
            Ok(result) => result,
            Err(e) => Err(format!("request task failed: {e}")),
        }
    }
}

async fn do_request(
    client: &reqwest::Client,
    req: HttpRequest,
) -> std::result::Result<HttpResponse, String> {
    let method =
        reqwest::Method::from_bytes(req.method.as_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut builder = client.request(method, &req.url);
    for (name, value) in &req.headers {
        builder = builder.header(name, value);
    }
    if !req.body.is_empty() {
        builder = builder.body(req.body);
    }
    let response = builder.send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect();
    let body = response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

// ---- the persisted status + its policy ---------------------------------------------------------

/// The persisted per-URL link status — the reconcile cache across runs (JSON at `status_path`).
#[derive(Clone, Serialize, Deserialize)]
pub struct Status {
    pub url: String,
    pub subject: String,
    pub title: String,
    /// `ok` | `gone` | `unreachable`.
    pub status: String,
    #[serde(default)]
    pub reason: String,
    /// Unix seconds of the last check.
    pub checked_at: u64,
    /// Unix seconds the URL first went (and has since stayed) broken; 0 when `ok`.
    #[serde(default)]
    pub first_broken_at: u64,
    /// How many consecutive runs it has checked broken.
    #[serde(default)]
    pub broken_count: u32,
}

/// The outcome of one check.
enum Outcome {
    Alive,
    Gone(String),
    Unreachable(String),
}

/// Whether a status is auto-removable evidence-wise. **Only a definitive `gone` (HTTP 404/410)
/// qualifies.** `unreachable` (a connection/timeout/DNS error) is NEVER auto-removable on a single
/// check: those failures are too unreliable — a rate-limiting CDN, a DNS hiccup, or the checker
/// throttling its own machine can all fake one. Unreachable links become removable only after
/// *sustained* confirmation ([`durably_unreachable`]), and even then only via the reviewed, human-
/// authorized unreachable purge — never automatically.
pub fn removable(s: &Status) -> bool {
    s.status == "gone"
}

/// Runs an `unreachable` must fail the patient re-check, and time it must stay broken, before it is
/// eligible for the (reviewed, authorized) unreachable purge. A false-unreachable (a slow or
/// burst-throttled but live site) recovers within a run or two and so never reaches the bar; only a
/// persistently unresolvable host accumulates enough failures over enough days.
const DURABLE_RUNS: u32 = 3;
const DURABLE_SECS: u64 = 7 * 24 * 60 * 60;

/// Whether an `unreachable` has failed across enough runs over enough time to be a review-and-purge
/// candidate. Never true for `gone`/`ok`. This is the *eligibility* test; removal still requires the
/// human to review the batch and authorize the purge.
pub fn durably_unreachable(s: &Status, now: u64) -> bool {
    s.status == "unreachable"
        && s.broken_count >= DURABLE_RUNS
        && now.saturating_sub(s.first_broken_at) >= DURABLE_SECS
}

/// Which reviewed set a [`PurgeView`] strikes — the two are separate authorized actions with
/// separate confidence levels (definitive vs sustained-heuristic), never conflated.
#[derive(Clone, Copy)]
pub enum RemovalSet {
    /// Definitively dead: HTTP 404/410, GET-confirmed.
    Gone,
    /// Durably unreachable: failed the patient re-check across ≥[`DURABLE_RUNS`] runs over
    /// ≥[`DURABLE_SECS`] (see [`durably_unreachable`]).
    DurableUnreachable,
}

impl RemovalSet {
    /// The candidate URLs from the status cache for this set.
    fn candidates(self, cache: &HashMap<String, Status>, now: u64) -> HashSet<String> {
        cache
            .values()
            .filter(|s| match self {
                RemovalSet::Gone => removable(s),
                RemovalSet::DurableUnreachable => durably_unreachable(s, now),
            })
            .map(|s| s.url.clone())
            .collect()
    }

    /// The bound purge IRI (also the endpoint's describe subject).
    fn iri(self) -> &'static str {
        match self {
            RemovalSet::Gone => "urn:cms:purge",
            RemovalSet::DurableUnreachable => "urn:cms:purge-unreachable",
        }
    }

    /// The `POST` route the confirm button submits to.
    fn route(self) -> &'static str {
        match self {
            RemovalSet::Gone => "/purge",
            RemovalSet::DurableUnreachable => "/purge-unreachable",
        }
    }
}

/// Load the persisted status cache (empty if absent/unreadable).
pub fn load_status(path: &std::path::Path) -> HashMap<String, Status> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Vec<Status>>(&bytes)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.url.clone(), s))
            .collect(),
        Err(_) => HashMap::new(),
    }
}

fn save_status(path: &std::path::Path, cache: &HashMap<String, Status>) {
    let mut list: Vec<&Status> = cache.values().collect();
    list.sort_by(|a, b| a.url.cmp(&b.url));
    if let Ok(bytes) = serde_json::to_vec_pretty(&list) {
        let _ = std::fs::write(path, bytes);
    }
}

/// The one-line summary a pass returns (and the shape the in-room indicator reads).
pub struct Summary {
    pub checked: usize,
    pub gone: usize,
    pub unreachable: usize,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            "link-check: {} gone (removable) · {} unreachable (flagged) · checked {}",
            self.gone, self.unreachable, self.checked
        )
    }
}

/// The live run state, written to a small meta file beside the status cache so the in-room
/// indicator can show progress while a pass runs (the per-URL status blob has no notion of "now
/// running"). Cheap to write; read on each status poll.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Meta {
    pub running: bool,
    pub checked: usize,
    pub total: usize,
    /// Unix seconds of the last progress tick — a liveness heartbeat. A `running: true` meta with a
    /// stale heartbeat is a pass that was killed before it could clear the flag (see
    /// [`STALE_META_SECS`]).
    #[serde(default)]
    pub heartbeat: u64,
    /// Unix seconds the last pass finished.
    #[serde(default)]
    pub finished_at: u64,
}

/// The meta file path (beside the status cache).
fn meta_path(status_path: &std::path::Path) -> PathBuf {
    status_path.with_file_name("cms-linkcheck-meta.json")
}

fn write_meta(path: &std::path::Path, meta: &Meta) {
    if let Ok(bytes) = serde_json::to_vec(meta) {
        let _ = std::fs::write(path, bytes);
    }
}

fn read_meta(path: &std::path::Path) -> Meta {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The configured status path (`CMS_LINKSTATUS` or the `$HOME/.ikigai` default) WITHOUT creating
/// the dir — so a reader (the status view) resolves the same file the pass writes.
fn resolved_status_path() -> PathBuf {
    std::env::var("CMS_LINKSTATUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".ikigai")
                .join("cms-linkstatus.json")
        })
}

// ---- the pass endpoint -------------------------------------------------------------------------

/// `urn:cms:linkcheck` — run the link-check pass. Sourcing it lists every bookmark, checks each,
/// reconciles the persisted status at `status_path`, writes a `dead-links.org` review beside it,
/// and returns the summary line. Idempotent-ish: the week-cache + reconcile skip fresh URLs.
pub struct LinkCheckPass {
    pub status_path: PathBuf,
}

#[async_trait]
impl Endpoint for LinkCheckPass {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let now = unix_now();
        let bookmarks = list_bookmarks(inv).await?;
        let mut cache = load_status(&self.status_path);

        let fresh_ok = |url: &str| {
            matches!(cache.get(url), Some(s)
                if s.status == "ok" && now.saturating_sub(s.checked_at) < WEEK_SECS)
        };
        // Optional `limit` — check at most N of the non-fresh URLs (handy for a subset run).
        let limit = inv
            .inline_str("limit")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        let to_check: Vec<&(String, String, String)> = bookmarks
            .iter()
            .filter(|(_, url, _)| !fresh_ok(url))
            .take(limit)
            .collect();

        // Live run state for the in-room indicator.
        let meta = meta_path(&self.status_path);
        write_meta(
            &meta,
            &Meta {
                running: true,
                checked: 0,
                total: to_check.len(),
                heartbeat: now,
                finished_at: 0,
            },
        );

        run_checks(inv, &to_check, &mut cache, &self.status_path, &meta, now).await;
        // Drop status for bookmarks that no longer exist.
        let current: HashSet<&str> = bookmarks.iter().map(|(_, u, _)| u.as_str()).collect();
        cache.retain(|url, _| current.contains(url.as_str()));

        save_status(&self.status_path, &cache);
        write_meta(
            &meta,
            &Meta {
                running: false,
                checked: to_check.len(),
                total: to_check.len(),
                heartbeat: unix_now(),
                finished_at: unix_now(),
            },
        );
        write_report(&self.status_path, &cache, now);
        let (gone, unreachable) = buckets(&cache);
        let summary = Summary {
            checked: to_check.len(),
            gone: gone.len(),
            unreachable: unreachable.len(),
        };
        Ok(Representation::new(
            ReprType::new("text/plain"),
            summary.line().into_bytes(),
        ))
    }

    fn name(&self) -> &str {
        "cms-linkcheck"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:linkcheck")
            .summary(
                "Run the link-check pass: HEAD-check every bookmark (a HEAD 404 confirmed by a \
                 GET before it counts as gone), reconcile the persisted status, and write the \
                 dead-links review. Returns a summary line.",
            )
            .verb(Verb::Source)
            .input(
                ikigai_core::ArgSpec::new("limit")
                    .summary("check at most N of the non-fresh URLs (a subset run)"),
            )
            // It dereferences the web, so it needs a net grant (the inner urn:httpHead enforces it).
            .requires("urn:cap:net:*")
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(subject IRI, url, title)` for every bookmark carrying an http(s) `dc:identifier`, deduped.
async fn list_bookmarks(inv: &Invocation<'_>) -> Result<Vec<(String, String, String)>> {
    let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         SELECT ?s ?u ?t WHERE { ?s dc:identifier ?u . \
         FILTER(STRSTARTS(STR(?u), \"http\")) OPTIONAL { ?s dc:title ?t } }";
    let request = Request::new(
        Verb::Source,
        Iri::parse("urn:sparql:select").expect("valid IRI"),
    )
    .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
    .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let repr = inv.issue(request).await?;
    let json: serde_json::Value =
        serde_json::from_slice(&repr.bytes).unwrap_or(serde_json::Value::Null);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    if let Some(rows) = json["results"]["bindings"].as_array() {
        for r in rows {
            let (Some(s), Some(u)) = (r["s"]["value"].as_str(), r["u"]["value"].as_str()) else {
                continue;
            };
            if !seen.insert(u.to_string()) {
                continue;
            }
            let t = r["t"]["value"].as_str().unwrap_or(u);
            out.push((s.to_string(), u.to_string(), t.to_string()));
        }
    }
    Ok(out)
}

/// Run every item as a parked future bounded at [`CONCURRENCY`] in flight, folding each outcome into
/// `cache` **as it lands** — updating the meta heartbeat each tick, and persisting the status cache
/// every [`CHECKPOINT_EVERY`] completions. Streaming (not collect-then-apply) is what makes an
/// interrupted pass keep its progress: the cache on disk is never more than `CHECKPOINT_EVERY` checks
/// behind, and the heartbeat lets the reader tell a live pass from a killed one.
async fn run_checks(
    inv: &Invocation<'_>,
    items: &[&(String, String, String)],
    cache: &mut HashMap<String, Status>,
    status_path: &std::path::Path,
    meta: &std::path::Path,
    now: u64,
) {
    let total = items.len();
    let done = AtomicUsize::new(0);
    // Own the `(index, url)` pairs so each future borrows only `inv`/`done` (one clear lifetime),
    // not the slice of borrows — a borrowed stream item trips a higher-ranked-lifetime "FnOnce is
    // not general enough" under the `async_trait`-boxed `invoke`.
    let work: Vec<(usize, String)> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (i, it.1.clone()))
        .collect();
    let mut results = futures::stream::iter(work)
        .map(|(i, url)| check_one(inv, &done, total, i, url))
        .buffer_unordered(CONCURRENCY);
    while let Some((idx, outcome)) = results.next().await {
        let (subject, url, title) = items[idx];
        let entry = merge(cache.get(url), subject, url, title, &outcome, now);
        cache.insert(url.clone(), entry);
        let n = done.load(Ordering::Relaxed);
        // A tiny meta write each tick keeps the heartbeat fresh (cheap); a full status checkpoint
        // only every CHECKPOINT_EVERY (the bigger write).
        write_meta(
            meta,
            &Meta {
                running: true,
                checked: n,
                total,
                heartbeat: unix_now(),
                finished_at: 0,
            },
        );
        if n.is_multiple_of(CHECKPOINT_EVERY) {
            save_status(status_path, cache);
        }
    }
}

/// One item's check + progress log.
async fn check_one(
    inv: &Invocation<'_>,
    done: &AtomicUsize,
    total: usize,
    i: usize,
    url: String,
) -> (usize, Outcome) {
    let outcome = check(inv, &url).await;
    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
    match &outcome {
        Outcome::Gone(r) => eprintln!("[{n}/{total}] GONE    {url}  ({r})"),
        Outcome::Unreachable(r) => eprintln!("[{n}/{total}] unreach {url}  ({r})"),
        Outcome::Alive => {}
    }
    (i, outcome)
}

/// One reachability check, retrying a transient failure once after a short backoff — so a
/// rate-limited or DNS-hiccup request that fails on the first try gets a second chance to come back
/// `ok` instead of being flagged unreachable. Only `Unreachable` is transient; `Gone`/`Alive` are
/// final. The delay is via `futures-timer`, so it works whether the pass is driven by tokio or by
/// the `urn:time` timer thread.
async fn check(inv: &Invocation<'_>, url: &str) -> Outcome {
    match check_once(inv, url).await {
        Outcome::Unreachable(_) => {
            futures_timer::Delay::new(RETRY_BACKOFF).await;
            check_once(inv, url).await
        }
        final_outcome => final_outcome,
    }
}

/// A single reachability check. The cheap first pass is `urn:httpHead`+`Exists` (cacheable a week):
/// `"true"` = alive, a **transient** error = unreachable, a **permanent** one (a status the server
/// answered with) = alive (a HEAD-hostile but live site isn't condemned). A `"false"` (HEAD 404/410)
/// is NOT trusted on its own — **many servers 404 a HEAD but serve a GET** — so we confirm with a
/// real GET before ever concluding `gone`. Only a GET that *also* 404s is a removal candidate.
async fn check_once(inv: &Invocation<'_>, url: &str) -> Outcome {
    let request = Request::new(Verb::Exists, Iri::parse("urn:httpHead").expect("valid IRI"))
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "max_age",
            ArgRef::Inline(WEEK_SECS.to_string().into_bytes()),
        );
    match inv.issue(request).await {
        Ok(repr) => match repr.bytes.as_slice() {
            b"true" => Outcome::Alive,
            b"false" => confirm_gone_with_get(inv, url).await,
            other => {
                Outcome::Unreachable(format!("unexpected: {}", String::from_utf8_lossy(other)))
            }
        },
        Err(e) if e.is_transient() => Outcome::Unreachable(e.to_string()),
        Err(_) => Outcome::Alive,
    }
}

/// A HEAD said 404/410 — but HEAD is unreliable (plenty of live servers reject it with a 404 while
/// serving the same URL on GET). Ask the authority: a real `urn:httpGet`. A GET that succeeds means
/// the page is live (HEAD-hostile, not gone); a GET that *also* 404s (`Error::NotFound`) is a genuine
/// `gone`; a transient GET error is `unreachable`; any other answered status (403/400/…) means the
/// resource is there, just not fetchable this way — not gone.
async fn confirm_gone_with_get(inv: &Invocation<'_>, url: &str) -> Outcome {
    let request = Request::new(Verb::Source, Iri::parse("urn:httpGet").expect("valid IRI"))
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "max_age",
            ArgRef::Inline(WEEK_SECS.to_string().into_bytes()),
        );
    match inv.issue(request).await {
        Ok(_) => Outcome::Alive,
        Err(Error::NotFound(_)) => Outcome::Gone("HTTP 404/410 (GET-confirmed)".to_string()),
        Err(e) if e.is_transient() => Outcome::Unreachable(e.to_string()),
        Err(_) => Outcome::Alive,
    }
}

/// Fold a check outcome into the prior status, carrying forward sustained-deadness tracking.
fn merge(
    prev: Option<&Status>,
    subject: &str,
    url: &str,
    title: &str,
    outcome: &Outcome,
    now: u64,
) -> Status {
    let base = |status: &str, reason: String, first: u64, count: u32| Status {
        url: url.to_string(),
        subject: subject.to_string(),
        title: title.to_string(),
        status: status.to_string(),
        reason,
        checked_at: now,
        first_broken_at: first,
        broken_count: count,
    };
    let (status, reason) = match outcome {
        Outcome::Alive => return base("ok", String::new(), 0, 0),
        Outcome::Gone(r) => ("gone", r.clone()),
        Outcome::Unreachable(r) => ("unreachable", r.clone()),
    };
    let still_broken = prev.map(|p| p.status != "ok" && p.first_broken_at > 0);
    let (first, count) = match (still_broken, prev) {
        (Some(true), Some(p)) => (p.first_broken_at, p.broken_count + 1),
        _ => (now, 1),
    };
    base(status, reason, first, count)
}

/// Partition into (gone, unreachable), each sorted by URL. `gone` is the removable set;
/// `unreachable` is flagged for the human but never auto-removed (see [`removable`]).
fn buckets(cache: &HashMap<String, Status>) -> (Vec<&Status>, Vec<&Status>) {
    let (mut gone, mut unreachable) = (Vec::new(), Vec::new());
    for s in cache.values() {
        match s.status.as_str() {
            "gone" => gone.push(s),
            "unreachable" => unreachable.push(s),
            _ => {}
        }
    }
    for v in [&mut gone, &mut unreachable] {
        v.sort_by(|a, b| a.url.cmp(&b.url));
    }
    (gone, unreachable)
}

/// Partition into (gone, durably-unreachable, under-observation), each sorted by URL — the three
/// removal/flag categories the review and the org worksheet both speak.
fn dead_buckets(
    cache: &HashMap<String, Status>,
    now: u64,
) -> (Vec<&Status>, Vec<&Status>, Vec<&Status>) {
    let (gone, unreachable) = buckets(cache);
    let (durable, observing): (Vec<&Status>, Vec<&Status>) = unreachable
        .into_iter()
        .partition(|s| durably_unreachable(s, now));
    (gone, durable, observing)
}

/// The three review buckets for the in-room view: `gone`, durably-`unreachable`, and the *count*
/// still under observation (tracked but not offered for removal, so a count suffices there).
fn review_buckets(
    cache: &HashMap<String, Status>,
    now: u64,
) -> (Vec<&Status>, Vec<&Status>, usize) {
    let (gone, durable, observing) = dead_buckets(cache, now);
    (gone, durable, observing.len())
}

/// (Re)write the `dead-links.org` worksheet beside the status cache from the *current* cache — so
/// it stays in step with the room. Called at the end of every pass AND right after a purge (the
/// purge prunes the cache, so regenerating here keeps the worksheet from lagging the removal).
fn write_report(status_path: &std::path::Path, cache: &HashMap<String, Status>, now: u64) {
    let (gone, durable, observing) = dead_buckets(cache, now);
    let path = status_path.with_file_name("dead-links.org");
    let _ = std::fs::write(path, report(&gone, &durable, &observing, now));
}

/// The human-readable review as an org file, one section per category so the worksheet tells the
/// same story as the room: removable `gone`, removal-eligible durable-`unreachable`, and the
/// `unreachable` still under observation (not yet eligible).
fn report(gone: &[&Status], durable: &[&Status], observing: &[&Status], now: u64) -> String {
    let mut s = String::from("#+TITLE: Dead bookmarks — link check\n\n");
    let mut section = |title: &str, items: &[&Status], note: &str| {
        s.push_str(&format!("* {title}: {}\n", items.len()));
        if !note.is_empty() {
            s.push_str(&format!("  # {note}\n"));
        }
        for it in items {
            s.push_str(&format!("** [[{}][{}]]\n", it.url, org_safe(&it.title)));
            let dead_days = now.saturating_sub(it.first_broken_at) / 86_400;
            s.push_str(&format!(
                "   {} · {} · dead {}d · checked {}×\n",
                it.status,
                org_safe(&it.reason),
                dead_days,
                it.broken_count
            ));
        }
        s.push('\n');
    };
    section("Gone — removable", gone, "HTTP 404/410: definitively dead");
    section(
        "Unreachable, durable — removal-eligible",
        durable,
        "couldn't connect across ≥3 checks over ≥7 days — offered for the reviewed unreachable purge",
    );
    section(
        "Unreachable, under observation — not yet eligible",
        observing,
        "connection/timeout/DNS errors, but not yet sustained (need 3 failed checks over a week)",
    );
    s
}

fn org_safe(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
        .replace('[', "(")
        .replace(']', ")")
}

// ---- the in-room status view -------------------------------------------------------------------

/// `urn:cms:linkstatus` — a small HTML fragment for the room's live link-check indicator. While a
/// pass runs it shows progress (`checking links… 1,240 / 5,373`); otherwise it shows the last
/// run's tally from the persisted status (`links: 650 gone · 638 unreachable · checked 2h ago`);
/// nothing before the first run. Reads the meta + status files (`CMS_LINKSTATUS`/default) — it does
/// no network, so it can live in the serving kernel and be polled by htmx.
pub struct LinkStatusView;

#[async_trait]
impl Endpoint for LinkStatusView {
    async fn invoke(&self, _inv: &Invocation<'_>) -> Result<Representation> {
        let status_path = resolved_status_path();
        let meta = read_meta(&meta_path(&status_path));
        let cache = load_status(&status_path);
        let html = status_fragment(&meta, &cache, unix_now());
        Ok(Representation::new(
            ReprType::new("text/html").with_param("charset", "utf-8"),
            html.into_bytes(),
        ))
    }

    fn name(&self) -> &str {
        "cms-linkstatus"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:linkstatus")
            .summary("The room's live link-check indicator: a small HTML fragment of the run progress or the last-run tally.")
            .verb(Verb::Source)
    }
}

/// Render the indicator fragment: run progress while a pass is running, else the last-run tally
/// from the persisted status, else empty (never run).
fn status_fragment(meta: &Meta, cache: &HashMap<String, Status>, now: u64) -> String {
    // `running` only counts if the heartbeat is fresh — a hard-killed pass leaves `running: true`
    // frozen, so an aged-out heartbeat means "interrupted", and we fall through to the last tally.
    let live = meta.running && now.saturating_sub(meta.heartbeat) <= STALE_META_SECS;
    if live {
        return format!(
            "<span class=\"cms-linkcheck running\">checking links… {} / {}</span>",
            group(meta.checked),
            group(meta.total)
        );
    }
    if cache.is_empty() {
        return String::new();
    }
    let (gone, unreachable) = buckets(cache);
    let last = cache.values().map(|s| s.checked_at).max().unwrap_or(0);
    format!(
        "<span class=\"cms-linkcheck\">links: {} gone · {} unreachable · checked {}</span>",
        group(gone.len()),
        group(unreachable.len()),
        ago(now, last),
    )
}

/// `urn:cms:review` — the suggested-deletes review as an htmx card fragment. Reads the persisted
/// status: the **removable** set (`gone` = HTTP 404/410) becomes cards, and the count of **flagged**
/// `unreachable` links rides in the header (they're surfaced but never auto-removed — connection
/// errors are too unreliable). A view is a query; here the query is the removable set. Read-only —
/// the authorized purge is a separate action.
pub struct ReviewView;

#[async_trait]
impl Endpoint for ReviewView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let cache = load_status(&resolved_status_path());
        let now = unix_now();
        let (gone, durable, observing) = review_buckets(&cache, now);
        let xml = review_xml(&gone, &durable, observing, now);
        let req = Request::new(
            Verb::Source,
            Iri::parse("urn:xslt:transform").expect("valid IRI"),
        )
        .with_arg("content", ArgRef::Inline(xml.into_bytes()))
        .with_arg(
            "stylesheet",
            ArgRef::Inline(b"urn:cms:style:review".to_vec()),
        );
        let out = inv.issue(req).await?;
        // NOT cacheable: this reads the status file directly (std::fs, not through the kernel), so
        // there's no golden thread to invalidate it — a cached fragment would freeze at its first
        // render and keep showing already-purged links. Recompute every resolve (a cheap file read).
        Ok(Representation::new(
            ReprType::new("text/html").with_param("charset", "utf-8"),
            out.bytes,
        ))
    }

    fn name(&self) -> &str {
        "cms-review"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:review")
            .summary("The suggested-deletes review: the link-check removal candidates rendered as cards.")
            .verb(Verb::Source)
    }
}

/// Build the review doc (`urn:cms:review#`): a `<section>` per removal category (each carrying its
/// own purge action + call-to-action), plus a `<note>` for the count still under observation. Each
/// section is emitted only when non-empty, so the xrust stylesheet needs no conditionals — it just
/// renders whatever sections exist (and `<empty/>` when there are none at all).
fn review_xml(gone: &[&Status], durable: &[&Status], observing: usize, now: u64) -> String {
    let mut s = String::from("<review xmlns=\"urn:cms:review#\">");
    if gone.is_empty() && durable.is_empty() {
        s.push_str("<empty/>");
    } else {
        if !gone.is_empty() {
            section_xml(
                &mut s,
                gone,
                now,
                &format!(
                    "{} removable — definitively dead (HTTP 404/410)",
                    group(gone.len())
                ),
                RemovalSet::Gone.iri(),
                "Purge dead links…",
            );
        }
        if !durable.is_empty() {
            section_xml(
                &mut s,
                durable,
                now,
                &format!(
                    "{} durably unreachable — failed ≥3 checks over ≥7 days",
                    group(durable.len())
                ),
                RemovalSet::DurableUnreachable.iri(),
                "Purge unreachable…",
            );
        }
    }
    if observing > 0 {
        s.push_str("<note>");
        xml_text(
            &mut s,
            &format!(
                "{} more unreachable under observation — not yet eligible (need 3 failed checks \
                 over a week).",
                group(observing)
            ),
        );
        s.push_str("</note>");
    }
    s.push_str("</review>");
    s
}

/// Emit one `<section>` (header label + purge action + call-to-action) wrapping its item cards. The
/// section `kind` (for card colour) is the items' shared status — `gone` or `unreachable`.
fn section_xml(s: &mut String, items: &[&Status], now: u64, label: &str, action: &str, cta: &str) {
    s.push_str("<section kind=\"");
    xml_attr(s, &items[0].status);
    s.push_str("\" label=\"");
    xml_attr(s, label);
    s.push_str("\" action=\"");
    xml_attr(s, action);
    s.push_str("\" cta=\"");
    xml_attr(s, cta);
    s.push_str("\">");
    for it in items {
        let days = now.saturating_sub(it.first_broken_at) / 86_400;
        s.push_str("<item status=\"");
        xml_attr(s, &it.status);
        s.push_str("\" reason=\"");
        xml_attr(s, &it.reason);
        s.push_str(&format!("\" days=\"{days}\" url=\""));
        xml_attr(s, &it.url);
        s.push_str("\" title=\"");
        xml_attr(s, &it.title);
        s.push_str("\"/>");
    }
    s.push_str("</section>");
}

/// Escape a value for an XML double-quoted attribute.
fn xml_attr(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Escape a value for XML element text content.
fn xml_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

// ---- the authorized purge ----------------------------------------------------------------------

/// `urn:cms:purge` — the reviewed removal. Resolving it with `Source` returns the confirm prompt
/// (safe, idempotent); with `Sink` it *executes*: back up the bookmarks file, strike the confirmed
/// removal candidates (`gone` + confirmed-`unreachable`) by URL, and write it back **through the
/// kernel** — which cuts the bookmarks golden thread, so the derived graph re-derives and the room
/// refreshes live. The HTTP face only reaches the `Sink` on an authenticated `POST /purge`.
pub struct PurgeView {
    /// Which reviewed set this instance strikes (`gone` vs durably-`unreachable`).
    pub set: RemovalSet,
    /// The `urn:cms:src:{subpath}` IRI of the bookmarks file (what `BookmarkGraph` reads).
    pub bookmarks_iri: String,
    /// The `urn:cms:src:{subpath}.bak` IRI the old content is backed up to. Each set uses its own
    /// backup file so purging one doesn't clobber the other's backup.
    pub bak_iri: String,
}

#[async_trait]
impl Endpoint for PurgeView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        if inv.request.verb == Verb::Sink {
            self.execute(inv).await
        } else {
            Ok(fragment(self.confirm_html()))
        }
    }

    fn name(&self) -> &str {
        match self.set {
            RemovalSet::Gone => "cms-purge",
            RemovalSet::DurableUnreachable => "cms-purge-unreachable",
        }
    }

    fn describe(&self) -> Description {
        Description::new(self.set.iri())
            .summary(
                "Purge the reviewed removal set from the source file (Sink executes, Source \
                 returns the confirm prompt). Backs the file up first, then writes through the \
                 kernel so the graph re-derives.",
            )
            .verb(Verb::Sink)
            // It writes the bookmarks file (and its backup) through the fs resource.
            .requires("urn:cap:fs:write:*")
    }
}

impl PurgeView {
    /// The confirm prompt (Source): the candidate count + a Confirm/Cancel pair, worded per set.
    fn confirm_html(&self) -> String {
        let cache = load_status(&resolved_status_path());
        let now = unix_now();
        let n = self.set.candidates(&cache, now).len();
        if n == 0 {
            return "<div class=\"cms-purge\"><p>Nothing to purge.</p>\
                <button hx-get=\"/r/urn:cms:review\">back</button></div>"
                .to_string();
        }
        let what = match self.set {
            RemovalSet::Gone => format!("<b>{}</b> definitively-dead (404/410) links", group(n)),
            RemovalSet::DurableUnreachable => format!(
                "<b>{}</b> durably-unreachable links (repeatedly couldn't connect across ≥3 checks \
                 over ≥7 days — almost all dead domains, but a rare persistently-slow site could \
                 still be alive)",
                group(n)
            ),
        };
        format!(
            "<div class=\"cms-purge\"><p>Remove {what} from the bookmarks file? A backup is saved \
             first — this can't be undone from the room.</p>\
             <button class=\"cms-purge-go\" hx-post=\"{}\">Confirm purge</button> \
             <button hx-get=\"/r/urn:cms:review\">Cancel</button></div>",
            self.set.route()
        )
    }

    /// Execute (Sink): back up, strike, write through the kernel (→ live refresh), and drop the
    /// purged URLs from the status cache too.
    async fn execute(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let status_path = resolved_status_path();
        let now = unix_now();
        let cache = load_status(&status_path);
        // Owned URLs, so the cache borrow is released before we rewrite the cache below. The
        // candidate set is exactly this instance's reviewed set — `gone`, or durably-`unreachable`.
        let removable: HashSet<String> = self.set.candidates(&cache, now);
        if removable.is_empty() {
            return Ok(fragment(
                "<div class=\"cms-purge\"><p>Nothing to purge.</p></div>".to_string(),
            ));
        }
        let iri = Iri::parse(&self.bookmarks_iri)
            .map_err(|e| Error::Endpoint(format!("bad bookmarks iri: {e}")))?;
        let bak = Iri::parse(&self.bak_iri)
            .map_err(|e| Error::Endpoint(format!("bad backup iri: {e}")))?;
        // Read the current file through the kernel, strike the matching entries.
        let current = inv.source(&iri).await?;
        let text = String::from_utf8_lossy(&current.bytes).into_owned();
        let refs: HashSet<&str> = removable.iter().map(String::as_str).collect();
        let (new_text, removed) = strike(&text, &refs);
        // Back up the old content, then write the new — the write cuts the bookmarks golden thread
        // (BookmarkGraph depends on it), so the graph re-derives and the room refreshes live.
        inv.issue(
            Request::new(Verb::Sink, bak).with_arg("content", ArgRef::Inline(text.into_bytes())),
        )
        .await?;
        inv.issue(
            Request::new(Verb::Sink, iri)
                .with_arg("content", ArgRef::Inline(new_text.into_bytes())),
        )
        .await?;
        // The review view + indicator read the status cache (not the graph), so drop the purged
        // URLs from it too — otherwise they'd keep showing as candidates until the next pass prunes
        // them. Re-load in case a pass wrote it meanwhile.
        let mut cache = load_status(&status_path);
        cache.retain(|url, _| !removable.contains(url));
        save_status(&status_path, &cache);
        // Regenerate the dead-links.org worksheet from the pruned cache so it doesn't lag the room
        // (the purge just removed these; the next scheduled pass would otherwise be the only thing
        // to refresh the org file).
        write_report(&status_path, &cache, now);
        Ok(fragment(format!(
            "<div class=\"cms-purge\"><p>Removed <b>{}</b> dead links · backup saved. The room has \
             refreshed.</p><button hx-get=\"/r/urn:cms:tags\">back to the room</button></div>",
            group(removed)
        )))
    }
}

/// Remove the org entries whose bookmark URL is in `removable`: drop each matching `*`-heading and
/// the non-heading lines under it (its property drawer/body), keeping everything else verbatim.
/// Returns the new content and how many entries were struck.
fn strike(content: &str, removable: &HashSet<&str>) -> (String, usize) {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::with_capacity(content.len());
    let mut removed = 0;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if heading_url(line).is_some_and(|u| removable.contains(u.as_str())) {
            // Drop the heading and its subtree (following non-heading lines).
            removed += 1;
            i += 1;
            while i < lines.len() && !lines[i].starts_with('*') {
                i += 1;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    (out, removed)
}

/// The bookmark URL in an org heading `*… [[url][title]]` (or `[[url]]`), or `None` if the line is
/// not a heading or has no link.
fn heading_url(line: &str) -> Option<String> {
    if !line.starts_with('*') {
        return None;
    }
    let start = line.find("[[")? + 2;
    let rest = &line[start..];
    let end = rest.find([']', '['])?; // up to the `][` separator or the closing `]]`
    Some(rest[..end].to_string())
}

/// Wrap an HTML string as an uncacheable text/html representation.
fn fragment(html: String) -> Representation {
    Representation::new(
        ReprType::new("text/html").with_param("charset", "utf-8"),
        html.into_bytes(),
    )
}

/// A short "time ago" for the last check.
fn ago(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then);
    if secs < 90 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m ago", secs / 60)
    } else if secs < 172_800 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Thousands-separate a count for display (`5373` → `5,373`).
fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*c as char);
    }
    out
}

// ---- S2: OpenLibrary tag suggestion ------------------------------------------------------------

/// Polite spacing between OpenLibrary lookups — one book at a time, a beat apart, so a batch of
/// suggestions never looks like a scrape (Conan the Librarian stays calm).
const OL_SPACING: Duration = Duration::from_secs(2);

/// `urn:cms:tag-suggest` — suggest tags for untagged books. Sourcing it queries the graph for books
/// with no tag (and no pending suggestion), looks each up in OpenLibrary (ISBN-first, then title),
/// turns the subjects into clean candidate tags, and writes them to the suggestions overlay for
/// your `+`/`x` review. Capped per run (`limit`, default 5) and paced — gentle by design. The LLM
/// residual that maps these onto your vocabulary (and coins better ones) is S3.
pub struct TagSuggestPass {
    /// The `urn:llm:{provider}:*` backend the pass asks and probes — a name from the
    /// registry (`~/.config/ikigai/llm.json`), resolved by [`resolve_llm_provider`].
    provider: String,
    /// The overlay store the pass reads (curated vocabulary) and writes (suggestions).
    tags: crate::tagstore::TagPaths,
}

#[async_trait]
impl Endpoint for TagSuggestPass {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        // GATE (Brian's requirement): only run if the local LLM is up — otherwise no OpenLibrary
        // calls, nothing written. `urn:llm:{provider}:up` is a cheap liveness probe.
        if !llm_up(inv, &self.provider).await {
            return Ok(Representation::new(
                ReprType::new("text/plain"),
                b"tag-suggest: skipped \xe2\x80\x94 local LLM unavailable".to_vec(),
            ));
        }
        let limit = inv
            .inline_str("limit")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(5);
        // Two vocabularies for the prompt: the human-CURATED tags (the approved overlay — always fed
        // as "strongly prefer", so an accepted tag steers suggestions from its first acceptance) and
        // the COMMON existing tags (top-200 by frequency, minus the curated ones already listed).
        let curated = self.tags.approved_tags();
        let common: Vec<String> = tag_vocabulary(inv)
            .await
            .into_iter()
            .filter(|t| !curated.contains(t))
            .collect();
        let books = list_untagged_books(inv, limit).await?;
        eprintln!(
            "[tag-suggest] local LLM is up; {} untagged book(s) to check, {} curated + {} common tags in vocab",
            books.len(),
            curated.len(),
            common.len()
        );
        let (mut tagged, mut added, mut via_llm, mut via_fallback) =
            (0usize, 0usize, 0usize, 0usize);
        for (i, b) in books.iter().enumerate() {
            if i > 0 {
                futures_timer::Delay::new(OL_SPACING).await;
            }
            let subjects = openlibrary_subjects(inv, &b.isbn, &b.title).await;
            // The LLM maps the noisy OpenLibrary subjects + the book's description onto the user's
            // vocabulary (coining a new tag when warranted); if the ask fails or yields nothing,
            // fall back to the deterministic S2 filter so the book still gets *something*.
            let (tags, source) =
                match llm_tags(inv, &self.provider, b, &subjects, &curated, &common).await {
                    Some(t) => (t, "llm"),
                    None => (filter_subjects(&subjects), "openlibrary-fallback"),
                };
            // Say where each book's tags come from, and what the LLM was working from — so a run is
            // legible: LLM-refined vs the deterministic fallback, and the raw OpenLibrary input.
            let ol = if subjects.is_empty() {
                "(no OpenLibrary match)".to_string()
            } else {
                subjects.join(", ")
            };
            let out = if tags.is_empty() {
                "(no suggestion)".to_string()
            } else {
                tags.join(", ")
            };
            eprintln!("[tag-suggest] [{source}] \"{}\"", b.title);
            eprintln!("    OpenLibrary: {ol}");
            eprintln!("    → {out}");
            if !tags.is_empty() {
                tagged += 1;
                if source == "llm" {
                    via_llm += 1;
                } else {
                    via_fallback += 1;
                }
            }
            for t in &tags {
                self.tags.add_suggestion(&b.id, t);
                added += 1;
            }
        }
        let line = format!(
            "tag-suggest: {added} suggestions across {tagged}/{} books \
             ({via_llm} via LLM, {via_fallback} via OpenLibrary fallback)",
            books.len()
        );
        Ok(Representation::new(
            ReprType::new("text/plain"),
            line.into_bytes(),
        ))
    }

    fn name(&self) -> &str {
        "cms-tag-suggest"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:tag-suggest")
            .summary(
                "Suggest tags for untagged books: OpenLibrary subjects + the book's description, \
                 refined by the local LLM onto your tag vocabulary, written to the suggestions \
                 overlay for review. Runs only if the LLM is up. ISBN-first, capped + paced.",
            )
            .verb(Verb::Source)
            .input(
                ikigai_core::ArgSpec::new("limit")
                    .optional()
                    .summary("check at most N untagged books this run (default 5)"),
            )
            .requires("urn:cap:net:*")
    }
}

/// An untagged book and the context the LLM tags it from.
struct UntaggedBook {
    id: String,
    title: String,
    isbn: String,
    author: String,
    description: String,
}

/// Whether the local LLM answers a liveness probe. Never errors — unreachable = down = skip.
async fn llm_up(inv: &Invocation<'_>, provider: &str) -> bool {
    let Ok(iri) = Iri::parse(format!("urn:llm:{provider}:up")) else {
        return false;
    };
    let request = Request::new(Verb::Source, iri);
    matches!(inv.issue(request).await, Ok(r) if r.bytes == b"true")
}

/// The user's existing tag vocabulary — the top-200 tags by use, so the LLM prefers established
/// tags over inventing near-duplicates. Best-effort (empty on error).
async fn tag_vocabulary(inv: &Invocation<'_>) -> Vec<String> {
    let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         SELECT ?tag (COUNT(?s) AS ?n) WHERE { ?s dc:subject ?tag } \
         GROUP BY ?tag ORDER BY DESC(?n) LIMIT 200";
    let request = Request::new(
        Verb::Source,
        Iri::parse("urn:sparql:select").expect("valid IRI"),
    )
    .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
    .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let Ok(repr) = inv.issue(request).await else {
        return Vec::new();
    };
    let json: serde_json::Value =
        serde_json::from_slice(&repr.bytes).unwrap_or(serde_json::Value::Null);
    json["results"]["bindings"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r["tag"]["value"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Ask the LLM for clean tags. `None` on any failure (→ the caller falls back to the S2 filter) or
/// when the model returns nothing usable.
async fn llm_tags(
    inv: &Invocation<'_>,
    provider: &str,
    book: &UntaggedBook,
    subjects: &[String],
    curated: &[String],
    common: &[String],
) -> Option<Vec<String>> {
    let system =
        "You tag books for a personal knowledge base. Reply with ONLY 2-4 short lowercase \
         topical tags separated by commas — no sentences, no explanation. STRONGLY prefer the \
         user's curated tags when one fits; otherwise a common existing tag; coin a new concise \
         tag only when nothing existing fits or a clear topic is missing. Never output publisher \
         names, classification codes, or generic words like \"general\".";
    let mut prompt = format!("Book: \"{}\"", book.title);
    if !book.author.is_empty() {
        prompt.push_str(&format!(" by {}", book.author));
    }
    prompt.push('.');
    if !book.description.is_empty() {
        let d: String = book.description.chars().take(400).collect();
        prompt.push_str(&format!("\nDescription: {d}"));
    }
    if !subjects.is_empty() {
        prompt.push_str(&format!("\nOpenLibrary subjects: {}", subjects.join(", ")));
    }
    if !curated.is_empty() {
        prompt.push_str(&format!(
            "\nYour curated tags (strongly prefer these): {}",
            curated.join(", ")
        ));
    }
    if !common.is_empty() {
        prompt.push_str(&format!("\nOther existing tags: {}", common.join(", ")));
    }
    prompt.push_str("\nTags:");
    // Route to the pass's provider explicitly (not the `urn:llm:ask` facade) so the
    // configured choice — not the registry's default — answers.
    let request = Request::new(
        Verb::Source,
        Iri::parse(format!("urn:llm:{provider}:ask")).ok()?,
    )
    .with_arg("system", ArgRef::Inline(system.as_bytes().to_vec()))
    .with_arg("prompt", ArgRef::Inline(prompt.into_bytes()))
    .with_arg("temperature", ArgRef::Inline(b"0.2".to_vec()))
    .with_arg("max_tokens", ArgRef::Inline(b"64".to_vec()));
    let repr = inv.issue(request).await.ok()?;
    let tags = parse_llm_tags(&String::from_utf8_lossy(&repr.bytes));
    (!tags.is_empty()).then_some(tags)
}

/// Parse the model's reply (comma/newline-separated) into clean slug tags: slugify, drop empties /
/// over-verbose phrases, dedupe, cap at 4.
fn parse_llm_tags(reply: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in reply.split([',', '\n', ';']) {
        let slug = slug_tag(part);
        if slug.len() < 2 || !slug.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if slug.matches('-').count() > 3 {
            continue;
        }
        if !out.contains(&slug) {
            out.push(slug);
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// Untagged books (no tag, no pending suggestion) with the context the LLM needs — title-ordered
/// (stable), capped at `limit`. Skipping already-suggested books makes re-runs advance, not repeat.
async fn list_untagged_books(inv: &Invocation<'_>, limit: usize) -> Result<Vec<UntaggedBook>> {
    let query = format!(
        "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
         SELECT ?id ?title ?isbn ?author ?description WHERE {{ \
           ?id a cms:Book ; dc:title ?title . \
           OPTIONAL {{ ?id cms:isbn ?isbn }} OPTIONAL {{ ?id dc:creator ?author }} \
           OPTIONAL {{ ?id dc:description ?description }} \
           FILTER NOT EXISTS {{ ?id dc:subject ?sub }} \
           FILTER NOT EXISTS {{ ?id cms:suggestedTag ?sg }} \
           FILTER NOT EXISTS {{ ?id cms:dismissedTag ?dt }} }} ORDER BY ?title LIMIT {limit}"
    );
    let request = Request::new(
        Verb::Source,
        Iri::parse("urn:sparql:select").expect("valid IRI"),
    )
    .with_arg("query", ArgRef::Inline(query.into_bytes()))
    .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let repr = inv.issue(request).await?;
    let json: serde_json::Value =
        serde_json::from_slice(&repr.bytes).unwrap_or(serde_json::Value::Null);
    let field = |r: &serde_json::Value, k: &str| r[k]["value"].as_str().unwrap_or("").to_string();
    let mut out = Vec::new();
    if let Some(rows) = json["results"]["bindings"].as_array() {
        for r in rows {
            let (Some(id), Some(title)) = (r["id"]["value"].as_str(), r["title"]["value"].as_str())
            else {
                continue;
            };
            out.push(UntaggedBook {
                id: id.to_string(),
                title: title.to_string(),
                isbn: field(r, "isbn"),
                author: field(r, "author"),
                description: field(r, "description"),
            });
        }
    }
    Ok(out)
}

/// The response shape of the two OpenLibrary endpoints we read.
enum OlShape {
    /// `/api/books?...jscmd=data` → `{ "ISBN:x": { subjects: [{name}] } }`.
    Books,
    /// `/search.json?...` → `{ docs: [ { subject: [..] } ] }`.
    Search,
}

/// OpenLibrary subjects for a book: ISBN-first (exact edition), then a title search. Best-effort —
/// any error or miss yields an empty list (the book simply gets no suggestion this run).
async fn openlibrary_subjects(inv: &Invocation<'_>, isbn: &str, title: &str) -> Vec<String> {
    if !isbn.is_empty() {
        let url =
            format!("https://openlibrary.org/api/books?bibkeys=ISBN:{isbn}&format=json&jscmd=data");
        let subs = ol_fetch(inv, &url, OlShape::Books).await;
        if !subs.is_empty() {
            return subs;
        }
    }
    if !title.is_empty() {
        let url = format!(
            "https://openlibrary.org/search.json?title={}&fields=subject&limit=1",
            url_encode(title)
        );
        return ol_fetch(inv, &url, OlShape::Search).await;
    }
    Vec::new()
}

/// One `urn:httpGet` to OpenLibrary (cacheable a week — its subject data is stable), parsed for the
/// subject list per `shape`.
async fn ol_fetch(inv: &Invocation<'_>, url: &str, shape: OlShape) -> Vec<String> {
    let request = Request::new(Verb::Source, Iri::parse("urn:httpGet").expect("valid IRI"))
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "max_age",
            ArgRef::Inline(WEEK_SECS.to_string().into_bytes()),
        );
    let Ok(repr) = inv.issue(request).await else {
        return Vec::new();
    };
    let json: serde_json::Value =
        serde_json::from_slice(&repr.bytes).unwrap_or(serde_json::Value::Null);
    match shape {
        OlShape::Books => json
            .as_object()
            .into_iter()
            .flatten()
            .flat_map(|(_, v)| v["subjects"].as_array().cloned().unwrap_or_default())
            .filter_map(|s| s["name"].as_str().map(String::from))
            .collect(),
        OlShape::Search => json["docs"]
            .as_array()
            .and_then(|d| d.first())
            .and_then(|d| d["subject"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Percent-encode a query-string value (the OpenLibrary title search).
fn url_encode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// Raw OpenLibrary subjects → clean candidate tags: drop classification/BISAC codes and all-caps
/// category headers, slugify, drop over-verbose phrases, dedupe, cap at 4. Deliberately
/// deterministic — the LLM residual (map to your vocabulary, coin better tags) is S3.
fn filter_subjects(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in raw {
        if is_subject_junk(s) {
            continue;
        }
        let slug = slug_tag(s);
        if slug.len() < 2 || !slug.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if slug.matches('-').count() > 3 {
            continue; // > 4 words — too verbose to be a good tag
        }
        if !out.contains(&slug) {
            out.push(slug);
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// Whether a raw OpenLibrary subject is code/noise rather than a topic: a classification code
/// (`cs.cmp_sc.app_sw`), a BISAC code (`Com051260`), or an ALL-CAPS category header
/// (`BUSINESS & ECONOMICS`).
fn is_subject_junk(raw: &str) -> bool {
    let t = raw.trim();
    if t.contains('.') {
        return true;
    }
    if t.len() >= 6
        && t.is_char_boundary(3)
        && t[..3].chars().all(|c| c.is_ascii_alphabetic())
        && t[3..].chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    let letters: String = t.chars().filter(|c| c.is_ascii_alphabetic()).collect();
    letters.len() > 4 && letters.chars().all(|c| c.is_ascii_uppercase())
}

/// Lowercase-hyphen slug (matching the book graph's tag slugs).
fn slug_tag(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

// ---- kernel assembly ---------------------------------------------------------------------------

/// A maintenance kernel: the CMS graph spaces (bookmarks + `zotero` books) + the `ikigai-http`
/// outbound endpoints over `transport` + the `urn:cms:linkcheck` and `urn:cms:tag-suggest` passes,
/// with a system clock so cacheable reads honor their deadlines. `transport` is injectable so a
/// test can supply a canned one.
// Flat by design: each arg mirrors one resolved config value, and a test injects its own
// transport/registry — bundling them would just add an intermediate struct nobody else uses.
#[allow(clippy::too_many_arguments)]
pub fn maintenance_kernel(
    src_dir: PathBuf,
    zotero: Option<PathBuf>,
    bookmarks: Option<String>,
    status_path: PathBuf,
    tags: crate::tagstore::TagPaths,
    transport: std::sync::Arc<dyn HttpTransport>,
    registry: ikigai_llm::Registry,
    llm_provider: &str,
) -> Kernel {
    let mut spaces = crate::cms_spaces_with(src_dir, zotero, None, bookmarks, tags.clone());
    spaces.push(
        std::sync::Arc::new(ikigai_http::space(transport.clone())) as std::sync::Arc<dyn Space>
    );
    // The LLM backends over the SAME transport, every registry provider bound at
    // `urn:llm:{provider}:*`; the tag-suggest pass asks and probes `llm_provider`'s.
    spaces
        .push(std::sync::Arc::new(ikigai_llm::space(transport, registry))
            as std::sync::Arc<dyn Space>);
    spaces.push(std::sync::Arc::new(
        EndpointSpace::new()
            .bind(
                Exact::new("urn:cms:linkcheck"),
                LinkCheckPass { status_path },
            )
            .bind(
                Exact::new("urn:cms:tag-suggest"),
                TagSuggestPass {
                    provider: llm_provider.to_string(),
                    tags,
                },
            ),
    ) as std::sync::Arc<dyn Space>);
    Kernel::new(std::sync::Arc::new(Fallback::new(spaces)))
        .with_clock(std::sync::Arc::new(SystemClock))
}

/// The compiled-in registry when no `llm.json` exists: a local Ollama, small model.
pub fn default_llm_registry() -> ikigai_llm::Registry {
    ikigai_llm::Registry::single(ikigai_llm::OpenAiConfig::ollama("llama3.2"))
}

/// The LLM registry from the config home (`~/.config/ikigai/llm.json`) when present —
/// a malformed file fails loud — else [`default_llm_registry`].
pub fn llm_registry() -> std::result::Result<ikigai_llm::Registry, String> {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) => llm_registry_at(&home.join(".config/ikigai/llm.json")),
        None => Ok(default_llm_registry()),
    }
}

fn llm_registry_at(path: &std::path::Path) -> std::result::Result<ikigai_llm::Registry, String> {
    if !path.is_file() {
        return Ok(default_llm_registry());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    ikigai_llm::Registry::from_json(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Resolve which provider the passes use: an explicit choice (cms.toml `llm_provider`)
/// must name a registry provider — fail loud, never silently fall back; unset means the
/// registry's own default.
pub fn resolve_llm_provider(
    registry: &ikigai_llm::Registry,
    explicit: Option<&str>,
) -> std::result::Result<String, String> {
    match explicit {
        Some(p) if registry.providers.iter().any(|c| c.provider == p) => Ok(p.to_string()),
        Some(p) => Err(format!(
            "llm_provider {p}: not in the registry (have: {})",
            registry
                .providers
                .iter()
                .map(|c| c.provider.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
        None => Ok(registry.default.clone()),
    }
}

/// The default persisted-status path: `$HOME/.ikigai/cms-linkstatus.json` — the ikigai-owned
/// state directory (created if missing), kept out of your synced content dirs. `CMS_LINKSTATUS`
/// overrides it; `dead-links.org` is written beside it.
pub fn default_status_path() -> PathBuf {
    let dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".ikigai");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("cms-linkstatus.json")
}

/// [`maintenance_kernel`] over the real reqwest transport and the config-home LLM
/// registry. Must be built inside a tokio runtime. Errors (fail loud) on a malformed
/// `llm.json` or an `llm_provider` the registry doesn't know.
pub fn build_maintenance_kernel(
    src_dir: PathBuf,
    zotero: Option<PathBuf>,
    bookmarks: Option<String>,
    status_path: PathBuf,
    tags: crate::tagstore::TagPaths,
    llm_provider: Option<String>,
) -> std::result::Result<Kernel, String> {
    let registry = llm_registry()?;
    let provider = resolve_llm_provider(&registry, llm_provider.as_deref())?;
    Ok(maintenance_kernel(
        src_dir,
        zotero,
        bookmarks,
        status_path,
        tags,
        std::sync::Arc::new(ReqwestTransport::new()),
        registry,
        &provider,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikigai_core::{Capability, Clock, Time};
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;

    /// A canned transport returning a fixed status, counting sends.
    struct Canned {
        status: u16,
        sends: Arc<AtomicU32>,
    }
    #[async_trait]
    impl HttpTransport for Canned {
        async fn send(&self, _req: HttpRequest) -> std::result::Result<HttpResponse, String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(HttpResponse {
                status: self.status,
                headers: Vec::new(),
                body: Vec::new(),
            })
        }
    }

    /// A canned transport that routes by URL: the OpenLibrary lookup, the LLM chat completion, and
    /// the LLM liveness probe (`/models`) each get their own 200 body — so one transport can drive
    /// the whole tag-suggest pass (probe → OpenLibrary → ask).
    struct RoutedCanned;
    #[async_trait]
    impl HttpTransport for RoutedCanned {
        async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
            let body: &[u8] = if req.url.contains("openlibrary") {
                br#"{"ISBN:9781617294556":{"subjects":[{"name":"Rust (Computer program language)"},{"name":"Com051260"}]}}"#
            } else if req.url.contains("chat/completions") {
                // The model maps the noisy subjects onto clean tags.
                br#"{"choices":[{"message":{"content":"rust, systems-programming"}}]}"#
            } else {
                b"{}" // the /models liveness probe → 200 → up
            };
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: body.to_vec(),
            })
        }
    }

    #[test]
    fn parse_llm_tags_slugs_splits_dedupes_and_caps() {
        let tags = parse_llm_tags("Rust, systems programming; rust\ndistributed systems, networking, extra one two three four five");
        assert_eq!(
            tags,
            vec![
                "rust".to_string(),
                "systems-programming".to_string(),
                "distributed-systems".to_string(),
                "networking".to_string()
            ]
        );
    }

    #[test]
    fn filter_subjects_drops_codes_headers_and_verbose_slugs_and_caps() {
        let raw: Vec<String> = [
            "Rust (Computer program language)",
            "Com051260",            // BISAC code
            "cs.cmp_sc.app_sw",     // classification code (has '.')
            "BUSINESS & ECONOMICS", // all-caps header
            "Systems programming",
            "General computing extra words here now", // too verbose (>4 words)
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let tags = filter_subjects(&raw);
        assert!(
            tags.contains(&"rust-computer-program-language".to_string()),
            "{tags:?}"
        );
        assert!(
            tags.contains(&"systems-programming".to_string()),
            "{tags:?}"
        );
        assert!(
            !tags.iter().any(|t| t.contains("com051260")),
            "BISAC dropped: {tags:?}"
        );
        assert!(
            !tags.iter().any(|t| t.contains('.')),
            "codes dropped: {tags:?}"
        );
        assert!(
            !tags.contains(&"business-economics".to_string()),
            "header dropped: {tags:?}"
        );
        assert!(
            !tags.iter().any(|t| t.matches('-').count() > 3),
            "verbose dropped: {tags:?}"
        );
        assert!(tags.len() <= 4);
    }

    /// A tempdir with a near-empty bookmarks file (so the graph assembles) and an untagged book
    /// whose subject IRI carries its ISBN. Returns (dir, zotero path).
    fn untagged_book_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(&bm, "* Bookmarks\n").unwrap();
        let z = dir.path().join("z.rdf");
        std::fs::write(
            &z,
            r##"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:bib="http://purl.org/net/biblio#" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:z="http://www.zotero.org/namespaces/export#">
  <bib:Book rdf:about="urn:isbn:9781617294556"><z:itemType>book</z:itemType><dc:title>Rust in Action</dc:title></bib:Book>
</rdf:RDF>"##,
        )
        .unwrap();
        (dir, z)
    }

    #[test]
    fn tag_suggest_writes_the_llms_refined_tags_for_an_untagged_book() {
        let (dir, z) = untagged_book_fixture();
        let tags = crate::tagstore::TagPaths::in_dir(dir.path());
        // RoutedCanned: /models probe → up, OpenLibrary → noisy subjects, chat → "rust, systems-programming".
        let kernel = maintenance_kernel(
            dir.path().to_path_buf(),
            Some(z),
            None,
            dir.path().join("st.json"),
            tags.clone(),
            Arc::new(RoutedCanned),
            default_llm_registry(),
            "ollama",
        );
        let req = Request::new(Verb::Source, Iri::parse("urn:cms:tag-suggest").unwrap());
        let repr = futures::executor::block_on(kernel.issue(req, &Capability::root()))
            .expect("tag-suggest runs");
        let summary = String::from_utf8(repr.bytes).unwrap();
        assert!(summary.contains("2 suggestions"), "summary: {summary}");
        assert!(
            summary.contains("1 via LLM"),
            "source differentiated: {summary}"
        );
        // The LLM's tags landed (not the raw/deterministic ones), keyed on the book IRI.
        let sug = crate::tagstore::entries(&tags.suggestions);
        let landed: Vec<&str> = sug.iter().map(|e| e.tag.as_str()).collect();
        assert!(landed.contains(&"rust"), "{landed:?}");
        assert!(landed.contains(&"systems-programming"), "{landed:?}");
        assert_eq!(sug.len(), 2, "{landed:?}");
        assert!(
            sug.iter().all(|e| e.iri.starts_with("urn:cms:book:")),
            "{sug:?}"
        );
    }

    #[test]
    fn a_dismissed_book_drops_out_of_the_candidate_set() {
        let (dir, z) = untagged_book_fixture();
        let tags = crate::tagstore::TagPaths::in_dir(dir.path());
        let kernel = maintenance_kernel(
            dir.path().to_path_buf(),
            Some(z),
            None,
            dir.path().join("st.json"),
            tags.clone(),
            Arc::new(RoutedCanned),
            default_llm_registry(),
            "ollama",
        );
        // Find the book IRI and dismiss a tag on it (as the `x` button does).
        let sel = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
            .with_arg(
                "query",
                ArgRef::Inline(b"PREFIX cms: <https://ikigai-rs.dev/ns/cms#> SELECT ?id WHERE { ?id a cms:Book } LIMIT 1".to_vec()),
            )
            .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
        let repr = futures::executor::block_on(kernel.issue(sel, &Capability::root())).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&repr.bytes).unwrap();
        let id = v["results"]["bindings"][0]["id"]["value"]
            .as_str()
            .unwrap()
            .to_string();
        tags.reject(&id, "not-a-fit");

        // Now the pass finds no candidate (the sole book is dismissed) — no OpenLibrary/LLM churn.
        let req = Request::new(Verb::Source, Iri::parse("urn:cms:tag-suggest").unwrap());
        let repr = futures::executor::block_on(kernel.issue(req, &Capability::root()))
            .expect("tag-suggest runs");
        let summary = String::from_utf8(repr.bytes).unwrap();
        assert!(summary.contains("across 0/0"), "book excluded: {summary}");
        assert!(crate::tagstore::entries(&tags.suggestions).is_empty());
    }

    #[test]
    fn tag_suggest_skips_when_the_llm_is_down() {
        let (dir, z) = untagged_book_fixture();
        let tags = crate::tagstore::TagPaths::in_dir(dir.path());
        // Every request 503s → the liveness probe reads `false` → the pass no-ops.
        let sends = Arc::new(AtomicU32::new(0));
        let kernel = maintenance_kernel(
            dir.path().to_path_buf(),
            Some(z),
            None,
            dir.path().join("st.json"),
            tags.clone(),
            Arc::new(Canned {
                status: 503,
                sends: Arc::clone(&sends),
            }),
            default_llm_registry(),
            "ollama",
        );
        let req = Request::new(Verb::Source, Iri::parse("urn:cms:tag-suggest").unwrap());
        let repr = futures::executor::block_on(kernel.issue(req, &Capability::root()))
            .expect("tag-suggest runs");
        assert!(
            String::from_utf8_lossy(&repr.bytes).contains("skipped"),
            "should skip when LLM down"
        );
        // Nothing written, and no OpenLibrary call was made (only the one liveness probe).
        assert!(crate::tagstore::entries(&tags.suggestions).is_empty());
        assert_eq!(
            sends.load(Ordering::SeqCst),
            1,
            "only the liveness probe ran"
        );
    }

    #[derive(Clone)]
    struct TestClock(Arc<AtomicU64>);
    impl Clock for TestClock {
        fn now(&self) -> Time {
            Time::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    fn kernel_over(
        status: u16,
        sends: Arc<AtomicU32>,
        clock: TestClock,
        status_path: PathBuf,
    ) -> Kernel {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n** [[https://sci.example][Science]]\n   :PROPERTIES:\n   :TAGS: science\n   :END:\n",
        )
        .unwrap();
        let tags = crate::tagstore::TagPaths::in_dir(dir.path());
        let src = dir.keep();
        let mut spaces = crate::cms_spaces_with(src, None, None, None, tags);
        spaces.push(
            Arc::new(ikigai_http::space(Arc::new(Canned { status, sends }))) as Arc<dyn Space>,
        );
        spaces.push(Arc::new(EndpointSpace::new().bind(
            Exact::new("urn:cms:linkcheck"),
            LinkCheckPass { status_path },
        )) as Arc<dyn Space>);
        Kernel::new(Arc::new(Fallback::new(spaces))).with_clock(Arc::new(clock))
    }

    #[test]
    fn sourcing_the_pass_checks_bookmarks_and_persists_status() {
        let dir = tempfile::tempdir().unwrap();
        let status_path = dir.path().join("status.json");
        let clock = TestClock(Arc::new(AtomicU64::new(0)));
        // The one fixture bookmark returns 404 → gone.
        let kernel = kernel_over(404, Arc::new(AtomicU32::new(0)), clock, status_path.clone());
        let req = Request::new(Verb::Source, Iri::parse("urn:cms:linkcheck").unwrap());
        let repr =
            futures::executor::block_on(kernel.issue(req, &Capability::root())).expect("pass runs");
        let summary = String::from_utf8(repr.bytes).unwrap();
        assert!(summary.contains("1 gone"), "summary: {summary}");
        // Status was persisted, and the bookmark is recorded gone.
        let cache = load_status(&status_path);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.values().next().unwrap().status, "gone");

        // The status indicator resource is mounted in the same kernel and resolves (content comes
        // from the env/default path, so we only assert it's reachable, not its text).
        let sreq = Request::new(Verb::Source, Iri::parse("urn:cms:linkstatus").unwrap());
        assert!(
            futures::executor::block_on(kernel.issue(sreq, &Capability::root())).is_ok(),
            "urn:cms:linkstatus resolves"
        );

        // Point the view endpoints at the same controlled cache the pass wrote, then add a
        // durably-unreachable entry (old + many runs) and an observing one (too few runs) so the
        // review exercises BOTH the gone section AND the durable-unreachable section + the note.
        std::env::set_var("CMS_LINKSTATUS", &status_path);
        let mut cache = load_status(&status_path);
        let mut durable = st("unreachable", 0, 4); // first_broken at epoch (>7d ago), 4 runs
        durable.url = "http://durable.example".into();
        cache.insert(durable.url.clone(), durable);
        let mut observing = st("unreachable", 0, 1); // only 1 run → still under observation
        observing.url = "http://observing.example".into();
        cache.insert(observing.url.clone(), observing);
        save_status(&status_path, &cache);

        // The review view resolves through the review stylesheet (xrust) — this exercises the
        // whole pipeline (endpoint → build XML → urn:xslt:transform → urn:cms:style:review).
        let rreq = Request::new(Verb::Source, Iri::parse("urn:cms:review").unwrap());
        let rrepr = futures::executor::block_on(kernel.issue(rreq, &Capability::root()))
            .expect("review resolves");
        let html = String::from_utf8_lossy(&rrepr.bytes);
        // Both sections render, each wired to its own purge action, plus the observation note.
        assert!(
            html.contains("/r/urn:cms:purge"),
            "gone purge button: {html}"
        );
        assert!(
            html.contains("/r/urn:cms:purge-unreachable"),
            "unreachable purge button: {html}"
        );
        assert!(
            html.contains("http://durable.example"),
            "durable card renders: {html}"
        );
        assert!(
            html.contains("under observation"),
            "observation note renders: {html}"
        );

        // Both purge confirm prompts (Source — safe, no mutation) resolve and render, each wording
        // its own set and posting to its own route.
        let preq = Request::new(Verb::Source, Iri::parse("urn:cms:purge").unwrap());
        let prepr = futures::executor::block_on(kernel.issue(preq, &Capability::root()))
            .expect("purge confirm resolves");
        assert!(
            String::from_utf8_lossy(&prepr.bytes).contains("hx-post=\"/purge\""),
            "gone purge confirm posts to /purge"
        );
        let ureq = Request::new(
            Verb::Source,
            Iri::parse("urn:cms:purge-unreachable").unwrap(),
        );
        let urepr = futures::executor::block_on(kernel.issue(ureq, &Capability::root()))
            .expect("unreachable purge confirm resolves");
        let uhtml = String::from_utf8_lossy(&urepr.bytes);
        assert!(
            uhtml.contains("hx-post=\"/purge-unreachable\"")
                && uhtml.contains("durably-unreachable"),
            "unreachable purge confirm posts to /purge-unreachable: {uhtml}"
        );

        // Execute the gone purge (Sink): strikes the gone bookmark from the file, prunes the cache,
        // and REGENERATES dead-links.org from the pruned cache — so the worksheet doesn't lag.
        let xreq = Request::new(Verb::Sink, Iri::parse("urn:cms:purge").unwrap());
        let xrepr = futures::executor::block_on(kernel.issue(xreq, &Capability::root()))
            .expect("gone purge executes");
        assert!(
            String::from_utf8_lossy(&xrepr.bytes).contains("Removed"),
            "purge reports removal"
        );
        let org = std::fs::read_to_string(status_path.with_file_name("dead-links.org")).unwrap();
        // The purged gone link is gone from the worksheet; the un-purged unreachables remain, each
        // in its own (now split) section.
        assert!(!org.contains("sci.example"), "purged link dropped: {org}");
        assert!(
            org.contains("http://durable.example"),
            "durable listed: {org}"
        );
        assert!(
            org.contains("removal-eligible") && org.contains("under observation"),
            "worksheet split into durable + observation sections: {org}"
        );
    }

    fn st(status: &str, first: u64, count: u32) -> Status {
        Status {
            url: "http://x".into(),
            subject: "urn:cms:bookmark:x".into(),
            title: "X".into(),
            status: status.into(),
            reason: "r".into(),
            checked_at: first + (count as u64).saturating_sub(1) * 86_400,
            first_broken_at: first,
            broken_count: count,
        }
    }

    #[test]
    fn merge_tracks_sustained_deadness_and_resets_on_recovery() {
        let now0 = 1_000_000u64;
        let first = merge(
            None,
            "s",
            "http://x",
            "X",
            &Outcome::Unreachable("t".into()),
            now0,
        );
        assert_eq!(first.status, "unreachable");
        assert_eq!(first.broken_count, 1);
        let later = merge(
            Some(&first),
            "s",
            "http://x",
            "X",
            &Outcome::Unreachable("t".into()),
            now0 + 86_400,
        );
        assert_eq!(later.broken_count, 2);
        assert_eq!(later.first_broken_at, now0);
        let ok = merge(
            Some(&later),
            "s",
            "http://x",
            "X",
            &Outcome::Alive,
            now0 + 90_000,
        );
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.broken_count, 0);
    }

    #[test]
    fn only_gone_is_removable_never_unreachable() {
        // A definitive 404/410 (`gone`) is removable; `unreachable` never, no matter how many times
        // or how long it has failed (connection errors are too unreliable to delete on).
        assert!(removable(&st("gone", 0, 1)));
        assert!(!removable(&st("unreachable", 0, 9)));
        assert!(!removable(&st("ok", 0, 0)));
    }

    #[test]
    fn the_status_fragment_shows_progress_running_tally_idle_and_nothing_before_a_run() {
        let now = 3_000_000u64;
        // Running with a FRESH heartbeat → progress, thousands-grouped, with the `running` class.
        let running = status_fragment(
            &Meta {
                running: true,
                checked: 1_240,
                total: 5_373,
                heartbeat: now,
                finished_at: 0,
            },
            &HashMap::new(),
            now,
        );
        assert!(
            running.contains("checking links… 1,240 / 5,373"),
            "{running}"
        );
        assert!(running.contains("cms-linkcheck running"), "{running}");
        // Running but the heartbeat is STALE (killed pass, flag never cleared) → NOT shown as
        // running; with no cache it falls through to empty.
        let stale = status_fragment(
            &Meta {
                running: true,
                checked: 1_240,
                total: 5_373,
                heartbeat: now - STALE_META_SECS - 1,
                finished_at: 0,
            },
            &HashMap::new(),
            now,
        );
        assert!(
            stale.is_empty(),
            "stale running should not show progress: {stale}"
        );
        // Idle with a cache → the tally + "ago" (all entries checked in the same run, 2h ago).
        let mut cache = HashMap::new();
        let checked = |status: &str| {
            let mut s = st(status, now - 7_200, 1);
            s.checked_at = now - 7_200; // 2h ago
            s
        };
        cache.insert("g".into(), checked("gone"));
        cache.insert("u".into(), checked("unreachable"));
        let idle = status_fragment(&Meta::default(), &cache, now);
        assert!(idle.contains("1 gone"), "{idle}");
        assert!(idle.contains("1 unreachable"), "{idle}");
        assert!(idle.contains("2h ago"), "{idle}");
        // Never run → empty (the CSS hides an empty indicator).
        assert!(status_fragment(&Meta::default(), &HashMap::new(), now).is_empty());
    }

    #[test]
    fn durably_unreachable_needs_sustained_failure_never_gone_or_ok() {
        let now = 30 * 86_400u64;
        // 3 runs over ≥7 days → eligible.
        assert!(durably_unreachable(
            &st("unreachable", now - 7 * 86_400, 3),
            now
        ));
        // Enough runs but not enough elapsed time → not yet.
        assert!(!durably_unreachable(
            &st("unreachable", now - 3 * 86_400, 5),
            now
        ));
        // Old enough but too few runs → not yet.
        assert!(!durably_unreachable(
            &st("unreachable", now - 30 * 86_400, 2),
            now
        ));
        // gone / ok are never in the unreachable set.
        assert!(!durably_unreachable(&st("gone", now - 30 * 86_400, 9), now));
        assert!(!durably_unreachable(&st("ok", 0, 0), now));
        // …and gone is removable while a fresh unreachable is not (single-check).
        assert!(RemovalSet::Gone
            .candidates(&HashMap::from([("http://x".into(), st("gone", 0, 1))]), now)
            .contains("http://x"));
    }

    #[test]
    fn review_xml_emits_sections_per_set_escaped_or_an_empty_marker() {
        let now = 3_000_000u64;
        let mut g = st("gone", now - 172_800, 1); // 2 days
        g.url = "https://x/?a=1&b=2".into();
        g.title = "A \"quoted\" <title>".into();
        g.reason = "HTTP 404/410 (gone)".into();
        let u = st("unreachable", now - 10 * 86_400, 4);
        let xml = review_xml(&[&g], &[&u], 638, now);
        // Two sections, each with its own purge action; the observing count rides in a note.
        assert!(xml.contains("action=\"urn:cms:purge\""), "{xml}");
        assert!(
            xml.contains("action=\"urn:cms:purge-unreachable\""),
            "{xml}"
        );
        assert!(xml.contains("<note>"), "{xml}");
        assert!(xml.contains("638 more unreachable"), "{xml}");
        assert!(xml.contains("status=\"gone\""), "{xml}");
        assert!(xml.contains("days=\"2\""), "{xml}");
        // Attribute values are XML-escaped.
        assert!(xml.contains("a=1&amp;b=2"), "{xml}");
        assert!(xml.contains("&quot;quoted&quot; &lt;title&gt;"), "{xml}");
        // No candidates at all → the empty marker (and no note when nothing observing).
        let empty = review_xml(&[], &[], 0, now);
        assert!(empty.contains("<empty/>"), "{empty}");
        assert!(!empty.contains("<note>"), "{empty}");
    }

    #[test]
    fn heading_url_extracts_the_link_target() {
        assert_eq!(
            heading_url("** [[https://x/y][Title]]").as_deref(),
            Some("https://x/y")
        );
        assert_eq!(
            heading_url("** [[https://x/y]]").as_deref(),
            Some("https://x/y")
        );
        assert_eq!(heading_url("   indented [[https://x]]"), None); // not a heading
        assert_eq!(heading_url("** no link here"), None);
    }

    #[test]
    fn strike_removes_matching_entries_and_their_drawers_keeping_the_rest() {
        let content = "* Bookmarks\n\
            ** [[https://dead.example][Dead]]\n   :PROPERTIES:\n   :ID: 1\n   :END:\n\
            ** [[https://live.example][Live]]\n   :PROPERTIES:\n   :ID: 2\n   :END:\n\
            ** [[https://gone.example]]\n";
        let mut removable = HashSet::new();
        removable.insert("https://dead.example");
        removable.insert("https://gone.example");
        let (out, removed) = strike(content, &removable);
        assert_eq!(removed, 2);
        assert!(!out.contains("dead.example"), "{out}");
        assert!(!out.contains("gone.example"), "{out}");
        assert!(
            !out.contains(":ID: 1"),
            "struck entry's drawer is gone: {out}"
        );
        assert!(out.contains("live.example"), "kept: {out}");
        assert!(out.contains(":ID: 2"), "kept entry's drawer stays: {out}");
        assert!(out.contains("* Bookmarks"), "parent heading stays: {out}");
    }

    #[test]
    fn llm_registry_loads_the_file_fails_loud_and_defaults_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        // Absent → the compiled default (ollama, small model).
        let reg = llm_registry_at(&dir.path().join("llm.json")).expect("default");
        assert_eq!(reg.default, "ollama");
        // Present + valid → the file's providers.
        let good = dir.path().join("good.json");
        std::fs::write(
            &good,
            r#"{ "default": "mlx", "providers": {
                 "mlx": { "base_url": "http://localhost:8080/v1", "model": "llama-70b" },
                 "ollama": { "base_url": "http://localhost:11434/v1", "model": "llama3.2:3b" } } }"#,
        )
        .unwrap();
        let reg = llm_registry_at(&good).expect("parses");
        assert_eq!(reg.default, "mlx");
        assert_eq!(reg.providers.len(), 2);
        // Present + malformed → fail loud, naming the file.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, "{ nope").unwrap();
        let err = llm_registry_at(&bad).unwrap_err();
        assert!(err.contains("bad.json"), "names the file: {err}");
    }

    #[test]
    fn llm_provider_explicit_must_exist_unset_takes_the_default() {
        let reg = default_llm_registry();
        assert_eq!(resolve_llm_provider(&reg, None).expect("default"), "ollama");
        assert_eq!(
            resolve_llm_provider(&reg, Some("ollama")).expect("named"),
            "ollama"
        );
        let err = resolve_llm_provider(&reg, Some("mlx")).unwrap_err();
        assert!(err.contains("mlx") && err.contains("ollama"), "{err}");
    }
}
