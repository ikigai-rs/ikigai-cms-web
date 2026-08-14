//! ikigai-cms-web — the semantic-CMS reading room.
//!
//! The CMS is one RDF graph where everything is a tagged, linkable, queryable
//! resource. This crate serves it: a kernel composed over the CMS sources, reached by
//! a browser over the wire, rendered as a query-driven reading room (a view *is* a
//! query).
//!
//! Two faces, split by target:
//! - **native** ([`build_cms_kernel`], the [`native`] module): the kernel — the CMS
//!   source files, the assembled bookmark graph (`urn:cms:graph`), SPARQL, and the
//!   `urn:cms:view:{tag}` reading-room render. The `cms-server` bin serves it over
//!   WebTransport.
//! - **wasm** ([`wire_client`]): only the `ikigai-wire` codec (`encodeIssue` /
//!   `decodeReply`) the browser calls to talk to that server. The kernel never runs in
//!   the page — the browser is a thin client that resolves views over the wire.

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
mod presentations;
#[cfg(not(target_family = "wasm"))]
pub use native::{build_cms_kernel, build_cms_kernel_with, cms_spaces, cms_spaces_with};
#[cfg(not(target_family = "wasm"))]
pub use presentations::Presentations;

// The tag overlays (approved + suggested tags) merged into urn:cms:graph — provisional LLM/OL
// tag suggestions with a human-authorized promote/dismiss, kept off the regenerated sources.
#[cfg(not(target_family = "wasm"))]
pub mod tagstore;

// Graph maintenance (the link-checker) — behind the `maintenance` feature, which adds
// outbound HTTP. `urn:cms:linkcheck` caches each check for a week.
#[cfg(feature = "maintenance")]
pub mod maintenance;

// The Zotero link pass (`urn:cms:zotero-links`) — sweeps the Zotero API for the durable item
// identity and the readable attachment behind each book, and writes the overlay the books graph
// joins. Outbound HTTP + a credential, so it rides the `maintenance` feature with the rest.
#[cfg(feature = "maintenance")]
pub mod zotero;

// Bin configuration: ~/.config/ikigai/cms.toml + CLI flags (no env vars). Behind
// `maintenance` — the feature every bin has (`server` includes it).
#[cfg(feature = "maintenance")]
pub mod config;

// The server-side WebAuthn relying party (rung 3b) — behind the `server` feature with
// the rest of the native server stack.
#[cfg(feature = "server")]
pub mod session;

#[cfg(target_family = "wasm")]
mod wire_client;
