//! The page face sends ikigai-web's page preset, `SecurityHeaders::html_door()` (FND-H, ledger
//! #2155), except where the room's own policy has to differ, and pins how it differs.
//!
//! - **The companion headers.** On 97f40c1 every response carried `nosniff` and the room's CSP,
//!   but no `Referrer-Policy` (a card opened from the room told the bookmarked site which room
//!   URL it came from) and no `Cross-Origin-Opener-Policy` (a window the room opened, or that
//!   opened the room, kept a handle on it across origins). The preset sends both.
//! - **The CSP.** The preset's `HTML_DOOR_CSP` as written refuses three things the room is made
//!   of, as a browser pass recorded: the wire codec's WebAssembly compile (`script-src`), the
//!   WebTransport dial to the wire's port (`connect-src`, from `default-src 'self'`), and every
//!   view's own `<style>` block (`style-src`). So the room keeps its own policy (`src/csp.rs`):
//!   the preset's directives, each exactly as the preset writes it, plus those three allowances
//!   and nothing else.

use std::collections::BTreeMap;

use ikigai_web::{SecurityHeaders, HTML_DOOR_CSP};

/// The `Name: value` header lines of a raw response, in order.
fn header_lines(resp: &str) -> Vec<&str> {
    let head = resp.split("\r\n\r\n").next().unwrap_or("");
    head.split("\r\n").skip(1).collect()
}

/// Every value `resp` sends for header `name` (matched case-insensitively).
fn values<'a>(resp: &'a str, name: &str) -> Vec<&'a str> {
    header_lines(resp)
        .into_iter()
        .filter_map(|l| l.split_once(": "))
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
        .collect()
}

/// A policy as directive → its sources, in order.
fn directives(csp: &str) -> BTreeMap<&str, Vec<&str>> {
    csp.split(';')
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| {
            let mut words = d.split_whitespace();
            (words.next().unwrap(), words.collect())
        })
        .collect()
}

#[tokio::test]
async fn every_response_carries_the_preset_companion_headers() {
    let preset = SecurityHeaders::html_door();
    let want = [
        (
            "X-Content-Type-Options",
            preset.nosniff.then_some("nosniff"),
        ),
        ("Referrer-Policy", preset.referrer_policy.as_deref()),
        ("Cross-Origin-Opener-Policy", preset.coop.as_deref()),
    ];
    // What the preset names today; a change to it upstream is a change to review here.
    assert_eq!(
        want.map(|(_, v)| v),
        [Some("nosniff"), Some("no-referrer"), Some("same-origin")]
    );

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>room</h1>").unwrap();
    std::fs::write(dir.path().join("room.css"), "body{}").unwrap();
    let port = super::http_face::start(dir.path(), "http://localhost:8080", true).await;
    // The page, a stylesheet, a view, the tag index, an error fragment, the passkey status, a
    // 404, and the 403 a foreign Host gets before anything is routed.
    for (target, host, status) in [
        ("/", "localhost:8080", "200"),
        ("/index.html", "localhost:8080", "200"),
        ("/room.css", "localhost:8080", "200"),
        (
            "/r/urn:cms:view:science?style=catalog",
            "localhost:8080",
            "200",
        ),
        ("/r/urn:cms:tags", "localhost:8080", "200"),
        ("/r/urn:cms:nope", "localhost:8080", "200"),
        ("/auth/status", "localhost:8080", "200"),
        ("/nope.html", "localhost:8080", "404"),
        ("/", "evil.example", "403"),
    ] {
        let resp =
            super::http_face::send(port, &format!("GET {target} HTTP/1.1\r\nHost: {host}")).await;
        assert!(
            resp.starts_with(&format!("HTTP/1.1 {status}")),
            "{target} (Host {host}): {resp}"
        );
        for (name, value) in want {
            let value = value.unwrap();
            assert_eq!(
                values(&resp, name),
                [value],
                "{target} (Host {host}): {name} once, as the preset names it: {resp}"
            );
        }
        assert_eq!(
            values(&resp, "Content-Security-Policy"),
            [ikigai_cms_web::csp::policy(4433)],
            "{target} (Host {host}): the room's own policy, once: {resp}"
        );
        // The preset sends HSTS only on a request that arrived over HTTPS; this face never
        // terminates TLS (a TLS terminator in front of it is where HSTS belongs).
        assert!(
            values(&resp, "Strict-Transport-Security").is_empty(),
            "{target}: {resp}"
        );
    }
}

#[test]
fn the_room_policy_is_the_preset_plus_three_named_allowances() {
    let room = ikigai_cms_web::csp::policy(4433);
    let (room, preset) = (directives(&room), directives(HTML_DOOR_CSP));

    // Every directive the preset writes, the room writes too.
    for name in preset.keys() {
        assert!(room.contains_key(name), "the room drops {name}: {room:?}");
    }

    // The view stylesheets' `<style>` blocks, by hash: what the room's style-src may add.
    let hashes: Vec<String> = ikigai_cms_web::style_blocks()
        .into_iter()
        .map(ikigai_cms_web::csp::style_hash)
        .collect();
    assert!(!hashes.is_empty(), "the views carry their own CSS");

    for (name, sources) in &room {
        // A directive the preset leaves to `default-src` starts from `default-src`'s sources.
        let base = preset.get(name).unwrap_or(&preset["default-src"]);
        for s in base {
            assert!(
                sources.contains(s),
                "{name} drops the preset's {s}: {sources:?}"
            );
        }
        let extra: Vec<&str> = sources
            .iter()
            .copied()
            .filter(|s| !base.contains(s))
            .collect();
        match *name {
            // The wire codec is WebAssembly; this allows compiling it, not `eval`.
            "script-src" => assert_eq!(extra, ["'wasm-unsafe-eval'"], "{name}"),
            // Each view's own `<style>` block, and nothing else: no `'unsafe-inline'`, no
            // host, no scheme.
            "style-src" => {
                assert!(!extra.is_empty(), "{name}: the views' blocks");
                for s in &extra {
                    assert!(
                        hashes.iter().any(|h| h == s),
                        "{name} adds {s}, which hashes no view's <style> block"
                    );
                }
                for h in &hashes {
                    assert!(extra.contains(&h.as_str()), "{name} misses {h}");
                }
            }
            // The WebTransport wire: its own port, on whatever host the page was loaded from.
            "connect-src" => assert_eq!(extra, ["https://*:4433"], "{name}"),
            _ => assert!(
                extra.is_empty(),
                "{name} loosens the preset by {extra:?}: {sources:?}"
            ),
        }
    }

    // And no directive the preset does not write, other than the wire's connect-src.
    let added: Vec<&&str> = room.keys().filter(|n| !preset.contains_key(*n)).collect();
    assert_eq!(added, [&"connect-src"], "{room:?}");
}
