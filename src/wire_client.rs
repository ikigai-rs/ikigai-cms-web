//! The browser's wire codec: encode an `ikigai-wire` `Call` and decode a `Reply`, so
//! the reading room can talk to `cms-server` over WebTransport. The kernel does NOT run
//! in the page — this is just the codec (the same bytes `ikigai-ipc`/`ikigai-quic`
//! speak); JS owns the WebTransport transport and htmx owns the swapping.

use ikigai_core::{ArgRef, Capability, Iri, Request, Verb};
use ikigai_wire::{decode, encode, Call, Reply};
use wasm_bindgen::prelude::*;

/// Build a `Source <iri>` request from `args` — a JSON object of string values, e.g.
/// `{"style":"mosaic"}`, or `{}` for none. `None` on a bad IRI.
fn source_request(iri: String, args_json: String) -> Option<Request> {
    let mut request = Request::new(Verb::Source, Iri::parse(iri).ok()?);
    if let Ok(args) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&args_json)
    {
        for (name, value) in args {
            let bytes = match value {
                serde_json::Value::String(s) => s.into_bytes(),
                other => other.to_string().into_bytes(),
            };
            request = request.with_arg(name, ArgRef::Inline(bytes));
        }
    }
    Some(request)
}

/// Encode an `Issue` Call — resolved under root (the trusted-host path, before rung 3's
/// relying party). Empty bytes on a bad IRI (the caller treats that as "nothing to send").
#[wasm_bindgen(js_name = encodeIssue)]
pub fn encode_issue(iri: String, args_json: String) -> Vec<u8> {
    match source_request(iri, args_json) {
        Some(request) => encode(&Call::Issue(request)).unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Encode an `IssueAs` Call — the same request carried under a capability scoped to
/// `scopes` (a JSON array of scope strings, e.g. the grant a verified passkey mints).
/// The server clamps it against the principal's entitlement, so a client can only
/// *attenuate*, never exceed, its grant. This is rung 3's browser-side cap boundary.
#[wasm_bindgen(js_name = encodeIssueAs)]
pub fn encode_issue_as(iri: String, args_json: String, scopes_json: String) -> Vec<u8> {
    let Some(request) = source_request(iri, args_json) else {
        return Vec::new();
    };
    let scopes: Vec<String> = serde_json::from_str(&scopes_json).unwrap_or_default();
    encode(&Call::IssueAs(request, Capability::scoped(scopes))).unwrap_or_default()
}

/// Decode a `Reply`. On `Resolved` returns the representation bytes as a string — for a
/// view that's the ready-to-swap HTML fragment; on any error returns a small error
/// fragment (so htmx swaps in something legible rather than nothing).
#[wasm_bindgen(js_name = decodeReply)]
pub fn decode_reply(bytes: Vec<u8>) -> String {
    match decode::<Reply>(&bytes) {
        Ok(Reply::Resolved(repr, _status)) => String::from_utf8_lossy(&repr.bytes).into_owned(),
        Ok(Reply::Error(e)) => error_html(&e),
        Ok(_) => error_html("unexpected reply kind"),
        Err(e) => error_html(&format!("undecodable reply: {e}")),
    }
}

fn error_html(msg: &str) -> String {
    let esc = msg
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<p class=\"cms-error\">{esc}</p>")
}
