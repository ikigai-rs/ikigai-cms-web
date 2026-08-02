//! `cms-linkcheck` — run the link-check pass once and print its summary. The pass itself lives in
//! the lib as the `urn:cms:linkcheck` resource; this bin just *sources* it (the `cms-server`
//! `urn:time` schedule sources the very same resource). Reads and reports only — removal is a
//! separate, reviewed step.
//!
//! Run: `cargo run --features maintenance --bin cms-linkcheck` — configured by
//! `~/.config/ikigai/cms.toml` + CLI flags (`--limit <n>` caps the pass); no env vars.

use ikigai_cms_web::maintenance::build_maintenance_kernel;
use ikigai_core::{ArgRef, Capability, Iri, Request, Verb};

#[tokio::main]
async fn main() {
    let cfg = match ikigai_cms_web::config::load(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let status_path = cfg
        .linkstatus
        .unwrap_or_else(ikigai_cms_web::maintenance::default_status_path);

    eprintln!(
        "source: {}  bookmarks: {}  status: {}",
        cfg.src_dir.display(),
        cfg.bookmarks.as_deref().unwrap_or("(default)"),
        status_path.display()
    );
    let kernel = build_maintenance_kernel(cfg.src_dir, cfg.zotero, cfg.bookmarks, status_path);
    let mut req = Request::new(Verb::Source, Iri::parse("urn:cms:linkcheck").unwrap());
    if let Some(limit) = cfg.limit {
        req = req.with_arg("limit", ArgRef::Inline(limit.into_bytes()));
    }
    match kernel.issue(req, &Capability::root()).await {
        Ok(repr) => println!("\n{}", String::from_utf8_lossy(&repr.bytes)),
        Err(e) => {
            eprintln!("link-check failed: {e}");
            std::process::exit(1);
        }
    }
}
