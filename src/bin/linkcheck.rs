//! `cms-linkcheck` — check every bookmark URL and persist a link-status cache, so a re-run only
//! re-checks what's new or has gone stale. It reads and reports only — removal is a separate,
//! reviewed step.
//!
//! Each URL is one honest `urn:httpHead`+`Exists` resolve through the kernel (`ikigai-http`):
//! `"true"` = reachable, `"false"` = gone (404/410), a typed error = unreachable (5xx / timeout /
//! DNS / refused). The transport is async, so the checks run as **parked futures** — bounded at
//! [`CONCURRENCY`] in flight — with no thread pool; concurrency comes from the awaits, not threads.
//! Passing `max_age` makes each check cacheable for a week (load-bearing in a long-lived host).
//!
//! The persisted cache (`cms-linkstatus.json`) tracks, per URL, the last check and — while broken
//! — how long it has been broken and across how many runs, so the removal step can tell a
//! sustained-dead link from a transient blip. A human-readable `dead-links.org` is regenerated
//! each run for review.
//!
//! Run: `CMS_BOOKMARKS=bookmarks-src.org cargo run --features maintenance --bin cms-linkcheck`
//!   optional: positional `<src_dir>` and `<limit>`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};

use ikigai_cms_web::maintenance::build_maintenance_kernel;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};

/// A fresh `ok` result is trusted for a week; broken URLs are re-checked every run (so
/// confirmation of a sustained failure accrues faster). Also the `max_age` passed to each check.
const WEEK_SECS: u64 = 7 * 24 * 60 * 60;
/// An `unreachable` (vs. `gone`) link is a removal candidate only once it has stayed broken across
/// ≥2 runs spanning at least this long — so a one-off timeout never qualifies.
const CONFIRM_SPAN_SECS: u64 = 24 * 60 * 60;
/// How many checks are in flight at once — a politeness/backpressure cap on parked futures (don't
/// open thousands of sockets at once), NOT a thread count. One runtime thread parks them all.
const CONCURRENCY: usize = 64;

/// The persisted per-URL link status — the reconcile cache across runs.
#[derive(Clone, Serialize, Deserialize)]
struct Status {
    url: String,
    subject: String,
    title: String,
    /// `ok` | `gone` | `unreachable`.
    status: String,
    #[serde(default)]
    reason: String,
    /// Unix seconds of the last check.
    checked_at: u64,
    /// Unix seconds the URL first went (and has since stayed) broken; 0 when `ok`.
    #[serde(default)]
    first_broken_at: u64,
    /// How many consecutive runs it has checked broken.
    #[serde(default)]
    broken_count: u32,
}

/// The outcome of one check: reachable, definitively gone (404/410), or unreachable (indeterminate
/// — a transient fault, or an odd status that isn't a clean presence answer).
enum Outcome {
    Alive,
    Gone(String),
    Unreachable(String),
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir: PathBuf = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CMS_SRC_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Dropbox/org-mode-files"))
                .unwrap_or_default()
        });
    let limit: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let bookmarks = std::env::var("CMS_BOOKMARKS").ok();
    let status_path =
        std::env::var("CMS_LINKSTATUS").unwrap_or_else(|_| "cms-linkstatus.json".into());
    let report_path = "dead-links.org";

    eprintln!(
        "source: {}  bookmarks: {}",
        src_dir.display(),
        bookmarks.as_deref().unwrap_or("(default)")
    );
    let kernel = build_maintenance_kernel(src_dir, bookmarks);
    let bookmarks = list_bookmarks(&kernel).await;
    let now = unix_now();
    let mut cache = load_status(&status_path);

    // Reconcile: skip a URL only if it checked `ok` within the week; re-check broken (and new)
    // URLs every run so a sustained failure is confirmed sooner.
    let fresh_ok = |url: &str| {
        matches!(cache.get(url), Some(s)
            if s.status == "ok" && now.saturating_sub(s.checked_at) < WEEK_SECS)
    };
    let fresh_ok_count = bookmarks.iter().filter(|(_, u, _)| fresh_ok(u)).count();
    let to_check: Vec<&(String, String, String)> = bookmarks
        .iter()
        .filter(|(_, url, _)| !fresh_ok(url))
        .take(limit)
        .collect();
    let limited = if to_check.len() < bookmarks.len() - fresh_ok_count {
        format!(" (limited to {limit})")
    } else {
        String::new()
    };
    eprintln!(
        "{} bookmarks · {fresh_ok_count} fresh-ok cached · checking {} (≤{CONCURRENCY} in flight){limited}…",
        bookmarks.len(),
        to_check.len(),
    );

    // Fan out as parked futures, then fold each outcome into the cache.
    for (idx, outcome) in check_all(&kernel, &to_check).await {
        let (subject, url, title) = to_check[idx];
        let entry = merge(cache.get(url), subject, url, title, &outcome, now);
        cache.insert(url.clone(), entry);
    }
    // Drop status for bookmarks that no longer exist (removed/edited away).
    let current: HashSet<&str> = bookmarks.iter().map(|(_, u, _)| u.as_str()).collect();
    cache.retain(|url, _| current.contains(url.as_str()));

    save_status(&status_path, &cache);
    let (gone, confirmed, pending) = buckets(&cache, now);
    std::fs::write(report_path, report(&gone, &confirmed, &pending, now)).expect("write report");

    eprintln!(
        "\n{} gone · {} unreachable-confirmed · {} unreachable-pending  →  {report_path}\n\
         cache: {status_path}  (re-run to confirm the pending ones; then `cms-linkclean` removes)",
        gone.len(),
        confirmed.len(),
        pending.len()
    );
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `(subject IRI, url, title)` for every bookmark carrying an http(s) `dc:identifier`,
/// de-duplicated by URL (the title falls back to the URL).
async fn list_bookmarks(kernel: &Kernel) -> Vec<(String, String, String)> {
    let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         SELECT ?s ?u ?t WHERE { ?s dc:identifier ?u . \
         FILTER(STRSTARTS(STR(?u), \"http\")) OPTIONAL { ?s dc:title ?t } }";
    let request = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let repr = kernel
        .issue(request, &Capability::root())
        .await
        .expect("list bookmarks");
    let json: serde_json::Value = serde_json::from_slice(&repr.bytes).expect("results json");
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
    out
}

/// Check `items` as parked futures, bounded at [`CONCURRENCY`] in flight. Returns `(index,
/// outcome)`; a broken outcome is logged as it lands.
async fn check_all(kernel: &Kernel, items: &[&(String, String, String)]) -> Vec<(usize, Outcome)> {
    let total = items.len();
    let done = AtomicUsize::new(0);
    futures::stream::iter(items.iter().enumerate())
        .map(|(i, item)| {
            let done = &done;
            async move {
                let outcome = check(kernel, &item.1).await;
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                match &outcome {
                    Outcome::Gone(r) => eprintln!("[{n}/{total}] GONE    {}  ({r})", item.1),
                    Outcome::Unreachable(r) => eprintln!("[{n}/{total}] unreach {}  ({r})", item.1),
                    Outcome::Alive => {}
                }
                (i, outcome)
            }
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await
}

/// One reachability check: resolve `urn:httpHead`+`Exists` (cacheable for a week). `"true"` =
/// reachable, `"false"` = gone (404/410). A typed error is split by `is_transient()` — the link
/// policy that lives in the caller: a **transient** error (5xx / timeout / DNS / refused) means we
/// couldn't reach it → *unreachable*; a **permanent** one (a 400/403/… the server *answered* with)
/// means the site is up and just doesn't serve a clean HEAD → treat as *alive*, don't flag it. So
/// only a definitive `"false"` is ever `gone`, and a HEAD-hostile-but-live server isn't condemned.
async fn check(kernel: &Kernel, url: &str) -> Outcome {
    let request = Request::new(Verb::Exists, Iri::parse("urn:httpHead").unwrap())
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "max_age",
            ArgRef::Inline(WEEK_SECS.to_string().into_bytes()),
        );
    match kernel.issue(request, &Capability::root()).await {
        Ok(repr) => match repr.bytes.as_slice() {
            b"true" => Outcome::Alive,
            b"false" => Outcome::Gone("HTTP 404/410 (gone)".to_string()),
            other => {
                Outcome::Unreachable(format!("unexpected: {}", String::from_utf8_lossy(other)))
            }
        },
        Err(e) if e.is_transient() => Outcome::Unreachable(e.to_string()),
        // The server answered with a non-presence status (400/403/…): it's alive, just not a
        // clean HEAD. Conservative — only 404/410 counts as gone.
        Err(_) => Outcome::Alive,
    }
}

/// Fold a check outcome into the prior status, carrying forward how long a still-broken URL has
/// been broken (so `unreachable` sustained-deadness accrues across runs).
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

/// Whether a broken status is a removal candidate: `gone` always; `unreachable` only once it has
/// stayed broken across ≥2 runs spanning at least [`CONFIRM_SPAN_SECS`].
fn removable(s: &Status, now: u64) -> bool {
    match s.status.as_str() {
        "gone" => true,
        "unreachable" => {
            s.broken_count >= 2 && now.saturating_sub(s.first_broken_at) >= CONFIRM_SPAN_SECS
        }
        _ => false,
    }
}

/// Partition the cache into (gone, unreachable-confirmed, unreachable-pending), each sorted by URL.
fn buckets(
    cache: &HashMap<String, Status>,
    now: u64,
) -> (Vec<&Status>, Vec<&Status>, Vec<&Status>) {
    let mut gone = Vec::new();
    let mut confirmed = Vec::new();
    let mut pending = Vec::new();
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

/// The human-readable review: an org file grouping dead bookmarks by removability. Each is a
/// clickable org link plus a one-line provenance (reason · how long dead · how many checks).
fn report(gone: &[&Status], confirmed: &[&Status], pending: &[&Status], now: u64) -> String {
    let mut s = String::from("#+TITLE: Dead bookmarks — link check\n\n");
    let section = |s: &mut String, title: &str, items: &[&Status], note: &str| {
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
        &mut s,
        "Gone — removable",
        gone,
        "404/410 or DNS: definitively dead",
    );
    section(
        &mut s,
        "Unreachable — confirmed, removable",
        confirmed,
        "timeout/refused, sustained across runs",
    );
    section(
        &mut s,
        "Unreachable — pending recheck",
        pending,
        "down at least once; re-run the check to confirm before removing",
    );
    s
}

/// Keep a title/reason on one org line and out of link-bracket trouble.
fn org_safe(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
        .replace('[', "(")
        .replace(']', ")")
}

fn load_status(path: &str) -> HashMap<String, Status> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Vec<Status>>(&bytes)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.url.clone(), s))
            .collect(),
        Err(_) => HashMap::new(),
    }
}

fn save_status(path: &str, cache: &HashMap<String, Status>) {
    let mut list: Vec<&Status> = cache.values().collect();
    list.sort_by(|a, b| a.url.cmp(&b.url));
    std::fs::write(path, serde_json::to_vec_pretty(&list).unwrap()).expect("write status");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(status: &str, first: u64, count: u32) -> Status {
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
            "sub",
            "http://x",
            "X",
            &Outcome::Unreachable("t".into()),
            now0,
        );
        assert_eq!(first.status, "unreachable");
        assert_eq!(first.broken_count, 1);
        assert_eq!(first.first_broken_at, now0);
        // Still broken a day later → count 2, first_broken preserved.
        let later = merge(
            Some(&first),
            "sub",
            "http://x",
            "X",
            &Outcome::Unreachable("t".into()),
            now0 + 86_400,
        );
        assert_eq!(later.broken_count, 2);
        assert_eq!(later.first_broken_at, now0);
        // A definitive gone is recorded as gone.
        let gone = merge(
            Some(&later),
            "sub",
            "http://x",
            "X",
            &Outcome::Gone("404".into()),
            now0 + 90_000,
        );
        assert_eq!(gone.status, "gone");
        // Recovered → back to ok, broken state cleared.
        let ok = merge(
            Some(&gone),
            "sub",
            "http://x",
            "X",
            &Outcome::Alive,
            now0 + 99_000,
        );
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.broken_count, 0);
        assert_eq!(ok.first_broken_at, 0);
    }

    #[test]
    fn removable_needs_two_runs_over_a_day_for_unreachable_but_gone_always() {
        let now = 3_000_000u64;
        assert!(
            removable(&s("gone", now, 1), now),
            "gone is always removable"
        );
        assert!(!removable(&s("unreachable", now, 1), now));
        assert!(!removable(&s("unreachable", now - 3_600, 2), now));
        assert!(removable(&s("unreachable", now - 86_400, 2), now));
        assert!(!removable(&s("ok", 0, 0), now));
    }

    #[test]
    fn the_report_groups_and_links_each_bucket() {
        let gone = s("gone", 100, 1);
        let conf = s("unreachable", 100, 3);
        let pend = s("unreachable", 100, 1);
        let out = report(&[&gone], &[&conf], &[&pend], 300_000);
        assert!(out.contains("* Gone — removable: 1"));
        assert!(out.contains("* Unreachable — confirmed, removable: 1"));
        assert!(out.contains("* Unreachable — pending recheck: 1"));
        assert!(
            out.contains("[[http://x][X]]"),
            "entries are clickable org links"
        );
    }
}
