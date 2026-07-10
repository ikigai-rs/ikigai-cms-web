//! `cms-linkcheck` — HEAD-check every bookmark URL and write link-status annotations.
//!
//! Reads the bookmark URLs out of `urn:cms:graph`, resolves `urn:httpHead` for each
//! (through the kernel, so it's capability-gated and paced), and writes a small Turtle
//! file of the **broken** ones: `<bookmark> cms:linkStatus "broken" ; cms:linkReason … ;
//! cms:checkedAt …`. It annotates — it never touches your org sources. The reading room
//! unions that file in to surface a "broken links" view; removal is a separate step.
//!
//! `urn:httpHead` returns Ok for any response (even a 404), so the transport here reports
//! non-2xx/3xx (and network failures) as broken — that's the reachability signal.
//!
//! Run: `cargo run --features maintenance --bin cms-linkcheck -- <src_dir> [out.ttl] [limit] [pace_ms]`

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ikigai_core::{ArgRef, Iri, Kernel, Request, Verb};
use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
use ikigai_resolve::Resolver;

/// A link-checking transport that flags **only genuine breakage** as `Err` (so
/// `urn:httpHead`'s Exists = "the bookmark still points at something"):
/// - `Ok` — 2xx/3xx, and also 401/403/405/429/5xx: the server *answered*, so the site is
///   alive even if it blocks HEAD or is transiently erroring. Not broken.
/// - `Err` — 404/410 (the page is gone) and network failures (DNS, refused, timeout: the
///   server is unreachable). These are the removable candidates.
///
/// This conservative bias avoids false positives — many live sites 403/405/520 a bare
/// HEAD from a bot, and auto-removing those would lose good bookmarks.
struct CheckTransport;

#[async_trait::async_trait]
impl HttpTransport for CheckTransport {
    async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
        let call = ureq::request(req.method.as_str(), &req.url)
            .timeout(Duration::from_secs(12))
            .call();
        match call {
            // ureq surfaces 2xx/3xx as Ok.
            Ok(resp) => Ok(HttpResponse {
                status: resp.status(),
                headers: Vec::new(),
                body: Vec::new(),
            }),
            // The server answered with a status: gone (404/410) is broken; anything else
            // (403 bot-block, 405 no-HEAD, 429 rate-limit, 5xx transient) is still alive.
            Err(ureq::Error::Status(404, _)) => Err("HTTP 404 (gone)".to_string()),
            Err(ureq::Error::Status(410, _)) => Err("HTTP 410 (gone)".to_string()),
            Err(ureq::Error::Status(code, _)) => Ok(HttpResponse {
                status: code,
                headers: Vec::new(),
                body: Vec::new(),
            }),
            // No response at all: DNS/connection/timeout — the server is unreachable.
            Err(ureq::Error::Transport(t)) => Err(unreachable_reason(&t)),
        }
    }
}

/// A short reason for a transport (network) failure, without the URL noise ureq prepends.
fn unreachable_reason(t: &ureq::Transport) -> String {
    match t.kind() {
        ureq::ErrorKind::Dns => "DNS: host not found".to_string(),
        ureq::ErrorKind::ConnectionFailed => "connection failed / timed out".to_string(),
        ureq::ErrorKind::Io => "connection failed / timed out".to_string(),
        other => format!("unreachable ({other:?})"),
    }
}

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

    // Bookmarks only: link-checking doesn't need the Zotero books (their identifiers are
    // Open Library lookups, not the resource itself).
    let cms = ikigai_cms_web::build_cms_kernel(src_dir.into(), None);
    let bookmarks = list_bookmarks(&cms);
    let total = bookmarks.len().min(limit);
    eprintln!(
        "checking {total} bookmark URLs (pace {}ms)…",
        pace.as_millis()
    );

    let http = Kernel::new(Arc::new(ikigai_http::space(Arc::new(CheckTransport))));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut broken: Vec<(String, String, String)> = Vec::new();
    for (i, (subject, url)) in bookmarks.iter().take(limit).enumerate() {
        match check(&http, url) {
            None => {}
            Some(reason) => {
                eprintln!("[{}/{total}] BROKEN {url}  ({reason})", i + 1);
                broken.push((subject.clone(), url.clone(), reason));
            }
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

/// `None` if the URL is reachable, `Some(reason)` if broken.
fn check(http: &Kernel, url: &str) -> Option<String> {
    let request = Request::new(Verb::Exists, Iri::parse("urn:httpHead").unwrap())
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()));
    match Resolver::issue(http, request) {
        Ok(_) => None,
        // Strip the kernel/endpoint wrapper so the annotation reads clean ("HTTP 404 (gone)").
        Err(reason) => Some(
            reason
                .rsplit("http transport: ")
                .next()
                .unwrap_or(&reason)
                .to_string(),
        ),
    }
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
