//! Graph maintenance: the **link-check pass** as a resource. Sourcing `urn:cms:linkcheck` runs
//! the whole pass — list every bookmark, check each by resolving `urn:httpHead`+`Exists` through
//! the kernel (`ikigai-http`, honest status), reconcile a persisted JSON status cache, and write a
//! `dead-links.org` review — returning a one-line summary. Both the `cms-linkcheck` bin and the
//! `cms-server` `urn:time` schedule just *source* it, so there is one implementation.
//!
//! `ikigai-http` reports status honestly; the *policy* about what's "dead" lives here (the
//! caller): `"true"` = alive, `"false"` = gone (404/410), a transient error = unreachable, a
//! permanent one (a 400/403 the server *answered* with) = alive. Each check passes `max_age`, so
//! in the long-lived server a URL checked within the week is a cache hit.
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
/// URLs are re-checked every run so a sustained failure is confirmed sooner.
const WEEK_SECS: u64 = 7 * 24 * 60 * 60;
/// An `unreachable` link is a removal candidate only once it has stayed broken across ≥2 runs
/// spanning at least this long — so a one-off timeout never qualifies.
const CONFIRM_SPAN_SECS: u64 = 24 * 60 * 60;
/// Checks in flight at once — a politeness/backpressure cap on parked futures, NOT a thread count.
/// Kept modest: a big burst across thousands of distinct hosts overwhelms the system DNS resolver
/// (spurious connection failures), and link-checking isn't latency-critical.
const CONCURRENCY: usize = 24;

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
            .timeout(Duration::from_secs(12))
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

/// Whether a broken status is a removal candidate: `gone` always; `unreachable` only once it has
/// stayed broken across ≥2 runs spanning at least [`CONFIRM_SPAN_SECS`]. Public so the review
/// view (a later phase) can bucket the same way.
pub fn removable(s: &Status, now: u64) -> bool {
    match s.status.as_str() {
        "gone" => true,
        "unreachable" => {
            s.broken_count >= 2 && now.saturating_sub(s.first_broken_at) >= CONFIRM_SPAN_SECS
        }
        _ => false,
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

/// The one-line summary a pass returns (and the shape a later in-room indicator reads).
pub struct Summary {
    pub checked: usize,
    pub gone: usize,
    pub confirmed: usize,
    pub pending: usize,
}

impl Summary {
    fn line(&self) -> String {
        format!(
            "link-check: {} gone · {} unreachable-confirmed · {} pending (checked {})",
            self.gone, self.confirmed, self.pending, self.checked
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
                finished_at: 0,
            },
        );

        for (idx, outcome) in check_all(inv, &to_check, &meta).await {
            let (subject, url, title) = to_check[idx];
            let entry = merge(cache.get(url), subject, url, title, &outcome, now);
            cache.insert(url.clone(), entry);
        }
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
                finished_at: now,
            },
        );
        let (gone, confirmed, pending) = buckets(&cache, now);
        let report_path = self.status_path.with_file_name("dead-links.org");
        let _ = std::fs::write(report_path, report(&gone, &confirmed, &pending, now));

        let summary = Summary {
            checked: to_check.len(),
            gone: gone.len(),
            confirmed: confirmed.len(),
            pending: pending.len(),
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
                "Run the link-check pass: HEAD-check every bookmark, reconcile the persisted \
                 status, and write the dead-links review. Returns a summary line.",
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

/// Check `items` as parked futures bounded at [`CONCURRENCY`] in flight, ticking the meta file's
/// progress so the in-room indicator can update.
async fn check_all(
    inv: &Invocation<'_>,
    items: &[&(String, String, String)],
    meta: &std::path::Path,
) -> Vec<(usize, Outcome)> {
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
    futures::stream::iter(work)
        .map(|(i, url)| check_one(inv, &done, total, meta, i, url))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await
}

/// One item's check + progress log, ticking the meta file every 25 completions (a benign race on
/// the counter — the indicator is approximate).
async fn check_one(
    inv: &Invocation<'_>,
    done: &AtomicUsize,
    total: usize,
    meta: &std::path::Path,
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
    if n.is_multiple_of(25) {
        write_meta(
            meta,
            &Meta {
                running: true,
                checked: n,
                total,
                finished_at: 0,
            },
        );
    }
    (i, outcome)
}

/// One reachability check via `urn:httpHead`+`Exists` (cacheable a week). `"true"` = alive,
/// `"false"` = gone; a **transient** error = unreachable (couldn't reach), a **permanent** one (a
/// status the server answered with) = alive (a HEAD-hostile but live site isn't condemned).
async fn check(inv: &Invocation<'_>, url: &str) -> Outcome {
    let request = Request::new(Verb::Exists, Iri::parse("urn:httpHead").expect("valid IRI"))
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "max_age",
            ArgRef::Inline(WEEK_SECS.to_string().into_bytes()),
        );
    match inv.issue(request).await {
        Ok(repr) => match repr.bytes.as_slice() {
            b"true" => Outcome::Alive,
            b"false" => Outcome::Gone("HTTP 404/410 (gone)".to_string()),
            other => {
                Outcome::Unreachable(format!("unexpected: {}", String::from_utf8_lossy(other)))
            }
        },
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

/// Partition into (gone, unreachable-confirmed, unreachable-pending), each sorted by URL.
fn buckets(
    cache: &HashMap<String, Status>,
    now: u64,
) -> (Vec<&Status>, Vec<&Status>, Vec<&Status>) {
    let (mut gone, mut confirmed, mut pending) = (Vec::new(), Vec::new(), Vec::new());
    for s in cache.values() {
        match s.status.as_str() {
            "gone" => gone.push(s),
            "unreachable" if removable(s, now) => confirmed.push(s),
            "unreachable" => pending.push(s),
            _ => {}
        }
    }
    for v in [&mut gone, &mut confirmed, &mut pending] {
        v.sort_by(|a, b| a.url.cmp(&b.url));
    }
    (gone, confirmed, pending)
}

/// The human-readable review as an org file, grouped by removability.
fn report(gone: &[&Status], confirmed: &[&Status], pending: &[&Status], now: u64) -> String {
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
    section(
        "Gone — removable",
        gone,
        "404/410 or DNS: definitively dead",
    );
    section(
        "Unreachable — confirmed, removable",
        confirmed,
        "timeout/refused, sustained across runs",
    );
    section(
        "Unreachable — pending recheck",
        pending,
        "down at least once; re-run the check to confirm before removing",
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
    if meta.running {
        return format!(
            "<span class=\"cms-linkcheck running\">checking links… {} / {}</span>",
            group(meta.checked),
            group(meta.total)
        );
    }
    if cache.is_empty() {
        return String::new();
    }
    let (gone, confirmed, pending) = buckets(cache, now);
    let unreachable = confirmed.len() + pending.len();
    let last = cache.values().map(|s| s.checked_at).max().unwrap_or(0);
    format!(
        "<span class=\"cms-linkcheck\">links: {} gone · {} unreachable · checked {}</span>",
        group(gone.len()),
        group(unreachable),
        ago(now, last),
    )
}

/// `urn:cms:review` — the suggested-deletes review as an htmx card fragment. Reads the persisted
/// status, takes the removal candidates (`gone` + confirmed-`unreachable`), and renders them
/// through the `review` stylesheet (a view is a query; here the "query" is the removable set). The
/// pending count rides along so the header can note "N pending confirmation". Read-only — the
/// authorized purge is a separate action.
pub struct ReviewView;

#[async_trait]
impl Endpoint for ReviewView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let cache = load_status(&resolved_status_path());
        let now = unix_now();
        let (gone, confirmed, pending) = buckets(&cache, now);
        let mut removable: Vec<&Status> = gone.into_iter().chain(confirmed).collect();
        removable.sort_by(|a, b| a.url.cmp(&b.url));
        let xml = review_xml(&removable, pending.len(), now);
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
        Ok(Representation::new(
            ReprType::new("text/html").with_param("charset", "utf-8"),
            out.bytes,
        )
        .cacheable())
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

/// Build the review doc (`urn:cms:review#`) from the removable candidates + the pending count.
/// An empty candidate set is its own element (the stylesheet needs no conditionals).
fn review_xml(removable: &[&Status], pending: usize, now: u64) -> String {
    let mut s = format!(
        "<review xmlns=\"urn:cms:review#\" removable=\"{}\" pending=\"{pending}\">",
        removable.len()
    );
    if removable.is_empty() {
        s.push_str("<empty/>");
    } else {
        for it in removable {
            let days = now.saturating_sub(it.first_broken_at) / 86_400;
            s.push_str("<item status=\"");
            xml_attr(&mut s, &it.status);
            s.push_str("\" reason=\"");
            xml_attr(&mut s, &it.reason);
            s.push_str(&format!("\" days=\"{days}\" url=\""));
            xml_attr(&mut s, &it.url);
            s.push_str("\" title=\"");
            xml_attr(&mut s, &it.title);
            s.push_str("\"/>");
        }
    }
    s.push_str("</review>");
    s
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

// ---- the authorized purge ----------------------------------------------------------------------

/// `urn:cms:purge` — the reviewed removal. Resolving it with `Source` returns the confirm prompt
/// (safe, idempotent); with `Sink` it *executes*: back up the bookmarks file, strike the confirmed
/// removal candidates (`gone` + confirmed-`unreachable`) by URL, and write it back **through the
/// kernel** — which cuts the bookmarks golden thread, so the derived graph re-derives and the room
/// refreshes live. The HTTP face only reaches the `Sink` on an authenticated `POST /purge`.
pub struct PurgeView {
    /// The `urn:cms:src:{subpath}` IRI of the bookmarks file (what `BookmarkGraph` reads).
    pub bookmarks_iri: String,
    /// The `urn:cms:src:{subpath}.bak` IRI the old content is backed up to.
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
        "cms-purge"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:purge")
            .summary(
                "Purge the confirmed-dead bookmarks from the source file (Sink executes, Source \
                 returns the confirm prompt). Backs the file up first, then writes through the \
                 kernel so the graph re-derives.",
            )
            .verb(Verb::Sink)
            // It writes the bookmarks file (and its backup) through the fs resource.
            .requires("urn:cap:fs:write:*")
    }
}

impl PurgeView {
    /// The confirm prompt (Source): the candidate count + a Confirm/Cancel pair.
    fn confirm_html(&self) -> String {
        let cache = load_status(&resolved_status_path());
        let now = unix_now();
        let (gone, confirmed, _pending) = buckets(&cache, now);
        let n = gone.len() + confirmed.len();
        if n == 0 {
            return "<div class=\"cms-purge\"><p>Nothing to purge.</p>\
                <button hx-get=\"/r/urn:cms:review\">back</button></div>"
                .to_string();
        }
        format!(
            "<div class=\"cms-purge\"><p>Remove <b>{}</b> confirmed-dead links from the bookmarks \
             file? A backup is saved first — this can't be undone from the room.</p>\
             <button class=\"cms-purge-go\" hx-post=\"/purge\">Confirm purge</button> \
             <button hx-get=\"/r/urn:cms:review\">Cancel</button></div>",
            group(n)
        )
    }

    /// Execute (Sink): back up, strike, write through the kernel (→ live refresh).
    async fn execute(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let cache = load_status(&resolved_status_path());
        let now = unix_now();
        let (gone, confirmed, _pending) = buckets(&cache, now);
        let removable: HashSet<&str> = gone
            .iter()
            .chain(&confirmed)
            .map(|s| s.url.as_str())
            .collect();
        if removable.is_empty() {
            return Ok(fragment(
                "<div class=\"cms-purge\"><p>Nothing to purge.</p></div>".to_string(),
            ));
        }
        let iri = Iri::parse(&self.bookmarks_iri)
            .map_err(|e| Error::Endpoint(format!("bad bookmarks iri: {e}")))?;
        let bak = Iri::parse(&self.bak_iri)
            .map_err(|e| Error::Endpoint(format!("bad backup iri: {e}")))?;
        // Read the current file through the kernel.
        let current = inv.source(&iri).await?;
        let text = String::from_utf8_lossy(&current.bytes).into_owned();
        let (new_text, removed) = strike(&text, &removable);
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

// ---- kernel assembly ---------------------------------------------------------------------------

/// A maintenance kernel: the CMS graph spaces + the `ikigai-http` outbound endpoints over
/// `transport` + the `urn:cms:linkcheck` pass (persisting to `status_path`), with a system clock
/// so cacheable reads honor their deadlines. `transport` is injectable so a test can supply a
/// canned one.
pub fn maintenance_kernel(
    src_dir: PathBuf,
    bookmarks: Option<String>,
    status_path: PathBuf,
    transport: std::sync::Arc<dyn HttpTransport>,
) -> Kernel {
    let mut spaces = crate::cms_spaces_with(src_dir, None, None, bookmarks);
    spaces.push(std::sync::Arc::new(ikigai_http::space(transport)) as std::sync::Arc<dyn Space>);
    spaces.push(std::sync::Arc::new(EndpointSpace::new().bind(
        Exact::new("urn:cms:linkcheck"),
        LinkCheckPass { status_path },
    )) as std::sync::Arc<dyn Space>);
    Kernel::new(std::sync::Arc::new(Fallback::new(spaces)))
        .with_clock(std::sync::Arc::new(SystemClock))
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

/// [`maintenance_kernel`] over the real reqwest transport. Must be built inside a tokio runtime.
pub fn build_maintenance_kernel(
    src_dir: PathBuf,
    bookmarks: Option<String>,
    status_path: PathBuf,
) -> Kernel {
    maintenance_kernel(
        src_dir,
        bookmarks,
        status_path,
        std::sync::Arc::new(ReqwestTransport::new()),
    )
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
        let src = dir.keep();
        let mut spaces = crate::cms_spaces_with(src, None, None, None);
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

        // The review view resolves through the review stylesheet (xrust) — this exercises the
        // whole pipeline (endpoint → build XML → urn:xslt:transform → urn:cms:style:review).
        let rreq = Request::new(Verb::Source, Iri::parse("urn:cms:review").unwrap());
        let rrepr = futures::executor::block_on(kernel.issue(rreq, &Capability::root()))
            .expect("review resolves");
        let html = String::from_utf8_lossy(&rrepr.bytes);
        assert!(
            html.contains("cms-review"),
            "review renders its section: {html}"
        );

        // The purge confirm prompt (Source — safe, no mutation) resolves and renders.
        let preq = Request::new(Verb::Source, Iri::parse("urn:cms:purge").unwrap());
        let prepr = futures::executor::block_on(kernel.issue(preq, &Capability::root()))
            .expect("purge confirm resolves");
        assert!(
            String::from_utf8_lossy(&prepr.bytes).contains("cms-purge"),
            "purge confirm renders"
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
    fn removable_needs_two_runs_over_a_day_for_unreachable_but_gone_always() {
        let now = 3_000_000u64;
        assert!(removable(&st("gone", now, 1), now));
        assert!(!removable(&st("unreachable", now, 1), now));
        assert!(!removable(&st("unreachable", now - 3_600, 2), now));
        assert!(removable(&st("unreachable", now - 86_400, 2), now));
        assert!(!removable(&st("ok", 0, 0), now));
    }

    #[test]
    fn the_status_fragment_shows_progress_running_tally_idle_and_nothing_before_a_run() {
        let now = 3_000_000u64;
        // Running → progress, thousands-grouped, with the `running` class.
        let running = status_fragment(
            &Meta {
                running: true,
                checked: 1_240,
                total: 5_373,
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
    fn review_xml_lists_candidates_with_escaped_attrs_or_an_empty_marker() {
        let now = 3_000_000u64;
        let mut g = st("gone", now - 172_800, 1); // 2 days
        g.url = "https://x/?a=1&b=2".into();
        g.title = "A \"quoted\" <title>".into();
        g.reason = "HTTP 404/410 (gone)".into();
        let xml = review_xml(&[&g], 638, now);
        assert!(xml.contains("removable=\"1\" pending=\"638\""), "{xml}");
        assert!(xml.contains("status=\"gone\""), "{xml}");
        assert!(xml.contains("days=\"2\""), "{xml}");
        // Attribute values are XML-escaped.
        assert!(xml.contains("a=1&amp;b=2"), "{xml}");
        assert!(xml.contains("&quot;quoted&quot; &lt;title&gt;"), "{xml}");
        // No candidates → the empty marker.
        assert!(review_xml(&[], 0, now).contains("<empty/>"));
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
}
