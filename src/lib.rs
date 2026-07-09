//! ikigai-cms-web — the semantic-CMS reading room.
//!
//! The CMS is one RDF graph where everything is a tagged, linkable, queryable
//! resource. This crate serves it: a kernel composed over the CMS sources, reached
//! by a browser over the wire, rendered as a query-driven reading room (a view *is*
//! a query). It is built in rungs:
//!
//! - **Rung 1 (here): the kernel spine.** [`build_cms_kernel`] composes the CMS
//!   source files (jailed, read through the kernel), the assembled bookmark graph
//!   (`urn:cms:graph`, org → Turtle via the ikigai-cms transreptor), and SPARQL over
//!   it (`urn:sparql:*`). Proven end to end: a `SELECT` over `graph=urn:cms:graph`
//!   returns the tagged bookmarks.
//! - Rung 2+: a WebTransport server, an htmx reading room (XSLT type-renderers), a
//!   passkey relying party, and the WebGPU view — layered on this spine.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ikigai_core::{
    ArgRef, Description, Endpoint, EndpointSpace, Error, Exact, Fallback, Invocation, Iri, Kernel,
    ReprType, Representation, Request, Result, Space, UriTemplate, Verb,
};

mod render;

/// The bookmarks file, addressed within the CMS source jail (`urn:cms:src:*`). The
/// jail root is supplied to [`build_cms_kernel`]; the path below is relative to it.
const BOOKMARKS_SRC: &str = "urn:cms:src:old-org/pinboard-bookmarks.org";

/// Compose the CMS kernel over `src_dir` (the jail root for `urn:cms:src:*`).
///
/// Binds:
/// - `urn:cms:src:{path}` — CMS source files, jailed to `src_dir`, cacheable +
///   golden-threaded (a change to a source file invalidates everything derived).
/// - `urn:cms:graph` — the assembled bookmark graph as Turtle (reads the org file
///   through the kernel, transrepts via [`ikigai_cms::bookmarks_to_turtle`]).
/// - `urn:cms:bookmarks` — the raw pipe-face transreptor, from ikigai-cms.
/// - `urn:sparql:{select,ask,describe,construct}` — SPARQL over `graph=<uri>`
///   sources resolved through the kernel; point `graph=urn:cms:graph` at the above.
pub fn build_cms_kernel(src_dir: PathBuf) -> Kernel {
    // The CMS source jail: real files, read THROUGH the kernel (cacheable + watched),
    // never with std::fs — so the derived graph is golden-threaded to them.
    let src = EndpointSpace::new().bind(
        UriTemplate::parse("urn:cms:src:{path}").expect("valid template"),
        ikigai_fs::FileEndpoint::new(src_dir).cacheable(),
    );
    // The assembled graph resource SPARQL points `graph=` at.
    let graph = EndpointSpace::new().bind(Exact::new("urn:cms:graph"), BookmarkGraph);
    // The reading-room views: `urn:cms:view:{tag}` → an htmx HTML fragment of every
    // resource tagged `{tag}`, rendered as cards. A view IS a query.
    let views = EndpointSpace::new().bind(
        UriTemplate::parse("urn:cms:view:{tag}").expect("valid template"),
        TagView,
    );

    let spaces: Vec<Arc<dyn Space>> = vec![
        Arc::new(src) as Arc<dyn Space>,
        Arc::new(graph) as Arc<dyn Space>,
        Arc::new(views) as Arc<dyn Space>,
        Arc::new(ikigai_cms::space()) as Arc<dyn Space>,
        Arc::new(ikigai_sparql::space()) as Arc<dyn Space>,
    ];
    Kernel::new(Arc::new(Fallback::new(spaces)))
}

/// `urn:cms:graph` — assemble the bookmark graph. Reads the org bookmarks file
/// through the kernel and transrepts it to Turtle; cacheable and golden-threaded on
/// the source (`inv.source` records the dependency, so a write/watch on the file
/// recomputes the graph — and everything queried from it).
struct BookmarkGraph;

#[async_trait]
impl Endpoint for BookmarkGraph {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let iri = Iri::parse(BOOKMARKS_SRC).expect("BOOKMARKS_SRC is a valid IRI");
        let src = inv.source(&iri).await?;
        let text = std::str::from_utf8(&src.bytes)
            .map_err(|e| Error::Endpoint(format!("bookmark source is not UTF-8: {e}")))?;
        let turtle = ikigai_cms::bookmarks_to_turtle(text);
        Ok(Representation::new(
            ReprType::new("text/turtle").with_param("charset", "utf-8"),
            turtle.into_bytes(),
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-graph"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:graph")
            .summary(
                "The assembled bookmark graph as RDF/Turtle: the org bookmarks file read \
                 through the kernel and transrepted onto the dc:subject tag axis. Point \
                 `urn:sparql:* graph=urn:cms:graph` at it.",
            )
            .verb(Verb::Source)
    }
}

/// `urn:cms:view:{tag}` — the reading room for a tag. Runs a SPARQL SELECT for every
/// resource carrying `dc:subject "{tag}"` (title, URL, and its full tag set) and
/// renders an htmx HTML fragment of cards. Cacheable and golden-threaded through the
/// query it issues, so an edit to the graph refreshes the view.
///
/// The render is a Rust template for now; it moves to swappable XSLT stylesheet
/// resources (`urn:cms:style:*`) next, so the room can be restyled without a rebuild.
struct TagView;

#[async_trait]
impl Endpoint for TagView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let tag = inv
            .bindings
            .get("tag")
            .ok_or_else(|| Error::MissingArgument("tag".to_string()))?;
        // The tag rides into a SPARQL string literal — escape the two chars that could
        // break out of it (a tag comes from a URI suffix, but stay safe by construction).
        let safe = tag.replace('\\', "\\\\").replace('"', "\\\"");
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             SELECT ?url ?title (GROUP_CONCAT(DISTINCT ?t; separator=\" \") AS ?tags) \
             WHERE {{ ?s dc:subject \"{safe}\" ; dc:identifier ?url ; dc:title ?title ; \
             dc:subject ?t }} GROUP BY ?url ?title ORDER BY ?title"
        );
        let sparql = Iri::parse("urn:sparql:select").expect("urn:sparql:select is a valid IRI");
        let request = Request::new(Verb::Source, sparql)
            .with_arg("query", ArgRef::Inline(query.into_bytes()))
            .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
        let results = inv.issue(request).await?;
        let rows = render::parse_rows(&results.bytes)
            .map_err(|e| Error::Endpoint(format!("bad SPARQL results: {e}")))?;
        Ok(Representation::new(
            ReprType::new("text/html").with_param("charset", "utf-8"),
            render::cards_html(tag, &rows).into_bytes(),
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-view"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:view")
            .summary(
                "The reading room for a tag: an htmx HTML fragment of every resource \
                 carrying `dc:subject {tag}`, rendered as cards. A view is a query.",
            )
            .verb(Verb::Source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikigai_resolve::Resolver;

    /// Write a tiny bookmarks fixture at the exact path the kernel expects, then
    /// prove the full spine: fs read → transrept → `urn:cms:graph` → SPARQL SELECT.
    fn kernel_over_fixture() -> (tempfile::TempDir, Kernel) {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n\
             ** [[https://webassembly.org][WebAssembly]]\n\
             \x20  :PROPERTIES:\n\
             \x20  :TAGS: wasm web\n\
             \x20  :END:\n\
             ** [[https://quicwg.org][QUIC Working Group]]\n\
             \x20  :PROPERTIES:\n\
             \x20  :TAGS: quic networking\n\
             \x20  :END:\n",
        )
        .unwrap();
        let kernel = build_cms_kernel(dir.path().to_path_buf());
        (dir, kernel)
    }

    fn select(kernel: &Kernel, query: &str) -> String {
        let request = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
            .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
            .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
        let (repr, _status) = Resolver::issue(kernel, request).expect("query resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    fn view(kernel: &Kernel, tag: &str) -> String {
        let iri = Iri::parse(format!("urn:cms:view:{tag}")).unwrap();
        let (repr, _status) =
            Resolver::issue(kernel, Request::new(Verb::Source, iri)).expect("view resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    #[test]
    fn a_select_over_the_cms_graph_finds_a_tagged_bookmark() {
        let (_dir, kernel) = kernel_over_fixture();
        // The bookmark is skolemized; its URL rides as dc:identifier — join through it.
        let json = select(
            &kernel,
            "SELECT ?url WHERE { \
               ?s <http://purl.org/dc/elements/1.1/subject> \"quic\" ; \
                  <http://purl.org/dc/elements/1.1/identifier> ?url }",
        );
        assert!(
            json.contains("quicwg.org"),
            "expected the QUIC bookmark in results, got: {json}"
        );
        assert!(
            !json.contains("webassembly.org"),
            "the wasm bookmark should not match a quic query, got: {json}"
        );
    }

    #[test]
    fn a_tag_view_renders_html_cards_over_the_graph() {
        let (_dir, kernel) = kernel_over_fixture();
        let html = view(&kernel, "quic");
        assert!(html.contains("class=\"cms-card\""), "{html}");
        assert!(html.contains(">#quic</h2>"), "{html}");
        assert!(html.contains("https://quicwg.org"), "{html}");
        assert!(
            !html.contains("webassembly.org"),
            "only quic-tagged resources: {html}"
        );
    }

    #[test]
    fn the_tag_axis_joins_across_entries() {
        let (_dir, kernel) = kernel_over_fixture();
        // Every tagged subject — proves the graph assembled and is queryable as a whole.
        let json = select(
            &kernel,
            "SELECT (COUNT(DISTINCT ?s) AS ?n) \
             WHERE { ?s <http://purl.org/dc/elements/1.1/subject> ?t }",
        );
        assert!(
            json.contains('2'),
            "expected 2 tagged bookmarks, got: {json}"
        );
    }
}
