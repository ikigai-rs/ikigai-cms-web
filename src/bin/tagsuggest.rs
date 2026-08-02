//! `cms-tag-suggest` — run the tag-suggestion pass once and print its summary. The pass lives in
//! the lib as the `urn:cms:tag-suggest` resource; this bin just *sources* it (the `cms-server`
//! `urn:time` schedule, S3, sources the very same resource behind the LLM gate). Writes the
//! suggestions overlay for `+`/`x` review — never a proper tag until you promote it.
//!
//! Run: `cargo run --features maintenance --bin cms-tag-suggest` — configured by
//! `~/.config/ikigai/cms.toml` + CLI flags (`--limit <n>`, default 5); no env vars.

use ikigai_cms_web::maintenance::{build_maintenance_kernel, default_status_path};
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

    eprintln!(
        "source: {}  zotero: {}",
        cfg.src_dir.display(),
        cfg.zotero
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none — no books)".into())
    );
    let status_path = cfg.linkstatus.unwrap_or_else(default_status_path);
    let kernel = match build_maintenance_kernel(
        cfg.src_dir,
        cfg.zotero,
        cfg.bookmarks,
        status_path,
        cfg.llm_provider,
    ) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let mut req = Request::new(Verb::Source, Iri::parse("urn:cms:tag-suggest").unwrap());
    if let Some(limit) = cfg.limit {
        req = req.with_arg("limit", ArgRef::Inline(limit.into_bytes()));
    }
    match kernel.issue(req, &Capability::root()).await {
        Ok(repr) => println!("\n{}", String::from_utf8_lossy(&repr.bytes)),
        Err(e) => {
            eprintln!("tag-suggest failed: {e}");
            std::process::exit(1);
        }
    }
}
