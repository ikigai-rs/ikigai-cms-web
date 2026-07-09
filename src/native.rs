//! The native CMS kernel: `build_cms_kernel` and the endpoints it composes.
//! Compiled only off wasm (it links the SPARQL/fs stack); the browser speaks the
//! wire codec in [`crate::wire_client`] instead.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ikigai_core::{
    ArgRef, Description, Endpoint, EndpointSpace, Error, Exact, Fallback, FnEndpoint, Invocation,
    Iri, Kernel, ReprType, Representation, Request, Result, Space, UriTemplate, Verb,
};

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
    // The reading-room stylesheets: `urn:cms:style:{name}` → an XSLT resource. The view
    // resolves one to render the graph, so the room restyles by naming a different one
    // (renderers are resources). Three ship embedded; a deployment can layer an fs
    // override for user-supplied themes.
    let styles = EndpointSpace::new().bind(
        UriTemplate::parse("urn:cms:style:{name}").expect("valid template"),
        FnEndpoint::new("cms-style", stylesheet),
    );

    let spaces: Vec<Arc<dyn Space>> = vec![
        Arc::new(src) as Arc<dyn Space>,
        Arc::new(graph) as Arc<dyn Space>,
        Arc::new(views) as Arc<dyn Space>,
        Arc::new(styles) as Arc<dyn Space>,
        Arc::new(ikigai_cms::space()) as Arc<dyn Space>,
        Arc::new(ikigai_sparql::space()) as Arc<dyn Space>,
        // urn:xslt:transform — the view pipes its CONSTRUCT'd RDF/XML through a stylesheet.
        Arc::new(ikigai_xslt::space()) as Arc<dyn Space>,
    ];
    Kernel::new(Arc::new(Fallback::new(spaces)))
}

/// `urn:cms:style:{name}` — a reading-room stylesheet (XSLT), keyed on the confirmed
/// oxigraph RDF/XML shape and authored to xrust's subset (no attribute value templates,
/// no string functions — tags are repeated `dc:subject` elements). Colors come from the
/// host page's CSS variables, so a theme stays light/dark aware.
fn stylesheet(inv: &Invocation<'_>) -> Result<Representation> {
    let name = inv
        .bindings
        .get("name")
        .map(|s| s.to_string())
        .unwrap_or_default();
    let xsl = match name.as_str() {
        "catalog" => include_str!("../styles/catalog.xsl"),
        "mosaic" => include_str!("../styles/mosaic.xsl"),
        "agenda" => include_str!("../styles/agenda.xsl"),
        _ => return Err(Error::Endpoint(format!("no stylesheet `{name}`"))),
    };
    Ok(Representation::new(
        ReprType::new("application/xslt+xml").with_param("charset", "utf-8"),
        xsl.as_bytes().to_vec(),
    )
    .cacheable())
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

/// `urn:cms:view:{tag}` — the reading room for a tag. CONSTRUCTs a per-card graph of
/// every resource carrying `dc:subject "{tag}"` (title, URL, and each tag as a separate
/// triple), serializes it as RDF/XML, and pipes it through the chosen stylesheet
/// (`urn:cms:style:{style}`, default `catalog`) via `urn:xslt:transform` — so the room's
/// look is *data*, restyled by naming a different stylesheet. Cacheable and
/// golden-threaded through the queries it issues, so an edit to the graph refreshes it.
struct TagView;

#[async_trait]
impl Endpoint for TagView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let tag = inv
            .bindings
            .get("tag")
            .ok_or_else(|| Error::MissingArgument("tag".to_string()))?;
        // Pick the stylesheet; only the shipped names are honored (never resolve an
        // arbitrary `urn:cms:style:*` off a client-supplied string).
        let style = match inv.inline_str("style").unwrap_or("catalog") {
            s @ ("catalog" | "mosaic" | "agenda") => s,
            _ => "catalog",
        };
        // The tag rides into a SPARQL string literal — escape the two chars that could
        // break out of it (a tag comes from a URI suffix, but stay safe by construction).
        let safe = tag.replace('\\', "\\\\").replace('"', "\\\"");
        // CONSTRUCT the per-card graph: multi-valued `dc:subject` stays as repeated
        // elements in the RDF/XML, so the stylesheet renders tag chips with no string
        // ops (xrust lacks them).
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag }} \
             WHERE {{ ?s dc:subject \"{safe}\" ; dc:title ?t ; dc:identifier ?u ; dc:subject ?tag }}"
        );
        let construct = Iri::parse("urn:sparql:construct").expect("valid IRI");
        let rdfxml = inv
            .issue(
                Request::new(Verb::Source, construct)
                    .with_arg("query", ArgRef::Inline(query.into_bytes()))
                    .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()))
                    .with_arg("as", ArgRef::Inline(b"rdfxml".to_vec())),
            )
            .await?;
        // Render it through the chosen stylesheet resource.
        let xslt = Iri::parse("urn:xslt:transform").expect("valid IRI");
        let html = inv
            .issue(
                Request::new(Verb::Source, xslt)
                    .with_arg("content", ArgRef::Inline(rdfxml.bytes))
                    .with_arg(
                        "stylesheet",
                        ArgRef::Inline(format!("urn:cms:style:{style}").into_bytes()),
                    ),
            )
            .await?;
        Ok(Representation::new(
            ReprType::new("text/html").with_param("charset", "utf-8"),
            html.bytes,
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
                 carrying `dc:subject {tag}`, rendered as cards through a stylesheet \
                 resource. A view is a query.",
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

    fn view(kernel: &Kernel, tag: &str, style: &str) -> String {
        let iri = Iri::parse(format!("urn:cms:view:{tag}")).unwrap();
        let request = Request::new(Verb::Source, iri)
            .with_arg("style", ArgRef::Inline(style.as_bytes().to_vec()));
        let (repr, _status) = Resolver::issue(kernel, request).expect("view resolves");
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
    fn a_tag_view_renders_html_cards_through_a_stylesheet() {
        let (_dir, kernel) = kernel_over_fixture();
        // Full pipeline: fixture → graph → CONSTRUCT → RDF/XML → xrust XSLT → htmx.
        let html = view(&kernel, "quic", "catalog");
        assert!(html.contains("class='cms-card'"), "{html}");
        assert!(html.contains("https://quicwg.org"), "{html}");
        assert!(
            html.contains("urn:cms:view:networking"),
            "tag chips link to co-tags: {html}"
        );
        assert!(
            !html.contains("webassembly.org"),
            "only quic-tagged resources: {html}"
        );
    }

    #[test]
    fn the_stylesheet_resource_swaps_the_skin() {
        let (_dir, kernel) = kernel_over_fixture();
        // Same view, same graph — only the named stylesheet differs.
        assert!(
            view(&kernel, "quic", "catalog").contains("flex-direction:column"),
            "catalog stacks"
        );
        assert!(
            view(&kernel, "quic", "mosaic").contains("display:grid"),
            "mosaic grids"
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
