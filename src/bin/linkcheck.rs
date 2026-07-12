//! `cms-linkcheck` — run the link-check pass once and print its summary. The pass itself lives in
//! the lib as the `urn:cms:linkcheck` resource; this bin just *sources* it (the `cms-server`
//! `urn:time` schedule sources the very same resource). Reads and reports only — removal is a
//! separate, reviewed step.
//!
//! Run: `CMS_BOOKMARKS=bookmarks-src.org cargo run --features maintenance --bin cms-linkcheck`
//!   optional: positional `<src_dir>` and `<limit>`; `CMS_LINKSTATUS` overrides the cache path.

use std::path::PathBuf;

use ikigai_cms_web::maintenance::build_maintenance_kernel;
use ikigai_core::{ArgRef, Capability, Iri, Request, Verb};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let src_dir: PathBuf = args
        .next()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CMS_SRC_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Dropbox/org-mode-files"))
                .unwrap_or_default()
        });
    let limit = args.next();
    let bookmarks = std::env::var("CMS_BOOKMARKS").ok();
    let status_path = std::env::var("CMS_LINKSTATUS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| src_dir.join(".cms-linkstatus.json"));

    eprintln!(
        "source: {}  bookmarks: {}  status: {}",
        src_dir.display(),
        bookmarks.as_deref().unwrap_or("(default)"),
        status_path.display()
    );
    let kernel = build_maintenance_kernel(src_dir, bookmarks, status_path);
    let mut req = Request::new(Verb::Source, Iri::parse("urn:cms:linkcheck").unwrap());
    if let Some(limit) = limit {
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
