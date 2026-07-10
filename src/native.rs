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
    // The reading-room views — each IS a query, rendered as an htmx HTML fragment:
    // `urn:cms:view:{tag}` (cards for a tag), `urn:cms:search` (cards whose title
    // matches `q`), `urn:cms:tags` (the clickable tag index).
    let views = EndpointSpace::new()
        .bind(
            UriTemplate::parse("urn:cms:view:{tag}").expect("valid template"),
            TagView,
        )
        .bind(Exact::new("urn:cms:search"), SearchView)
        .bind(Exact::new("urn:cms:tags"), TagsIndex);
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
        // The tag index renders SPARQL-results XML (not the card RDF/XML), so it's a
        // distinct stylesheet not offered as a card theme.
        "tags" => include_str!("../styles/tags.xsl"),
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

/// Escape a value riding into a SPARQL double-quoted string literal.
fn sparql_lit(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The validated card stylesheet from the `style` arg — only shipped card themes are
/// honored (never resolve an arbitrary `urn:cms:style:*` off a client string).
fn card_style<'a>(inv: &'a Invocation<'_>) -> &'a str {
    match inv.inline_str("style").unwrap_or("catalog") {
        s @ ("catalog" | "mosaic" | "agenda") => s,
        _ => "catalog",
    }
}

/// Run a SPARQL query through the kernel (`as` format) and pipe the result through a
/// stylesheet resource (`urn:cms:style:{style}`) via `urn:xslt:transform` — the shared
/// spine of every view: **a view is a query, rendered by a stylesheet resource.** The
/// result is cacheable and golden-threaded through the queries it issues, so an edit to
/// the graph refreshes it.
async fn render(
    inv: &Invocation<'_>,
    sparql: &str,
    query: String,
    as_fmt: &str,
    style: &str,
) -> Result<Representation> {
    let q = Iri::parse(sparql).expect("valid sparql IRI");
    let out = inv
        .issue(
            Request::new(Verb::Source, q)
                .with_arg("query", ArgRef::Inline(query.into_bytes()))
                .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()))
                .with_arg("as", ArgRef::Inline(as_fmt.as_bytes().to_vec())),
        )
        .await?;
    let xslt = Iri::parse("urn:xslt:transform").expect("valid IRI");
    let html = inv
        .issue(
            Request::new(Verb::Source, xslt)
                .with_arg("content", ArgRef::Inline(out.bytes))
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

/// `urn:cms:view:{tag}` — the reading room for a tag: cards for every resource carrying
/// `dc:subject "{tag}"`, rendered through the chosen card stylesheet. Multi-valued
/// `dc:subject` stays as repeated RDF/XML elements so the stylesheet renders tag chips
/// with no string ops (xrust lacks them).
struct TagView;

#[async_trait]
impl Endpoint for TagView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let tag = inv
            .bindings
            .get("tag")
            .ok_or_else(|| Error::MissingArgument("tag".to_string()))?;
        let safe = sparql_lit(tag);
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag }} \
             WHERE {{ ?s dc:subject \"{safe}\" ; dc:title ?t ; dc:identifier ?u ; dc:subject ?tag }}"
        );
        render(
            inv,
            "urn:sparql:construct",
            query,
            "rdfxml",
            card_style(inv),
        )
        .await
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

/// `urn:cms:search` — cards for resources whose `dc:title` contains the `q` term
/// (case-insensitive), capped so a common term can't return the whole graph. Renders
/// through the same card stylesheets as a tag view.
struct SearchView;

#[async_trait]
impl Endpoint for SearchView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let q = inv.inline_str("q").unwrap_or("").trim();
        if q.is_empty() {
            return Err(Error::MissingArgument("q".to_string()));
        }
        let safe = sparql_lit(q);
        // Select up to 60 matching resources (title contains the term), then join their
        // tags — LIMIT lives in the subquery so it caps *resources*, not triples.
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag }} \
             WHERE {{ {{ SELECT DISTINCT ?s ?t ?u WHERE {{ \
                 ?s dc:title ?t ; dc:identifier ?u . \
                 FILTER(CONTAINS(LCASE(?t), LCASE(\"{safe}\"))) \
             }} ORDER BY ?t LIMIT 60 }} ?s dc:subject ?tag }}"
        );
        render(
            inv,
            "urn:sparql:construct",
            query,
            "rdfxml",
            card_style(inv),
        )
        .await
    }

    fn name(&self) -> &str {
        "cms-search"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:search")
            .summary(
                "Search the room: cards for resources whose dc:title contains the `q` \
                 term (case-insensitive, capped). A view is a query.",
            )
            .verb(Verb::Source)
    }
}

/// `urn:cms:tags` — the tag index: the room's top tags by frequency, each a clickable
/// chip that opens its view. Renders SPARQL-results XML (a SELECT with counts) through
/// the `tags` stylesheet, so navigation itself is a query.
struct TagsIndex;

#[async_trait]
impl Endpoint for TagsIndex {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             SELECT ?tag (COUNT(DISTINCT ?s) AS ?n) WHERE { ?s dc:subject ?tag } \
             GROUP BY ?tag ORDER BY DESC(?n) LIMIT 200"
            .to_string();
        render(inv, "urn:sparql:select", query, "xml", "tags").await
    }

    fn name(&self) -> &str {
        "cms-tags"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:tags")
            .summary(
                "The tag index: the room's top tags by frequency, each a clickable chip \
                 that opens its view. A view is a query.",
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

    fn resolve_html(kernel: &Kernel, iri: &str, args: &[(&str, &str)]) -> String {
        let mut request = Request::new(Verb::Source, Iri::parse(iri.to_string()).unwrap());
        for (k, v) in args {
            request = request.with_arg(*k, ArgRef::Inline(v.as_bytes().to_vec()));
        }
        let (repr, _status) = Resolver::issue(kernel, request).expect("resolves");
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

    #[test]
    fn the_view_chain_enforces_capabilities() {
        use ikigai_core::Capability;
        let (_dir, kernel) = kernel_over_fixture();
        let iri = || Iri::parse("urn:cms:view:quic").unwrap();
        // The whole view chain bottoms out in an fs read of the source jail, which the
        // FileEndpoint gates on the session capability. Under root it's allowed → the
        // view resolves. This is the boundary rung 3's clamp will enforce per-principal.
        assert!(
            Resolver::issue_as(
                &kernel,
                Request::new(Verb::Source, iri()),
                &Capability::root()
            )
            .is_ok(),
            "root resolves the view"
        );
        // Under a capability that doesn't grant the source read, the fs endpoint denies
        // and the whole view fails — so clamping the session cap down actually gates it.
        let restricted = Capability::scoped(["urn:cap:cms:nothing"]);
        assert!(
            Resolver::issue_as(&kernel, Request::new(Verb::Source, iri()), &restricted).is_err(),
            "a cap without the source read is denied"
        );
    }

    #[test]
    fn search_matches_titles_case_insensitively_and_renders_cards() {
        let (_dir, kernel) = kernel_over_fixture();
        let html = resolve_html(&kernel, "urn:cms:search", &[("q", "quic")]);
        assert!(html.contains("https://quicwg.org"), "title match: {html}");
        assert!(
            !html.contains("webassembly.org"),
            "non-matching title excluded: {html}"
        );
        // Case-insensitive: "ASSEMBLY" finds "WebAssembly".
        let html2 = resolve_html(&kernel, "urn:cms:search", &[("q", "ASSEMBLY")]);
        assert!(
            html2.contains("https://webassembly.org"),
            "case-insensitive: {html2}"
        );
    }

    #[test]
    fn the_tag_index_renders_clickable_chips_with_counts() {
        let (_dir, kernel) = kernel_over_fixture();
        // SPARQL-results XML (SELECT with counts) → the `tags` stylesheet → chips.
        let html = resolve_html(&kernel, "urn:cms:tags", &[]);
        assert!(
            html.contains("urn:cms:view:quic"),
            "quic chip links: {html}"
        );
        assert!(
            html.contains("urn:cms:view:wasm"),
            "wasm chip links: {html}"
        );
        assert!(
            html.contains("class='cms-tag'"),
            "rendered as chips: {html}"
        );
    }
}
