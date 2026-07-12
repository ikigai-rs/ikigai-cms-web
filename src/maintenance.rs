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
    ArgRef, Description, Endpoint, EndpointSpace, Exact, Fallback, Invocation, Iri, Kernel,
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

        for (idx, outcome) in check_all(inv, &to_check).await {
            let (subject, url, title) = to_check[idx];
            let entry = merge(cache.get(url), subject, url, title, &outcome, now);
            cache.insert(url.clone(), entry);
        }
        // Drop status for bookmarks that no longer exist.
        let current: HashSet<&str> = bookmarks.iter().map(|(_, u, _)| u.as_str()).collect();
        cache.retain(|url, _| current.contains(url.as_str()));

        save_status(&self.status_path, &cache);
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

/// Check `items` as parked futures bounded at [`CONCURRENCY`] in flight.
async fn check_all(
    inv: &Invocation<'_>,
    items: &[&(String, String, String)],
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
        .map(|(i, url)| check_one(inv, &done, total, i, url))
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await
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
}
