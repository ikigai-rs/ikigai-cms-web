//! Query the CMS graph from the command line — a dev tool over [`build_cms_kernel`].
//!
//! ```text
//! cargo run --example query -- <src_dir> 'SELECT ?s WHERE { ?s ?p ?o } LIMIT 5'
//! ```
//!
//! `<src_dir>` is the jail root for `urn:cms:src:*` (e.g. `~/Dropbox/org-mode-files`,
//! which holds `old-org/pinboard-bookmarks.org`). The query runs against
//! `graph=urn:cms:graph`, the assembled bookmark graph.

use ikigai_core::{ArgRef, Iri, Request, Verb};
use ikigai_resolve::Resolver;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: query <src_dir> <sparql>");
    let query = args.next().expect("usage: query <src_dir> <sparql>");

    let kernel = ikigai_cms_web::build_cms_kernel(dir.into());
    let request = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.into_bytes()))
        .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));

    match Resolver::issue(&kernel, request) {
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
