//! The Content-Security-Policy every page-face response carries (ledger #1793).
//!
//! The room is htmx over same-origin files, so the policy can be strict: scripts only from the
//! room's own origin (htmx, the passkey script, the wire shim and its wasm codec), no inline
//! script and no `eval`, no plugins, no `<base>`, no framing, and forms that post only home.
//! Library data reaches the markup through the views, so this is the backstop behind the views'
//! own escaping and the `safe-link` rule: injected markup that slipped past both could not run.
//!
//! Two allowances, each narrow:
//! - **`'wasm-unsafe-eval'`** lets the page compile WebAssembly, which the wire shim's codec is
//!   (`dist/ikigai_cms_web_bg.wasm`). It does not allow `eval` or `new Function` in JavaScript.
//! - **The views' own `<style>` blocks, by hash.** Each view stylesheet in `styles/` carries its
//!   CSS with it (a view is self-contained), and htmx swaps it into the page as a `<style>`
//!   element. The policy lists the SHA-256 of each such block ([`crate::style_blocks`]), so those
//!   and nothing else apply; the page shell's own CSS is a file (`dist/room.css`). No
//!   `'unsafe-inline'` for anything.
//!
//! The wire (WebTransport) is a connection on its own port, so `connect-src` names that port on
//! any host: the page dials whichever host it was loaded from (`dist/wire.js`).
//!
//! Against ikigai-web's page preset, `SecurityHeaders::html_door()` (ledger #2155): this policy is
//! the preset's `HTML_DOOR_CSP`, every directive exactly as the preset writes it, plus these three
//! allowances and nothing else (`src/bin/server_tests/door.rs` pins it). The preset as written
//! refuses all three, so the room cannot send it. The preset's other headers it sends as they
//! are ([`headers`]).

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// The policy for a room whose WebTransport wire listens on `wire_port`.
///
/// ```
/// let csp = ikigai_cms_web::csp::policy(4433);
/// assert!(csp.starts_with("default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'sha256-"));
/// assert!(csp.contains("; connect-src 'self' https://*:4433;"));
/// assert!(csp.ends_with("object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
/// assert!(!csp.contains("unsafe-inline"));
/// ```
pub fn policy(wire_port: u16) -> String {
    let mut styles = String::from("'self'");
    let mut seen = Vec::new();
    for block in crate::style_blocks() {
        let hash = style_hash(block);
        if !seen.contains(&hash) {
            styles.push(' ');
            styles.push_str(&hash);
            seen.push(hash);
        }
    }
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src {styles}; \
         connect-src 'self' https://*:{wire_port}; object-src 'none'; base-uri 'none'; \
         frame-ancestors 'none'; form-action 'self'"
    )
}

/// The security header lines every page-face response carries, each ending in CRLF: the policy
/// for `wire_port` ([`policy`]), then the companion headers of ikigai-web's page preset,
/// `SecurityHeaders::html_door()`, as it names them: `X-Content-Type-Options: nosniff`,
/// `Referrer-Policy: no-referrer` (a card opened from the room does not tell the bookmarked site
/// which room URL it came from) and `Cross-Origin-Opener-Policy: same-origin`. Not its HSTS: the
/// preset sends that only on a request that arrived over HTTPS, and this face never terminates
/// TLS.
///
/// ```
/// let h = ikigai_cms_web::csp::headers(4433);
/// assert!(h.starts_with("Content-Security-Policy: default-src 'self'; "));
/// assert!(h.ends_with(
///     "\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
///      Cross-Origin-Opener-Policy: same-origin\r\n"
/// ));
/// assert!(!h.contains("Strict-Transport-Security"));
/// ```
pub fn headers(wire_port: u16) -> String {
    let preset = ikigai_web::SecurityHeaders::html_door();
    let mut out = format!("Content-Security-Policy: {}\r\n", policy(wire_port));
    if preset.nosniff {
        out.push_str("X-Content-Type-Options: nosniff\r\n");
    }
    for (name, value) in [
        ("Referrer-Policy", &preset.referrer_policy),
        ("Cross-Origin-Opener-Policy", &preset.coop),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out
}

/// The CSP hash source (`'sha256-…'`) that admits a `<style>` element whose text is `css`.
///
/// ```
/// // The value the CSP specification's own example gives for `alert('Hello, world.');`.
/// assert_eq!(
///     ikigai_cms_web::csp::style_hash("alert('Hello, world.');"),
///     "'sha256-qznLcsROx4GACP2dm0UCKCzCG+HiZ1guq6ZZDob/Tng='"
/// );
/// ```
pub fn style_hash(css: &str) -> String {
    let digest = Sha256::digest(css.as_bytes());
    format!(
        "'sha256-{}'",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}
