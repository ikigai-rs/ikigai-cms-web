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
//!
//! And one that is not an audit's: **the anonymous door's time budget** (ledger #964). A
//! signed-out `urn:sparql:*` request is stamped with `anonymous_sparql_budget_ms` on both faces;
//! reproduced on 1cb5e7c, where a 1.3 KB cross product answered 272 MB after 11 s.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{handle, percent_decode, resolve_http, serve_http, HttpAuth, PageOrigin, Session};
use ikigai_cms_web::config::DEFAULT_ANONYMOUS_SPARQL_BUDGET;
use ikigai_cms_web::session::{RecentLog, Rp};
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_wire::{decode, encode, Call, IssueWith, Reply, TraceContext};
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

/// Start the page server on a free loopback port, answering for `origin`, at the default
/// anonymous SPARQL budget.
pub(super) async fn start(dir: &Path, origin: &str, dev_open: bool) -> u16 {
    start_budgeted(dir, origin, dev_open, DEFAULT_ANONYMOUS_SPARQL_BUDGET).await
}

/// [`start`] with the anonymous SPARQL budget `budget` (`anonymous_sparql_budget_ms`).
async fn start_budgeted(dir: &Path, origin: &str, dev_open: bool, budget: Duration) -> u16 {
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
        Arc::from(ikigai_cms_web::csp::headers(4433)),
        dev_open,
        budget,
    ));
    port
}

/// Send `head` (request line + headers, no blank line) and read the whole response.
pub(super) async fn send(port: u16, head: &str) -> String {
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
            DEFAULT_ANONYMOUS_SPARQL_BUDGET,
        );
    }
    let dismissed =
        std::fs::read_to_string(dir.path().join("cms-tag-dismissed.ttl")).unwrap_or_default();
    assert!(!dismissed.contains("planted"), "{dismissed}");
}

/// Wire v9's `IssueWith` (ledger #1546) is served as the `IssueAs` it extends: the carried
/// capability is clamped to the session's ceiling, so a public session carrying root reads a
/// public stylesheet and is refused the gated bookmark graph, word for word as without the run
/// context; and a recording peer is refused the way `IssueTraced` is. On main before this door
/// knew the variant, the call did not decode at all.
#[test]
fn a_wire_v9_issue_with_is_clamped_like_an_issue_as() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, ent) = room(dir.path());
    let rp = rp(dir.path());
    let mut session = Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
        principal: None,
    };
    let mut call = |call: &Call| {
        let reply = handle(
            &kernel,
            &rp,
            &HttpAuth::default(),
            &ent,
            &RecentLog::default(),
            &mut session,
            &encode(call).unwrap(),
            DEFAULT_ANONYMOUS_SPARQL_BUDGET,
        );
        decode::<Reply>(&reply).expect("the door answers a Reply")
    };
    let with = IssueWith {
        trace: None,
        retries: Some(2),
        time_left_ms: Some(30_000),
    };
    let source = |iri: &str| Request::new(Verb::Source, Iri::parse(iri).unwrap());

    // A public resource resolves under the carried context exactly as it does without one.
    let style = "urn:cms:style:catalog";
    let plain = call(&Call::IssueAs(source(style), Capability::root()));
    let carried = call(&Call::IssueWith(source(style), Capability::root(), with));
    match (plain, carried) {
        (Reply::Resolved(a, _), Reply::Resolved(b, _)) => assert_eq!(a.bytes, b.bytes),
        other => panic!("the public stylesheet resolves both ways: {other:?}"),
    }

    // A gated one is refused: root carried by a public session is clamped to the empty ceiling.
    let gated = "urn:cms:graph:bookmarks";
    let plain = call(&Call::IssueAs(source(gated), Capability::root()));
    let carried = call(&Call::IssueWith(source(gated), Capability::root(), with));
    match (plain, carried) {
        (Reply::Error(a), Reply::Error(b)) => {
            assert_eq!(a, b);
            assert!(
                !a.contains("sci.example"),
                "the graph leaked into the refusal: {a}"
            );
        }
        other => panic!("the bookmark graph is gated both ways: {other:?}"),
    }

    // A recording peer gets the `IssueTraced` refusal, in the same words.
    let ctx = TraceContext {
        trace_id: 1,
        parent_span: None,
    };
    let recording = IssueWith {
        trace: Some(ctx),
        ..with
    };
    let traced = call(&Call::IssueWith(
        source(style),
        Capability::root(),
        recording,
    ));
    let legacy = call(&Call::IssueTraced(source(style), Capability::root(), ctx));
    assert_eq!(traced, legacy);
    match traced {
        Reply::Error(m) => assert!(m.contains("traced resolution"), "{m}"),
        other => panic!("a recording peer is refused: {other:?}"),
    }
}

// ---- The anonymous door's SPARQL time budget (ledger #964) ----
//
// A signed-out caller reaches `urn:sparql:*` with no graph it may read (every room graph is
// behind the fs grant, and `urn:kernel:actions` is not RDF), so the slow query carries its own
// data: three `VALUES` tables of 100 rows, cross-joined — a million solutions. `VALUES` rows
// cost nothing under ikigai-sparql's algebra bounds, so this passes them and has to be stopped by
// the clock. Measured on 1cb5e7c (sparql 0.1.11, no budget), it answered `200` with every row
// after ~11 s (debug); on 0.1.12 without the door's stamp it stopped at the space's 5 s ceiling.
//
// That query cannot test the budget any longer, because it is now stopped by whichever bound
// fires FIRST (ledger #1673): ikigai-sparql 0.2 also bounds an answer's size (16 MiB, 100,000
// rows; ledger #970), and a machine fast enough to serialize 16 MiB inside the door's second is
// refused for size. On plasma the two budget tests failed that way under a debug `cargo test`
// while CI's slower runners passed. So the queries that must meet the clock are built so that
// no other bound CAN fire: [`slow_cross_product`] has at most `SLOW_ROWS` small rows, far under
// both size bounds by construction, and makes each row expensive instead, so only time stops
// it. A machine fast enough to finish all of it inside the budget would answer `200` with
// `"bindings"`, which every test below refuses: it fails as "not slow enough", never as a
// different bound standing in for the door's.

/// `SELECT *` over three `VALUES` tables of `n` integers each: `n³` solutions, no dataset.
/// Used here only small, as the ordinary query a door must still answer.
fn cross_product(n: usize) -> String {
    let vals = (0..n)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("%20");
    format!(
        "SELECT%20*%20WHERE%20%7B%20VALUES%20%3Fa%20%7B{vals}%7D%20\
         VALUES%20%3Fb%20%7B{vals}%7D%20VALUES%20%3Fc%20%7B{vals}%7D%20%7D"
    )
}

/// How many integers each of [`slow_cross_product`]'s two `VALUES` tables holds.
const SLOW_SIDE: usize = 300;
/// The most rows [`slow_cross_product`] can answer: `SLOW_SIDE²`.
const SLOW_ROWS: u64 = (SLOW_SIDE * SLOW_SIDE) as u64;
/// An upper bound on one of its rows as SPARQL JSON: one integer binding
/// (`{"n":{"type":"literal","value":"1000006","datatype":"…XMLSchema#integer"}},` is 97 bytes).
const SLOW_ROW_BYTES: u64 = 128;

/// A cross product whose answer can never reach the space's size bounds, and whose every row
/// is slow: two `VALUES` tables of `SLOW_SIDE` integers, and for each pair a `BIND` that grows
/// ten `x`s and the two numbers ten-fold five times over with `REPLACE` (to a million `x`s)
/// and keeps only its `STRLEN`. The string depends on `?a` and `?b`, so the planner cannot fold
/// it into one constant. Measured on plasma (2026-10-15): about 20 ms a row in a debug build and
/// 1.2 ms in release, so all 90,000 rows would take about half an hour in debug and two minutes
/// in release; finishing inside the door's second would take a machine about a hundred times
/// faster than plasma's release build. Plain text; the HTTP tests percent-encode it.
fn slow_cross_product() -> String {
    let vals = (0..SLOW_SIDE)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(" ");
    let mut grown = r#"CONCAT(STR(?a), STR(?b), "xxxxxxxxxx")"#.to_string();
    for _ in 0..5 {
        grown = format!(r#"REPLACE({grown}, "x", "xxxxxxxxxx")"#);
    }
    format!(
        "SELECT ?n WHERE {{ VALUES ?a {{ {vals} }} VALUES ?b {{ {vals} }} \
         BIND(STRLEN({grown}) AS ?n) }}"
    )
}

/// `s` with every byte outside RFC 3986's unreserved set percent-encoded, for a request line.
fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// The slow query cannot be stopped by the size bounds, so a refusal of it is the clock's. Checked
// at compile time against the bounds the space applies; the JSON head and tail are a few dozen
// bytes, and one row's worth is ample room for them.
const _: () = {
    use ikigai_sparql::budget::{DEFAULT_MAX_BYTES, DEFAULT_MAX_ROWS};
    assert!(SLOW_ROWS < DEFAULT_MAX_ROWS);
    assert!((SLOW_ROWS + 1) * SLOW_ROW_BYTES < DEFAULT_MAX_BYTES);
};

/// And `SLOW_ROWS` is what the query builds: two tables of `SLOW_SIDE` members.
#[test]
fn the_slow_query_is_as_small_as_its_size_bound_says() {
    let q = slow_cross_product();
    let members = q
        .split("VALUES")
        .skip(1)
        .map(|t| {
            t[t.find('{').unwrap() + 1..t.find('}').unwrap()]
                .split_whitespace()
                .count()
        })
        .collect::<Vec<_>>();
    assert_eq!(members, vec![SLOW_SIDE, SLOW_SIDE]);
}

/// Comfortably under the space's 5 s ceiling and over the door's 1 s, so passing means the door
/// stopped it, not the space.
const WELL_UNDER_THE_CEILING: Duration = Duration::from_millis(3000);

/// GET `/r/{target}` signed out, returning the response and how long it took.
async fn timed_get(port: u16, target: &str) -> (String, Duration) {
    let start = Instant::now();
    let resp = send(
        port,
        &format!("GET /r/{target} HTTP/1.1\r\nHost: localhost:8080"),
    )
    .await;
    (resp, start.elapsed())
}

/// The reproduction: an anonymous cross product is refused at the door's 1 s budget, as a typed
/// timeout rendered the way this face renders every resolution error — `200 OK` and a `cms-error`
/// fragment (htmx swaps a 200; that mapping is the face's, not this budget's).
#[tokio::test]
async fn an_anonymous_cross_product_is_stopped_at_the_door_budget() {
    let dir = tempfile::tempdir().unwrap();
    let port = start(dir.path(), "http://localhost:8080", false).await;
    let (resp, took) = timed_get(
        port,
        &format!("urn:sparql:select?query={}", pct(&slow_cross_product())),
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
    assert!(resp.contains("cms-error"), "{resp}");
    assert!(
        resp.contains("time budget of 1000 ms"),
        "refused at the door's budget, not the space's: {resp}"
    );
    assert!(
        !resp.contains("\"bindings\""),
        "never a partial answer: {resp}"
    );
    assert!(took < WELL_UNDER_THE_CEILING, "took {took:?}");
}

/// `budget=` from an anonymous caller can only tighten the door: 600,000 ms still stops at the
/// door's budget, and a smaller one is honored.
#[tokio::test]
async fn an_anonymous_budget_cannot_loosen_the_door() {
    let dir = tempfile::tempdir().unwrap();
    let port = start(dir.path(), "http://localhost:8080", false).await;
    let query = pct(&slow_cross_product());
    let (resp, took) = timed_get(
        port,
        &format!("urn:sparql:select?budget=600000&query={query}"),
    )
    .await;
    assert!(resp.contains("time budget of 1000 ms"), "{resp}");
    assert!(took < WELL_UNDER_THE_CEILING, "took {took:?}");

    let (resp, took) =
        timed_get(port, &format!("urn:sparql:select?budget=200&query={query}")).await;
    assert!(
        resp.contains("time budget of 200 ms"),
        "a tighter budget is kept: {resp}"
    );
    assert!(took < Duration::from_millis(1000), "took {took:?}");
}

/// The budget is `anonymous_sparql_budget_ms`, not a constant: the server applies what it is
/// configured with.
#[tokio::test]
async fn the_door_applies_the_configured_budget() {
    let dir = tempfile::tempdir().unwrap();
    let port = start_budgeted(
        dir.path(),
        "http://localhost:8080",
        false,
        Duration::from_millis(300),
    )
    .await;
    let (resp, took) = timed_get(
        port,
        &format!("urn:sparql:select?query={}", pct(&slow_cross_product())),
    )
    .await;
    assert!(resp.contains("time budget of 300 ms"), "{resp}");
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    // An ordinary anonymous query still answers.
    let (ok, _) = timed_get(
        port,
        &format!("urn:sparql:select?query={}", cross_product(2)),
    )
    .await;
    assert!(ok.contains("\"bindings\""), "{ok}");
}

/// The wire's anonymous door is the same door: a never-signed-in WebTransport session is
/// stamped too, including when it sends `budget=` by reference (which ikigai-sparql would read as
/// no budget, i.e. its 5 s ceiling).
#[test]
fn an_anonymous_wire_session_is_budgeted_too() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, ent) = room(dir.path());
    let mut session = Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
        principal: None,
    };
    let query = slow_cross_product();
    for budget in [
        None,
        Some(ArgRef::Inline(b"600000".to_vec())),
        Some(ArgRef::Reference(
            Iri::parse("urn:cms:style:catalog").unwrap(),
        )),
    ] {
        let mut req = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
            .with_arg("query", ArgRef::Inline(query.clone().into_bytes()));
        if let Some(b) = budget.clone() {
            req = req.with_arg("budget", b);
        }
        let start = Instant::now();
        let reply = handle(
            &kernel,
            &rp(dir.path()),
            &HttpAuth::default(),
            &ent,
            &RecentLog::default(),
            &mut session,
            &encode(&Call::Issue(req)).unwrap(),
            Duration::from_millis(400),
        );
        let took = start.elapsed();
        match ikigai_wire::decode::<ikigai_wire::Reply>(&reply).unwrap() {
            ikigai_wire::Reply::Error(e) => {
                assert!(e.contains("time budget of 400 ms"), "{budget:?}: {e}")
            }
            other => panic!("{budget:?}: expected the timeout, got {other:?}"),
        }
        assert!(took < WELL_UNDER_THE_CEILING, "{budget:?} took {took:?}");
    }
}

/// What the stamp does to a request, case by case.
#[test]
fn the_stamp_narrows_only_sparql_and_keeps_only_a_tighter_budget() {
    let door = Duration::from_millis(1000);
    let budget_of = |req: &Request| match req.args.get("budget") {
        Some(ArgRef::Inline(b)) => Some(String::from_utf8(b.clone()).unwrap()),
        Some(other) => Some(format!("{other:?}")),
        None => None,
    };
    let sparql = |iri: &str| Request::new(Verb::Source, Iri::parse(iri).unwrap());
    let with = |iri: &str, arg: ArgRef| sparql(iri).with_arg("budget", arg);
    let stamp = |r| super::stamp_anonymous_budget(r, door);

    assert_eq!(
        budget_of(&stamp(sparql("urn:sparql:select"))).as_deref(),
        Some("1000")
    );
    assert_eq!(
        budget_of(&stamp(sparql("urn:sparql:construct"))).as_deref(),
        Some("1000")
    );
    assert_eq!(
        budget_of(&stamp(sparql("URN:SPARQL:ask"))).as_deref(),
        Some("1000")
    );
    for (sent, kept) in [
        ("250", "250"),
        (" 1000 ", " 1000 "),
        ("1001", "1000"),
        ("600000", "1000"),
        ("0", "1000"),
        ("1s", "1000"),
        ("-5", "1000"),
    ] {
        let out = stamp(with(
            "urn:sparql:select",
            ArgRef::Inline(sent.as_bytes().to_vec()),
        ));
        assert_eq!(budget_of(&out).as_deref(), Some(kept), "sent {sent:?}");
    }
    let by_ref = stamp(with(
        "urn:sparql:select",
        ArgRef::Reference(Iri::parse("urn:x").unwrap()),
    ));
    assert_eq!(budget_of(&by_ref).as_deref(), Some("1000"));
    // Anything that is not SPARQL passes through untouched.
    let view = stamp(sparql("urn:cms:view:science"));
    assert!(view.args.is_empty(), "{:?}", view.args);
    let other = stamp(with("urn:xslt:transform", ArgRef::Inline(b"9".to_vec())));
    assert_eq!(budget_of(&other).as_deref(), Some("9"));
}
