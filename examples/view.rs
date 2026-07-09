//! Render a tag's reading-room view (an htmx HTML fragment) over a source dir. Dev
//! tool:
//!
//! ```text
//! cargo run --example view -- <src_dir> <tag>
//! ```
//!
//! `<src_dir>` is the jail root for `urn:cms:src:*` (e.g. `~/Dropbox/org-mode-files`).
//! Resolves `urn:cms:view:<tag>` — the same resource the browser fetches over the wire.

use ikigai_core::{Iri, Request, Verb};
use ikigai_resolve::Resolver;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: view <src_dir> <tag>");
    let tag = args.next().expect("usage: view <src_dir> <tag>");

    let kernel = ikigai_cms_web::build_cms_kernel(dir.into());
    let iri = Iri::parse(format!("urn:cms:view:{tag}")).expect("valid IRI");
    match Resolver::issue(&kernel, Request::new(Verb::Source, iri)) {
        Ok((repr, status)) => {
            println!("{}", String::from_utf8_lossy(&repr.bytes));
            eprintln!("[{status:?}]");
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
