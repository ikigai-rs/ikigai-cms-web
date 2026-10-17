//! ikigai-cms-web — the semantic-CMS reading room.
//!
//! The CMS is one RDF graph where everything is a tagged, linkable, queryable
//! resource. This crate serves it: a kernel composed over the CMS sources, reached by
//! a browser over the wire, rendered as a query-driven reading room (a view *is* a
//! query).
//!
//! Two faces, split by target:
//! - **native** ([`build_cms_kernel`], the private `native` module): the kernel — the CMS
//!   source files, the assembled bookmark graph (`urn:cms:graph`), SPARQL, and the
//!   `urn:cms:view:{tag}` reading-room render. The `cms-server` bin serves it over
//!   WebTransport.
//! - **wasm** (the `wire_client` module, wasm32 only): only the `ikigai-wire` codec (`encodeIssue` /
//!   `decodeReply`) the browser calls to talk to that server. The kernel never runs in
//!   the page — the browser is a thin client that resolves views over the wire.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
mod presentations;
#[cfg(not(target_family = "wasm"))]
pub use native::{
    build_cms_kernel, build_cms_kernel_with, cms_spaces, cms_spaces_with, pct_decode, safe_href,
    style_blocks, url_segment,
};
#[cfg(not(target_family = "wasm"))]
pub use presentations::Presentations;
// The watch a serving host runs over the decks root, so a new or edited deck reaches the room
// without a restart (ledger #286).
#[cfg(feature = "server")]
pub use presentations::DeckWatch;

// The tag overlays (approved + suggested tags) merged into urn:cms:graph — provisional LLM/OL
// tag suggestions with a human-authorized promote/dismiss, kept off the regenerated sources.
#[cfg(not(target_family = "wasm"))]
pub mod tagstore;

// Locked, atomic read-modify-write of the room's own state files (the status cache and the tag
// overlays), which have writers in the serving kernel, the maintenance kernel and the bins.
#[cfg(not(target_family = "wasm"))]
mod filelock;

// Graph maintenance (the link-checker) — behind the `maintenance` feature, which adds
// outbound HTTP. `urn:cms:linkcheck` caches each check for a week.
#[cfg(feature = "maintenance")]
pub mod maintenance;

// Normalizing the ISBNs the Zotero export hands us (hyphenated, and often several in one field).
// Shared by the Zotero link pass, which matches on them, and the tag-suggest pass, which looks
// them up in OpenLibrary — both are `maintenance`.
#[cfg(feature = "maintenance")]
mod isbn;

// The Zotero link pass (`urn:cms:zotero-links`) — sweeps the Zotero API for the durable item
// identity and the readable attachment behind each book, and writes the overlay the books graph
// joins. Outbound HTTP + a credential, so it rides the `maintenance` feature with the rest.
#[cfg(feature = "maintenance")]
pub mod zotero;

// Bin configuration: `cms.toml` in the ikigai config home + CLI flags (no env vars). Behind
// `maintenance` — the feature every bin has (`server` includes it).
#[cfg(feature = "maintenance")]
pub mod config;

// The server-side WebAuthn relying party (rung 3b) — behind the `server` feature with
// the rest of the native server stack.
#[cfg(feature = "server")]
pub mod session;

// The Content-Security-Policy the page face sends on every response (ledger #1793): strict
// script and style sources, the views' own `<style>` blocks admitted by hash.
#[cfg(feature = "server")]
pub mod csp;

// The page face's accept loop: what an accept error means (skip one connection, back off on
// exhaustion, end on a dead listener). In the lib rather than the bin so a test can drive it in a
// child process with its descriptor limit lowered (`tests/accept_exhaustion.rs`).
#[cfg(feature = "server")]
pub mod accept;

#[cfg(target_family = "wasm")]
mod wire_client;
