//! The page face's own hardening, each case reproduced on d740417 by the 2026-10-05 audits:
//!
//! - **Reflected markup.** `/r/` put a resolution error into HTML unescaped and labeled every
//!   body `text/html`, so `GET /r/urn:xslt:transform?stylesheet=…&content=<img onerror=…>` —
//!   no session needed — answered 200 `text/html` carrying live script, which could then POST
//!   the purge routes same-origin.
//! - **Any Host, any Origin.** The page face answered whatever `Host` a request named and
//!   whatever `Origin` it came from: with `dev_open`, a DNS-rebound page read the room and a
//!   cross-origin form POST ran a removal as root.
//! - **A decoder that panics.** `percent_decode` sliced a `&str` by byte offset after a `%`, so
//!   `%` before a multi-byte character killed the connection's task before any check ran.
//! - **The wire's verb.** A never-signed-in WebTransport session issued `Source` on a Sink and
//!   the write ran (closed by `serves` in every endpoint; pinned here at the wire's door).

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use super::{handle, percent_decode, resolve_http, serve_http, HttpAuth, PageOrigin, Session};
use ikigai_cms_web::session::{RecentLog, Rp};
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_wire::{encode, Call};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PAYLOAD: &str = "<img src=x onerror=alert(1)>";
const PAYLOAD_ENC: &str = "%3Cimg%20src%3Dx%20onerror%3Dalert(1)%3E";

/// A one-bookmark room over `dir`, its overlays and status cache in the tempdir.
fn room(dir: &Path) -> (Kernel, Vec<String>) {
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(
        &bm,
        "* Bookmarks\n** [[https://sci.example][Science Bookmark]]\n   \
         :PROPERTIES:\n   :TAGS: science\n   :END:\n",
    )
    .unwrap();
    let kernel = ikigai_cms_web::build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        None,
        None,
        ikigai_cms_web::tagstore::TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    );
    (kernel, vec![format!("urn:cap:fs:read:{}", dir.display())])
}

fn rp(dir: &Path) -> Rp {
    Rp::new(
        "localhost",
        "http://localhost:8080",
        Arc::new(ikigai_secret::FileBackend::new(dir.join("keys"))),
    )
    .expect("rp builds")
}

/// Start the page server on a free loopback port, answering for `origin`.
async fn start(dir: &Path, origin: &str, dev_open: bool) -> u16 {
    let (kernel, ent) = room(dir);
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    tokio::spawn(serve_http(
        dir.to_path_buf(),
        SocketAddr::from(([127, 0, 0, 1], port)),
        Arc::new(kernel),
        Arc::new(rp(dir)),
        Arc::new(HttpAuth::default()),
        Arc::new(RecentLog::default()),
        Arc::new(ent),
        Arc::new(PageOrigin::parse(origin).unwrap()),
        dev_open,
    ));
    port
}

/// Send `head` (request line + headers, no blank line) and read the whole response.
async fn send(port: u16, head: &str) -> String {
    let mut conn = None;
    for _ in 0..50 {
        if let Ok(c) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            conn = Some(c);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut c = conn.expect("page server listening");
    c.write_all(format!("{head}\r\nContent-Length: 0\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut buf = Vec::new();
    let _ = c.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

#[test]
fn a_resolution_error_never_reflects_markup() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _) = room(dir.path());
    let public = Capability::scoped(Vec::<String>::new());
    for target in [
        format!("urn:xslt:transform?stylesheet=urn:cms:style:review&content={PAYLOAD_ENC}"),
        format!("urn:sparql:select?graph=urn:cms:graph:tags-approved&query={PAYLOAD_ENC}"),
        format!(
            "urn:sparql:select?graph={PAYLOAD_ENC}\
             &query=SELECT%20*%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D"
        ),
    ] {
        let (_s, ctype, body) =
            resolve_http(&kernel, &RecentLog::default(), &public, None, &target);
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains(PAYLOAD),
            "{target} reflected raw markup as {ctype}: {body}"
        );
        assert!(body.contains("cms-error"), "{target}: {body}");
    }
}

#[test]
fn a_body_is_served_as_its_own_media_type() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, ent) = room(dir.path());
    let full = Capability::scoped(ent);
    let recent = RecentLog::default();
    let (_s, graph, _) = resolve_http(&kernel, &recent, &full, None, "urn:cms:graph:bookmarks");
    assert!(graph.starts_with("text/turtle"), "{graph}");
    let query = "urn:sparql:select?graph=urn:cms:graph:bookmarks\
                 &query=SELECT%20*%20WHERE%20%7B%3Fs%20%3Fp%20%3Fo%7D";
    let (_s, results, _) = resolve_http(&kernel, &recent, &full, None, query);
    assert!(!results.starts_with("text/html"), "{results}");
    // The views are HTML by their own declaration, so htmx sees exactly what it saw before.
    let (_s, view, body) = resolve_http(
        &kernel,
        &recent,
        &full,
        None,
        "urn:cms:view:science?style=catalog",
    );
    assert!(view.starts_with("text/html"), "{view}");
    assert!(String::from_utf8_lossy(&body).contains("Science Bookmark"));
}

#[tokio::test]
async fn every_response_carries_nosniff() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>room</h1>").unwrap();
    let port = start(dir.path(), "http://localhost:8080", false).await;
    for target in ["/", "/r/urn:cms:graph:bookmarks", "/nope.html"] {
        let resp = send(
            port,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost:8080"),
        )
        .await;
        assert!(
            resp.contains("X-Content-Type-Options: nosniff"),
            "{target}: {resp}"
        );
    }
}

/// U2's probe, kept whole: with `dev_open` on, a rebound Host must not read the room and a
/// foreign Origin must not run a removal.
#[tokio::test]
async fn dev_open_answers_only_its_own_origin() {
    let dir = tempfile::tempdir().unwrap();
    let port = start(dir.path(), "http://localhost:8080", true).await;
    let read = send(
        port,
        "GET /r/urn:cms:view:science?style=catalog HTTP/1.1\r\nHost: attacker.example\r\n\
         Origin: http://attacker.example",
    )
    .await;
    assert!(read.starts_with("HTTP/1.1 403"), "{read}");
    assert!(!read.contains("Science Bookmark"), "{read}");
    let write = send(
        port,
        "POST /link/remove?url=https%3A%2F%2Fsci.example HTTP/1.1\r\nHost: localhost:8080\r\n\
         Origin: https://attacker.example",
    )
    .await;
    assert!(write.starts_with("HTTP/1.1 403"), "{write}");
    let file = std::fs::read_to_string(dir.path().join("old-org/pinboard-bookmarks.org")).unwrap();
    assert!(
        file.contains("sci.example"),
        "a foreign Origin removed the bookmark"
    );
    // The room's own origin is still served, and a loopback alias on another port (an ssh
    // tunnel's remap) is the same machine, not a rebinding.
    for host in ["localhost:8080", "127.0.0.1:9090", "[::1]:8080"] {
        let ok = send(
            port,
            &format!("GET /r/urn:cms:view:science?style=catalog HTTP/1.1\r\nHost: {host}"),
        )
        .await;
        assert!(ok.contains("Science Bookmark"), "{host}: {ok}");
    }
    // A same-origin POST (Origin = the Host it names) is admitted.
    let own = send(
        port,
        "POST /link/keep?url=https%3A%2F%2Fsci.example HTTP/1.1\r\nHost: localhost:8080\r\n\
         Origin: http://localhost:8080",
    )
    .await;
    assert!(own.contains("kept"), "{own}");
}

#[test]
fn the_page_origin_admits_its_own_host_and_nothing_else() {
    let local = PageOrigin::parse("http://localhost:8090").unwrap();
    assert!(local.admits(Some("localhost:8090"), None));
    assert!(local.admits(Some("LOCALHOST:8090"), None));
    assert!(
        local.admits(Some("127.0.0.1:8091"), None),
        "loopback alias, tunnel port"
    );
    assert!(local.admits(Some("[::1]:8090"), None));
    assert!(local.admits(Some("localhost:8090"), Some("http://localhost:8090")));
    assert!(!local.admits(None, None), "no Host");
    assert!(
        !local.admits(Some("evil.example:8090"), None),
        "rebound name"
    );
    assert!(!local.admits(Some("localhost:8090@evil"), None));
    assert!(
        !local.admits(Some("localhost:8090"), Some("http://localhost:3000")),
        "another local port is another origin, though SameSite calls it the same site"
    );
    assert!(!local.admits(Some("localhost:8090"), Some("null")));

    let proxied = PageOrigin::parse("https://room.example.com").unwrap();
    assert!(proxied.admits(Some("room.example.com"), None));
    assert!(proxied.admits(Some("room.example.com:443"), None));
    assert!(proxied.admits(Some("room.example.com"), Some("https://room.example.com")));
    assert!(
        !proxied.admits(Some("localhost"), None),
        "not a loopback origin"
    );
    assert!(!proxied.admits(Some("room.example.com:8443"), None));
    assert!(!proxied.admits(Some("room.example.com"), Some("https://evil.example.com")));
}

#[test]
fn percent_decode_survives_a_stray_percent_before_a_multibyte_char() {
    assert_eq!(percent_decode("urn:cms:view:%€"), "urn:cms:view:%€");
    assert_eq!(percent_decode("%a\u{e9}t\u{e9}"), "%a\u{e9}t\u{e9}");
    assert_eq!(percent_decode("a%20b%2"), "a b%2");
    assert_eq!(percent_decode("%C3%A9"), "\u{e9}");
}

#[tokio::test]
async fn a_raw_request_with_percent_and_multibyte_gets_an_answer() {
    let dir = tempfile::tempdir().unwrap();
    let port = start(dir.path(), "http://localhost:8080", false).await;
    let resp = send(
        port,
        "GET /r/urn:cms:view:%€ HTTP/1.1\r\nHost: localhost:8080",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1"), "no response at all: {resp:?}");
}

/// U's wire case: a never-signed-in session issues `Source` on a Sink. The endpoint refuses the
/// undeclared verb, so the dismissed overlay is never written.
#[test]
fn a_public_wire_session_cannot_dismiss_a_tag() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, ent) = room(dir.path());
    let mut session = Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
        principal: None,
    };
    for verb in [Verb::Source, Verb::Exists, Verb::Delete] {
        let req = Request::new(verb, Iri::parse("urn:cms:tag-reject").unwrap())
            .with_arg("book", ArgRef::Inline(b"urn:cms:bookmark:victim".to_vec()))
            .with_arg("tag", ArgRef::Inline(b"planted".to_vec()));
        let _ = handle(
            &kernel,
            &rp(dir.path()),
            &HttpAuth::default(),
            &ent,
            &RecentLog::default(),
            &mut session,
            &encode(&Call::Issue(req)).unwrap(),
        );
    }
    let dismissed =
        std::fs::read_to_string(dir.path().join("cms-tag-dismissed.ttl")).unwrap_or_default();
    assert!(!dismissed.contains("planted"), "{dismissed}");
}
