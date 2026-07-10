//! `cms-linkcheck` — a one-shot pass that HEAD-checks every bookmark URL and writes
//! link-status annotations (Turtle) for the broken ones. It annotates — it never touches
//! your org sources.
//!
//! Each URL is resolved through `urn:cms:linkcheck` (see [`ikigai_cms_web::maintenance`]),
//! which caches a result for a week — so inside a long-lived host a re-run only re-checks
//! what has gone stale. In this one-shot bin each URL is checked once. The persistent
//! timer-driven daemon is the next slice; this is the manual runner.
//!
//! Run: `cargo run --features maintenance --bin cms-linkcheck -- <src_dir> [out.ttl] [limit] [pace_ms]`

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ikigai_cms_web::maintenance::build_maintenance_kernel;
use ikigai_core::{ArgRef, Iri, Kernel, Request, Verb};
use ikigai_resolve::Resolver;

fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir = args
        .next()
        .expect("usage: cms-linkcheck <src_dir> [out.ttl] [limit] [pace_ms]");
    let out = args
        .next()
        .unwrap_or_else(|| "cms-linkstatus.ttl".to_string());
    let limit: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let pace = Duration::from_millis(args.next().and_then(|s| s.parse().ok()).unwrap_or(200));

    let kernel = build_maintenance_kernel(src_dir.into());
    let bookmarks = list_bookmarks(&kernel);
    let total = bookmarks.len().min(limit);
    eprintln!(
        "checking {total} bookmark URLs (pace {}ms)…",
        pace.as_millis()
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut broken: Vec<(String, String, String)> = Vec::new();
    for (i, (subject, url)) in bookmarks.iter().take(limit).enumerate() {
        if let Some(reason) = check(&kernel, url) {
            eprintln!("[{}/{total}] BROKEN {url}  ({reason})", i + 1);
            broken.push((subject.clone(), url.clone(), reason));
        }
        if i + 1 < total {
            std::thread::sleep(pace);
        }
    }

    std::fs::write(&out, annotations_turtle(&broken, now)).expect("write annotations");
    eprintln!(
        "\nchecked {total}, {} broken → {out}\n(union this into the graph to see a broken-links view)",
        broken.len()
    );
}

/// `(subject IRI, url)` for every bookmark carrying an http(s) `dc:identifier`.
fn list_bookmarks(kernel: &Kernel) -> Vec<(String, String)> {
    let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         SELECT ?s ?u WHERE { ?s dc:identifier ?u . FILTER(STRSTARTS(STR(?u), \"http\")) }";
    let request = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
        .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let (repr, _) = Resolver::issue(kernel, request).expect("list bookmarks");
    let json: serde_json::Value = serde_json::from_slice(&repr.bytes).expect("results json");
    json["results"]["bindings"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let s = r["s"]["value"].as_str()?;
                    let u = r["u"]["value"].as_str()?;
                    Some((s.to_string(), u.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `None` if reachable, `Some(reason)` if broken — via the week-cached `urn:cms:linkcheck`.
fn check(kernel: &Kernel, url: &str) -> Option<String> {
    let request = Request::new(Verb::Source, Iri::parse("urn:cms:linkcheck").unwrap())
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()));
    let (repr, _) = Resolver::issue(kernel, request).ok()?;
    let body = String::from_utf8_lossy(&repr.bytes);
    body.strip_prefix("broken\t").map(|r| r.to_string())
}

/// The broken links as a Turtle annotation graph (provenanced with the check time).
fn annotations_turtle(broken: &[(String, String, String)], checked_at: u64) -> String {
    let mut ttl = String::from("@prefix cms: <https://ikigai-rs.dev/ns/cms#> .\n");
    for (subject, url, reason) in broken {
        ttl.push_str(&format!(
            "<{subject}> cms:linkStatus \"broken\" ; cms:linkReason \"{}\" ; \
             cms:brokenUrl \"{}\" ; cms:checkedAt {checked_at} .\n",
            ttl_escape(reason),
            ttl_escape(url),
        ));
    }
    ttl
}

/// Escape a value for a Turtle double-quoted string literal.
fn ttl_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotations_are_provenanced_turtle_with_escaping() {
        let broken = vec![(
            "urn:cms:bookmark:abc".to_string(),
            "http://x/?q=\"z".to_string(),
            "HTTP 410 (gone)".to_string(),
        )];
        let ttl = annotations_turtle(&broken, 42);
        assert!(ttl.contains("<urn:cms:bookmark:abc> cms:linkStatus \"broken\""));
        assert!(ttl.contains("cms:linkReason \"HTTP 410 (gone)\""));
        assert!(ttl.contains("cms:checkedAt 42"));
        assert!(
            ttl.contains("q=\\\"z"),
            "quote in the URL is escaped: {ttl}"
        );
    }
}
