//! `cms-zotero-links` — sweep the Zotero API once and rewrite the book link overlay, then print the
//! coverage summary. The pass lives in the lib as the `urn:cms:zotero-links` resource; this bin just
//! *sources* it, exactly as `cms-tag-suggest` does for `urn:cms:tag-suggest`.
//!
//! What it produces: for every book the API can be matched to, a durable
//! `urn:zotero:item:{KEY}` identity, and for the ones with a readable EPUB/PDF in Zotero storage, a
//! link to that file in Zotero's web reader. The books graph joins the overlay, so the card in the
//! reading room opens the book instead of an Open Library search.
//!
//! Needs `urn:secret:zotero-api-key` in the keystore (a Zotero key with personal-library read
//! access). Nothing is downloaded or proxied — the link points at Zotero and the browser's own
//! session does the authenticating.
//!
//! Run: `cargo run --features maintenance --bin cms-zotero-links` — configured by
//! `cms.toml` in the ikigai config home + CLI flags; no env vars.

use ikigai_cms_web::maintenance::{build_maintenance_kernel, default_status_path};
use ikigai_core::{Capability, Iri, Request, Verb};

#[tokio::main]
async fn main() {
    let cfg = match ikigai_cms_web::config::load(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if cfg.zotero.is_none() {
        eprintln!("no Zotero library configured — nothing to link (see --zotero)");
        std::process::exit(2);
    }
    eprintln!("overlay: {}", cfg.tags.zotero_links.display());

    // No configured path and no data home ⇒ nowhere to reconcile; stop rather than write the
    // cache to whatever directory we happened to start in.
    let Some(status_path) = cfg.linkstatus.or_else(default_status_path) else {
        eprintln!("HOME is not set");
        std::process::exit(2);
    };
    // Held past the kernel: a fresh sweep is exactly when a book gains a durable identity, so the
    // tag overlays are rekeyed onto it the moment the overlay it comes from is rewritten.
    let tags = cfg.tags.clone();
    let kernel = match build_maintenance_kernel(
        cfg.src_dir,
        cfg.zotero,
        cfg.bookmarks,
        status_path,
        cfg.tags,
        cfg.llm_provider,
    ) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let req = Request::new(Verb::Source, Iri::parse("urn:cms:zotero-links").unwrap());
    match kernel.issue(req, &Capability::root()).await {
        Ok(repr) => println!("\n{}", String::from_utf8_lossy(&repr.bytes)),
        Err(e) => {
            eprintln!("zotero-links failed: {e}");
            std::process::exit(1);
        }
    }
    // Move any tag decision that a book just became matchable for onto its durable key. A no-op
    // when the sweep matched nothing new, and never fatal — the overlays serve correctly either
    // way; a refusal only means they stay on the old key one more run.
    match tags.migrate_to_durable_keys() {
        Ok(report) => println!("{}", report.summary()),
        Err(refused) => eprintln!("{refused}"),
    }
}
