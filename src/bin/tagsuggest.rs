//! `cms-tag-suggest` — run the tag-suggestion pass once and print its summary. The pass lives in
//! the lib as the `urn:cms:tag-suggest` resource; this bin just *sources* it (the `cms-server`
//! `urn:time` schedule, S3, will source the very same resource behind the LLM gate). Writes the
//! suggestions overlay for `+`/`x` review — never a proper tag until you promote it.
//!
//! Run: `CMS_ZOTERO="$HOME/Dropbox/Documents/Zotero/My Library.rdf" \
//!       cargo run --features maintenance --bin cms-tag-suggest -- [limit]`
//!   optional: positional `<limit>` (default 5); `CMS_SRC_DIR` / `CMS_BOOKMARKS` as for cms-linkcheck.

use std::path::PathBuf;

use ikigai_cms_web::maintenance::{build_maintenance_kernel, default_status_path};
use ikigai_core::{ArgRef, Capability, Iri, Request, Verb};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let limit = args.next();
    let src_dir: PathBuf = std::env::var_os("CMS_SRC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Dropbox/org-mode-files"))
                .unwrap_or_default()
        });
    let zotero = std::env::var_os("CMS_ZOTERO").map(PathBuf::from);
    let bookmarks = std::env::var("CMS_BOOKMARKS").ok();

    eprintln!(
        "source: {}  zotero: {}",
        src_dir.display(),
        zotero
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none — no books)".into())
    );
    let kernel = build_maintenance_kernel(src_dir, zotero, bookmarks, default_status_path());
    let mut req = Request::new(Verb::Source, Iri::parse("urn:cms:tag-suggest").unwrap());
    if let Some(limit) = limit {
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
