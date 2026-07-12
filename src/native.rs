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

/// The default bookmarks file, as a path within the CMS source jail (`urn:cms:src:*`,
/// relative to the jail root). Overridable via `CMS_BOOKMARKS` (see the `cms-server` bin).
const DEFAULT_BOOKMARKS: &str = "old-org/pinboard-bookmarks.org";

/// Compose the CMS kernel over `src_dir` (the jail root for `urn:cms:src:*`) and an
/// optional Zotero library (`My Library.rdf`) whose books join the same graph.
///
/// Binds:
/// - `urn:cms:src:{path}` — CMS source files, jailed to `src_dir`, cacheable +
///   golden-threaded (a change to a source file invalidates everything derived).
/// - `urn:cms:src:zotero` — the Zotero RDF library (if configured), served as
///   RDF/XML with an injected base so its relative IRIs resolve.
/// - `urn:cms:graph:bookmarks` — bookmarks as Turtle (org → [`ikigai_cms`]).
/// - `urn:cms:graph:books` — Zotero books normalized onto the CMS axis (a SPARQL
///   CONSTRUCT: `cms:Book`, `dc:title`, `dc:creator`, slugged `dc:subject`, and an
///   Open Library lookup as `dc:identifier` so a book renders like a bookmark).
/// - `urn:cms:graph` — the whole CMS: bookmarks ⊕ books, what SPARQL points at.
/// - `urn:sparql:{select,ask,describe,construct}` — SPARQL over `graph=<uri>`.
pub fn build_cms_kernel(src_dir: PathBuf, zotero: Option<PathBuf>) -> Kernel {
    build_cms_kernel_with(src_dir, zotero, None, None)
}

/// [`build_cms_kernel`] plus lectern presentations (`urn:cms:graph:presentations`): the
/// decks under the configured root join the graph as `cms:Presentation` resources. `None`
/// = no decks. `bookmarks` overrides the bookmarks file sub-path (relative to the jail
/// root); `None` uses [`DEFAULT_BOOKMARKS`].
pub fn build_cms_kernel_with(
    src_dir: PathBuf,
    zotero: Option<PathBuf>,
    presentations: Option<crate::presentations::Presentations>,
    bookmarks: Option<String>,
) -> Kernel {
    Kernel::new(Arc::new(Fallback::new(cms_spaces_with(
        src_dir,
        zotero,
        presentations,
        bookmarks,
    ))))
}

/// The spaces the CMS kernel is composed of, exposed so a maintenance kernel can add HTTP
/// (link-checking) alongside the same graph. See [`build_cms_kernel`] for the bindings.
pub fn cms_spaces(src_dir: PathBuf, zotero: Option<PathBuf>) -> Vec<Arc<dyn Space>> {
    cms_spaces_with(src_dir, zotero, None, None)
}

/// [`cms_spaces`] plus the presentations config and an optional bookmarks sub-path override.
pub fn cms_spaces_with(
    src_dir: PathBuf,
    zotero: Option<PathBuf>,
    presentations: Option<crate::presentations::Presentations>,
    bookmarks: Option<String>,
) -> Vec<Arc<dyn Space>> {
    // The CMS source jail: real files, read THROUGH the kernel (cacheable + watched),
    // never with std::fs — so the derived graph is golden-threaded to them.
    let src = EndpointSpace::new().bind(
        UriTemplate::parse("urn:cms:src:{path}").expect("valid template"),
        ikigai_fs::FileEndpoint::new(src_dir).cacheable(),
    );
    // The Zotero library source (its filename has a space, so it can't ride the
    // `urn:cms:src:{path}` template — a dedicated binding, present only if configured).
    let mut zotero_space = EndpointSpace::new();
    if let Some(path) = zotero {
        zotero_space = zotero_space.bind(Exact::new("urn:cms:src:zotero"), ZoteroSource(path));
    }
    // Deck files, read THROUGH the kernel (`urn:cms:deck:{path}`, rooted at the presentations
    // dir) so the presentations graph is golden-threaded + capability-gated on them, not a raw
    // std::fs read. Bound only when a presentations root is configured.
    let mut deck_space = EndpointSpace::new();
    if let Some(cfg) = &presentations {
        deck_space = deck_space.bind(
            UriTemplate::parse("urn:cms:deck:{path}").expect("valid template"),
            ikigai_fs::FileEndpoint::new(cfg.root.clone()).cacheable(),
        );
    }
    // The graph resources: bookmarks, books, and their union (what SPARQL points at).
    let bookmarks_src = format!(
        "urn:cms:src:{}",
        bookmarks.as_deref().unwrap_or(DEFAULT_BOOKMARKS)
    );
    // The purge writes the same bookmarks resource; keep its IRI before BookmarkGraph consumes it.
    #[cfg(feature = "maintenance")]
    let bookmarks_src_purge = bookmarks_src.clone();
    let graph = EndpointSpace::new()
        .bind(
            Exact::new("urn:cms:graph:bookmarks"),
            BookmarkGraph { src: bookmarks_src },
        )
        .bind(Exact::new("urn:cms:graph:books"), BooksGraph)
        .bind(
            Exact::new("urn:cms:graph:presentations"),
            crate::presentations::PresentationsGraph {
                config: presentations,
            },
        )
        .bind(Exact::new("urn:cms:graph"), CmsGraph);
    // The reading-room views — each IS a query, rendered as an htmx HTML fragment:
    // `urn:cms:view:{tag}` (cards for a tag), `urn:cms:search` (cards whose title
    // matches `q`), `urn:cms:tags` (the clickable tag index).
    let views = EndpointSpace::new()
        .bind(
            UriTemplate::parse("urn:cms:view:{tag}").expect("valid template"),
            TagView,
        )
        .bind(Exact::new("urn:cms:search"), SearchView)
        .bind(Exact::new("urn:cms:tags"), TagsIndex)
        .bind(
            UriTemplate::parse("urn:cms:type:{type}").expect("valid template"),
            TypeView,
        )
        .bind(Exact::new("urn:cms:types"), TypesIndex);
    // The reading-room stylesheets: `urn:cms:style:{name}` → an XSLT resource. The view
    // resolves one to render the graph, so the room restyles by naming a different one
    // (renderers are resources). Three ship embedded; a deployment can layer an fs
    // override for user-supplied themes.
    let styles = EndpointSpace::new().bind(
        UriTemplate::parse("urn:cms:style:{name}").expect("valid template"),
        FnEndpoint::new("cms-style", stylesheet),
    );

    #[allow(unused_mut)]
    let mut spaces = vec![
        // Before `src`: the exact `urn:cms:src:zotero` must win over the `urn:cms:src:{path}`
        // template (which would otherwise match it with path=`zotero`).
        Arc::new(zotero_space) as Arc<dyn Space>,
        Arc::new(deck_space) as Arc<dyn Space>,
        Arc::new(src) as Arc<dyn Space>,
        Arc::new(graph) as Arc<dyn Space>,
        Arc::new(views) as Arc<dyn Space>,
        Arc::new(styles) as Arc<dyn Space>,
        Arc::new(ikigai_cms::space()) as Arc<dyn Space>,
        Arc::new(ikigai_sparql::space()) as Arc<dyn Space>,
        // urn:xslt:transform — the view pipes its CONSTRUCT'd RDF/XML through a stylesheet.
        Arc::new(ikigai_xslt::space()) as Arc<dyn Space>,
    ];
    // The link-check status indicator (`urn:cms:linkstatus`) — a file-backed HTML fragment the room
    // htmx-polls for the running/last-run state. Present only when the maintenance stack is
    // compiled in (it shares that status format); it does no network, so it's safe in the serving
    // kernel.
    #[cfg(feature = "maintenance")]
    spaces.push(Arc::new(
        EndpointSpace::new()
            .bind(
                Exact::new("urn:cms:linkstatus"),
                crate::maintenance::LinkStatusView,
            )
            // urn:cms:review — the suggested-deletes review, rendered via urn:cms:style:review.
            .bind(Exact::new("urn:cms:review"), crate::maintenance::ReviewView)
            // urn:cms:purge — the reviewed removal (Source = confirm, Sink = execute).
            .bind(
                Exact::new("urn:cms:purge"),
                crate::maintenance::PurgeView {
                    bak_iri: format!("{bookmarks_src_purge}.bak"),
                    bookmarks_iri: bookmarks_src_purge,
                },
            ),
    ) as Arc<dyn Space>);
    spaces
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
        // The recency trail renders a small session-supplied doc (urn:cms:recent#), not
        // graph data — also a distinct stylesheet, not a card theme.
        "recent" => include_str!("../styles/recent.xsl"),
        // The type index renders SPARQL-results XML (kinds + counts) — like tags.
        "types" => include_str!("../styles/types.xsl"),
        // The suggested-deletes review renders a server-supplied doc (urn:cms:review#).
        "review" => include_str!("../styles/review.xsl"),
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
struct BookmarkGraph {
    /// The `urn:cms:src:{path}` IRI of the bookmarks org file (configurable via
    /// `CMS_BOOKMARKS`; defaults to [`DEFAULT_BOOKMARKS`]).
    src: String,
}

#[async_trait]
impl Endpoint for BookmarkGraph {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let iri = Iri::parse(&self.src)
            .map_err(|e| Error::Endpoint(format!("bad bookmarks IRI: {e}")))?;
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
        Description::new("urn:cms:graph:bookmarks")
            .summary(
                "Bookmarks as RDF/Turtle: the org bookmarks file read through the kernel \
                 and transrepted onto the dc:subject tag axis.",
            )
            .verb(Verb::Source)
    }
}

/// The CONSTRUCT that normalizes Zotero's RDF onto the CMS axis: each `bib:Book` becomes
/// a skolemized `cms:Book` with `dc:title`, `dc:creator` ("Surname, Given" per author),
/// slugged `dc:subject` tags (letter-bearing only — drops call-number noise), and an
/// Open Library title-search as `dc:identifier` so a book renders like a bookmark card.
const BOOK_CONSTRUCT: &str = r#"PREFIX bib: <http://purl.org/net/biblio#>
PREFIX dc: <http://purl.org/dc/elements/1.1/>
PREFIX z: <http://www.zotero.org/namespaces/export#>
PREFIX foaf: <http://xmlns.com/foaf/0.1/>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX cms: <https://ikigai-rs.dev/ns/cms#>
CONSTRUCT { ?id a cms:Book ; dc:title ?title ; dc:identifier ?lookup ; dc:creator ?author ; dc:subject ?slug }
WHERE {
  ?book a bib:Book ; dc:title ?title .
  BIND(IRI(CONCAT("urn:cms:book:", SHA256(STR(?book)))) AS ?id)
  BIND(CONCAT("https://openlibrary.org/search?q=", ENCODE_FOR_URI(?title)) AS ?lookup)
  OPTIONAL {
    ?book bib:authors ?seq . ?seq ?ap ?person .
    FILTER(STRSTARTS(STR(?ap), "http://www.w3.org/1999/02/22-rdf-syntax-ns#_"))
    ?person foaf:surname ?sn . OPTIONAL { ?person foaf:givenName ?gn }
    BIND(IF(BOUND(?gn), CONCAT(?sn, ", ", ?gn), ?sn) AS ?author)
  }
  OPTIONAL {
    ?book dc:subject ?tn . ?tn rdf:value ?tag .
    BIND(LCASE(REPLACE(REPLACE(?tag, "[^a-zA-Z0-9]+", "-"), "(^-+|-+$)", "")) AS ?slug0)
    FILTER(REGEX(?slug0, "[a-z]"))
    BIND(?slug0 AS ?slug)
  }
}"#;

/// `urn:cms:src:zotero` — the Zotero `My Library.rdf`, served as RDF/XML with an injected
/// `xml:base` so its relative IRIs (`#item_N`) resolve when SPARQL parses it. A dedicated
/// source (not a `FileEndpoint`) because the filename has a space and the file needs the
/// base-injection preprocessing.
struct ZoteroSource(PathBuf);

#[async_trait]
impl Endpoint for ZoteroSource {
    async fn invoke(&self, _inv: &Invocation<'_>) -> Result<Representation> {
        let text = std::fs::read_to_string(&self.0)
            .map_err(|e| Error::Endpoint(format!("read zotero library: {e}")))?;
        let based = text.replacen("<rdf:RDF", "<rdf:RDF xml:base=\"http://zotero.local/\"", 1);
        Ok(Representation::new(
            ReprType::new("application/rdf+xml").with_param("charset", "utf-8"),
            based.into_bytes(),
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-src-zotero"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:src:zotero")
            .summary("The Zotero RDF library (My Library.rdf), base-injected for parsing.")
            .verb(Verb::Source)
    }
}

/// `urn:cms:graph:books` — Zotero books normalized onto the CMS axis via [`BOOK_CONSTRUCT`].
/// Tolerant of no library configured (or a parse failure): yields empty Turtle so the
/// whole graph degrades to bookmarks-only rather than failing.
struct BooksGraph;

#[async_trait]
impl Endpoint for BooksGraph {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let construct = Iri::parse("urn:sparql:construct").expect("valid IRI");
        let req = Request::new(Verb::Source, construct)
            .with_arg("query", ArgRef::Inline(BOOK_CONSTRUCT.as_bytes().to_vec()))
            .with_arg("graph", ArgRef::Inline(b"urn:cms:src:zotero".to_vec()))
            .with_arg("as", ArgRef::Inline(b"turtle".to_vec()));
        let turtle = match inv.issue(req).await {
            Ok(repr) => repr.bytes,
            Err(_) => Vec::new(), // no library / unparseable → no books
        };
        Ok(Representation::new(
            ReprType::new("text/turtle").with_param("charset", "utf-8"),
            turtle,
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-graph-books"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:graph:books")
            .summary(
                "Zotero books normalized onto the CMS axis (cms:Book, dc:title/creator/subject).",
            )
            .verb(Verb::Source)
    }
}

/// `urn:cms:graph` — the whole CMS: bookmarks ⊕ books, as one Turtle document (both are
/// Turtle; concatenation is valid and needs no extra dependency). Cacheable and
/// golden-threaded through the two sub-graphs it issues.
struct CmsGraph;

#[async_trait]
impl Endpoint for CmsGraph {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let bookmarks = inv
            .issue(Request::new(
                Verb::Source,
                Iri::parse("urn:cms:graph:bookmarks").expect("valid IRI"),
            ))
            .await?;
        // Type the bookmarks `cms:Bookmark` (ikigai-cms doesn't emit a type; books already
        // carry `cms:Book`). Constructed over the bookmarks graph alone, so it can't touch
        // books. This makes the type facet honest: every resource declares its kind.
        let bookmark_types = inv
            .issue(
                Request::new(
                    Verb::Source,
                    Iri::parse("urn:sparql:construct").expect("valid IRI"),
                )
                .with_arg(
                    "query",
                    ArgRef::Inline(
                        "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
                         PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
                         CONSTRUCT { ?s a cms:Bookmark } WHERE { ?s dc:identifier ?u }"
                            .as_bytes()
                            .to_vec(),
                    ),
                )
                .with_arg("graph", ArgRef::Inline(b"urn:cms:graph:bookmarks".to_vec()))
                .with_arg("as", ArgRef::Inline(b"turtle".to_vec())),
            )
            .await?;
        let books = inv
            .issue(Request::new(
                Verb::Source,
                Iri::parse("urn:cms:graph:books").expect("valid IRI"),
            ))
            .await?;
        // Presentations already carry `a cms:Presentation` (typed at the source), so unlike
        // bookmarks they need no separate type-construct. Empty when no root is configured.
        let presentations = inv
            .issue(Request::new(
                Verb::Source,
                Iri::parse("urn:cms:graph:presentations").expect("valid IRI"),
            ))
            .await?;
        let mut turtle = bookmarks.bytes;
        turtle.push(b'\n');
        turtle.extend_from_slice(&bookmark_types.bytes);
        turtle.push(b'\n');
        turtle.extend_from_slice(&books.bytes);
        turtle.push(b'\n');
        turtle.extend_from_slice(&presentations.bytes);
        Ok(Representation::new(
            ReprType::new("text/turtle").with_param("charset", "utf-8"),
            turtle,
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-graph"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:graph")
            .summary(
                "The whole CMS graph as RDF/Turtle — bookmarks ⊕ Zotero books on one \
                 dc:subject/dc:title axis. Point `urn:sparql:* graph=urn:cms:graph` at it.",
            )
            .verb(Verb::Source)
    }
}

/// Escape a value riding into a SPARQL double-quoted string literal.
fn sparql_lit(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `cms:` class for a content-type slug (`book` | `bookmark` | `presentation`), or
/// `None` for an unknown kind. **The single source of truth for the type↔class map** — both
/// the type view and a tag view's `type` scope resolve through it, so they can't disagree.
/// (They did: a missing `presentation` arm made a scoped tag view silently drop its filter
/// while the header still said "Presentations".)
///
/// ADDING A CONTENT TYPE means updating, in lockstep:
/// 1. this map, plus a graph source that types the resource `a cms:{Class}`;
/// 2. `type_label` below (the plural header a type view shows);
/// 3. a **colour rule** `.cms-card[data-kind="{slug}"]` (+ dark variant) in `dist/index.html`.
///    The card *rendering* is generic — every card carries a `cms:kind` slug (see
///    `KIND_CONSTRUCT`) that the stylesheets turn into `data-kind`, so only the colour is
///    per-type; a new type with no rule just shows no accent until you add one.
fn cms_class(ty: &str) -> Option<&'static str> {
    match ty {
        "book" => Some("Book"),
        "bookmark" => Some("Bookmark"),
        "presentation" => Some("Presentation"),
        _ => None,
    }
}

/// The human display label for a content-type slug — the plural, capitalized header a type view
/// (or a tag's type scope) shows: `book` → `Books`. Kept beside `cms_class`, in lockstep, so the
/// header can't drift from the filter. Unknown slugs never reach here — the type view rejects
/// them before a facet is built — so the fallback is only a total-match formality.
fn type_label(ty: &str) -> &'static str {
    match ty {
        "book" => "Books",
        "bookmark" => "Bookmarks",
        "presentation" => "Presentations",
        _ => "Items",
    }
}

/// Emit each card's kind as a lowercased slug (`cms:kind "presentation"`) so the stylesheets
/// can tag it `data-kind` for per-type colouring — with no per-type logic in the SPARQL or
/// the XSLT (the slug is derived from whatever `cms:` type the resource carries). Appended to
/// the card views' CONSTRUCT/WHERE. Requires `PREFIX cms:` on the query.
const KIND_CONSTRUCT: &str = " ; cms:kind ?kind";
const KIND_WHERE: &str = " OPTIONAL { ?s a ?kt . \
    FILTER(STRSTARTS(STR(?kt), \"https://ikigai-rs.dev/ns/cms#\")) \
    BIND(LCASE(REPLACE(STR(?kt), \"^.*#\", \"\")) AS ?kind) }";

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

/// How many resources a card view shows per page. A big tag (`#webassembly` has 200+)
/// or a broad type would otherwise render every card in one fragment.
const PAGE_SIZE: usize = 60;

/// The page start from the `offset` arg (0 = first page). Anything unparseable → 0, so a
/// missing/garbage offset just shows page one rather than erroring.
fn page_offset(inv: &Invocation<'_>) -> usize {
    inv.inline_str("offset")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// The `ORDER BY` term for the sort variable from the `dir` arg: `DESC(?v)` for
/// `dir=desc`, else plain `?v` (ascending — the default and every other value). The var is
/// caller-supplied (never client input), so this only ever wraps a fixed variable.
fn order_by(inv: &Invocation<'_>, var: &str) -> String {
    match inv.inline_str("dir") {
        Ok("desc") => format!("DESC({var})"),
        _ => var.to_string(),
    }
}

/// The sort direction arg normalized to `"desc"` or `"asc"` (default) — for the view hx-vals.
fn dir_arg(inv: &Invocation<'_>) -> &'static str {
    if inv.inline_str("dir") == Ok("desc") {
        "desc"
    } else {
        "asc"
    }
}

/// A paginated resource subquery `{ SELECT {vars} … }` yielding the page
/// `[offset, offset+PAGE_SIZE)` of DISTINCT resources matching `body`, ordered by `order`.
///
/// It nests two slices — **OFFSET in the inner query (no LIMIT), LIMIT in the outer (no
/// OFFSET)** — rather than a single `LIMIT n OFFSET m`. Semantically identical, but it dodges
/// an oxigraph bug: sparopt 0.3.6's cardinality estimator computes `length - start` for a
/// `LIMIT/OFFSET` slice and PANICS (`attempt to subtract with overflow`) whenever `m > n` —
/// i.e. any page past the first at a small page size. Split, each slice has only a start OR
/// a length, so the subtraction never runs.
fn paged_subquery(vars: &str, body: &str, order: &str, offset: usize) -> String {
    format!(
        "{{ SELECT {vars} WHERE {{ \
             {{ SELECT DISTINCT {vars} WHERE {{ {body} }} ORDER BY {order} OFFSET {offset} }} \
         }} ORDER BY {order} LIMIT {PAGE_SIZE} }}"
    )
}

/// Count the resources a view's pattern matches (its `SELECT (COUNT(DISTINCT ?s) AS ?n)`),
/// so the pager knows the total and whether a next page exists. Golden-threaded like the
/// page query, so a graph edit refreshes it.
async fn count_resources(inv: &Invocation<'_>, query: String) -> Result<usize> {
    let sel = Iri::parse("urn:sparql:select").expect("valid sparql IRI");
    let out = inv
        .issue(
            Request::new(Verb::Source, sel)
                .with_arg("query", ArgRef::Inline(query.into_bytes()))
                .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec())),
        )
        .await?;
    let json: serde_json::Value =
        serde_json::from_slice(&out.bytes).unwrap_or(serde_json::Value::Null);
    Ok(json["results"]["bindings"][0]["n"]["value"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0))
}

/// The first/prev/next/last pager appended under a page of cards. Each live control is an
/// htmx `hx-get` to `/r/{iri}?offset={target}` (the other args — style/dir/type/q — ride the
/// enclosing view's inherited `hx-vals`, so paging preserves them); an edge that doesn't exist
/// renders as a dimmed span. Empty when it all fits on one page.
fn pager_html(iri: &str, offset: usize, total: usize) -> String {
    if total <= PAGE_SIZE {
        return String::new();
    }
    let start = offset + 1;
    let end = (offset + PAGE_SIZE).min(total);
    let last = (total - 1) / PAGE_SIZE * PAGE_SIZE; // offset of the final page
    let at_start = offset == 0;
    let at_end = end >= total;
    // One pager control: a live htmx button that jumps to `target`, or a dimmed span at an edge.
    let ctl = |avail: bool, target: usize, label: &str| {
        if avail {
            format!(
                "<button class=\"cms-page\" hx-get=\"/r/{iri}?offset={target}\">{label}</button>"
            )
        } else {
            format!("<span class=\"cms-page cms-page-off\">{label}</span>")
        }
    };
    let mut nav = String::from("<nav class=\"cms-pager\">");
    nav.push_str(&ctl(!at_start, 0, "« first"));
    nav.push_str(&ctl(!at_start, offset.saturating_sub(PAGE_SIZE), "‹ prev"));
    nav.push_str(&format!(
        "<span class=\"cms-page-info\">{start}–{end} of {total}</span>"
    ));
    nav.push_str(&ctl(!at_end, offset + PAGE_SIZE, "next ›"));
    nav.push_str(&ctl(!at_end, last, "last »"));
    nav.push_str("</nav>");
    nav
}

/// The identity + args of the view being rendered — its public IRI (for the pager/controls)
/// and the current `style`/`dir`/`type`/`q`. Emitted as an inheritable `hx-vals` on the view
/// wrapper, so every htmx control inside (tag chips, pager) carries the same context without
/// baking it into each link.
struct ViewCtx<'a> {
    iri: &'a str,
    style: &'a str,
    dir: &'a str,
    type_scope: Option<&'a str>,
    q: Option<&'a str>,
    facet: Facet<'a>,
}

/// What the current view is faceted on — drives the context header's label and the target its
/// "clear" control returns to (a tag/search clears to the tag index; a type clears to the type
/// index). Kept as data so the header markup stays free of per-view branching.
enum Facet<'a> {
    Tag(&'a str),
    Search(&'a str),
    Type(&'a str),
}

/// The view context as an HTML-attribute-escaped JSON object for `hx-vals`.
fn view_hxvals(ctx: &ViewCtx<'_>) -> String {
    let mut m = serde_json::Map::new();
    m.insert("style".into(), ctx.style.into());
    m.insert("dir".into(), ctx.dir.into());
    if let Some(t) = ctx.type_scope {
        m.insert("type".into(), t.into());
    }
    if let Some(q) = ctx.q {
        m.insert("q".into(), q.into());
    }
    serde_json::Value::Object(m)
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The context header: what the view is faceted on, plus a `×` control that clears back to the
/// relevant index. Server-rendered so it always reflects the live view; the clear button is a
/// plain htmx `hx-get` (its target inherited from `#room`). A tag opened within a type carries
/// its scope as a trailing note. Markup-only — no computed state — so it lifts straight into an
/// external template later.
fn facet_html(ctx: &ViewCtx<'_>) -> String {
    let (label, clear_iri) = match ctx.facet {
        Facet::Tag(t) => (format!("#{}", html_escape(t)), "urn:cms:tags"),
        Facet::Search(q) => (format!("&ldquo;{}&rdquo;", html_escape(q)), "urn:cms:tags"),
        Facet::Type(t) => (type_label(t).to_string(), "urn:cms:types"),
    };
    let scope = match ctx.type_scope {
        Some(s) if !s.is_empty() => {
            format!(
                "<span class=\"cms-facet-scope\">in {}</span>",
                type_label(s)
            )
        }
        _ => String::new(),
    };
    format!(
        "<div class=\"cms-facet\"><span class=\"cms-facet-label\">{label}</span>{scope}\
         <button class=\"cms-facet-clear\" hx-get=\"/r/{clear_iri}\" title=\"clear\">&times;</button></div>"
    )
}

/// The restyle + sort toolbar. Each control overrides exactly ONE axis (style or dir) via its
/// own `hx-vals` and inherits the rest from the enclosing `.cms-view` wrapper — so restyling
/// keeps the tag/query and resorting keeps the style, with no context baked into the link.
/// Overriding only style/dir (never offset) means both actions land back on page one. The
/// current selection renders inert (a `<span>`), the alternatives as buttons. Markup-only.
fn toolbar_html(ctx: &ViewCtx<'_>) -> String {
    let iri = ctx.iri;
    let seg = |current: bool, overrides: &str, label: &str| -> String {
        if current {
            format!("<span class=\"cms-seg-btn is-active\">{label}</span>")
        } else {
            format!(
                "<button class=\"cms-seg-btn\" hx-get=\"/r/{iri}\" hx-vals='{overrides}'>{label}</button>"
            )
        }
    };
    let mut t = String::from("<div class=\"cms-toolbar\"><div class=\"cms-seg\">");
    t.push_str(&seg(
        ctx.style == "catalog",
        "{\"style\":\"catalog\"}",
        "Catalog",
    ));
    t.push_str(&seg(
        ctx.style == "mosaic",
        "{\"style\":\"mosaic\"}",
        "Mosaic",
    ));
    t.push_str(&seg(
        ctx.style == "agenda",
        "{\"style\":\"agenda\"}",
        "Agenda",
    ));
    t.push_str("</div><div class=\"cms-seg\">");
    t.push_str(&seg(ctx.dir == "asc", "{\"dir\":\"asc\"}", "A\u{2192}Z"));
    t.push_str(&seg(ctx.dir == "desc", "{\"dir\":\"desc\"}", "Z\u{2192}A"));
    t.push_str("</div></div>");
    t
}

/// Minimal HTML-text escaping for the few view-context strings (tag/query/type) that reach the
/// server-rendered chrome. The card bodies go through XSLT; only these header labels are
/// interpolated by hand, so they escape the five markup-significant characters here.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Render one page of a card view: a wrapper carrying the view context as `hx-vals`, the
/// page's CONSTRUCT through the stylesheet, then an htmx pager sized from `count_query`.
/// Shared by the tag, search, and type views — the pagination is uniform, the query differs.
async fn render_page(
    inv: &Invocation<'_>,
    ctx: &ViewCtx<'_>,
    count_query: String,
    construct_query: String,
    offset: usize,
) -> Result<Representation> {
    let total = count_resources(inv, count_query).await?;
    let cards = render(
        inv,
        "urn:sparql:construct",
        construct_query,
        "rdfxml",
        ctx.style,
    )
    .await?;
    // The wrapper's hx-vals is inherited by every control the fragment swaps into #room, so a
    // tag click or a page step keeps the current style/dir/type/q.
    let mut html =
        format!("<div class=\"cms-view\" hx-vals=\"{}\">", view_hxvals(ctx)).into_bytes();
    html.extend_from_slice(facet_html(ctx).as_bytes());
    html.extend_from_slice(toolbar_html(ctx).as_bytes());
    html.extend_from_slice(&cards.bytes);
    html.extend_from_slice(pager_html(ctx.iri, offset, total).as_bytes());
    html.extend_from_slice(b"</div>");
    Ok(Representation::new(
        ReprType::new("text/html").with_param("charset", "utf-8"),
        html,
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
        // Optional `type` scope: a tag click inside a type view stays within that kind.
        // Resolved through `cms_class` (the shared map), so the filter can never disagree
        // with the header the browser shows. An unknown/absent type → no filter (all kinds).
        let type_filter = match inv.inline_str("type").ok().and_then(cms_class) {
            Some(class) => format!("?s a <https://ikigai-rs.dev/ns/cms#{class}> . "),
            None => String::new(),
        };
        let offset = page_offset(inv);
        let order = order_by(inv, "?st");
        let count_query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ {type_filter}?s dc:subject \"{safe}\" }}"
        );
        // Page the *resources* (title-ordered, stable) in a subquery, then join each one's
        // full data — LIMIT/OFFSET on triples would slice a card in half.
        let paged = paged_subquery(
            "?s ?st",
            &format!("{type_filter}?s dc:subject \"{safe}\" ; dc:title ?st"),
            &order,
            offset,
        );
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag ; dc:creator ?c{KIND_CONSTRUCT} }} \
             WHERE {{ {paged} \
                      ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag . OPTIONAL {{ ?s dc:creator ?c }}{KIND_WHERE} }}"
        );
        let view_iri = format!("urn:cms:view:{tag}");
        let ctx = ViewCtx {
            iri: &view_iri,
            style: card_style(inv),
            dir: dir_arg(inv),
            type_scope: inv.inline_str("type").ok(),
            q: None,
            facet: Facet::Tag(tag),
        };
        render_page(inv, &ctx, count_query, query, offset).await
    }

    fn name(&self) -> &str {
        "cms-view"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:view")
            .summary(
                "The reading room for a tag: an htmx HTML fragment of every resource \
                 carrying `dc:subject {tag}`, rendered as cards through a stylesheet \
                 resource. Optional `type` arg (book|bookmark) scopes the tag to a kind. \
                 A view is a query.",
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
        let offset = page_offset(inv);
        let order = order_by(inv, "?t");
        // Match on title-contains; page the resources in a subquery so LIMIT/OFFSET slice
        // *resources*, then join each one's tags for its chips.
        let count_query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ \
                 ?s dc:title ?t . FILTER(CONTAINS(LCASE(?t), LCASE(\"{safe}\"))) }}"
        );
        let paged = paged_subquery(
            "?s ?t ?u",
            &format!("?s dc:title ?t ; dc:identifier ?u . FILTER(CONTAINS(LCASE(?t), LCASE(\"{safe}\")))"),
            &order,
            offset,
        );
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag ; dc:creator ?c{KIND_CONSTRUCT} }} \
             WHERE {{ {paged} \
             OPTIONAL {{ ?s dc:subject ?tag }} OPTIONAL {{ ?s dc:creator ?c }}{KIND_WHERE} }}"
        );
        let ctx = ViewCtx {
            iri: "urn:cms:search",
            style: card_style(inv),
            dir: dir_arg(inv),
            type_scope: None,
            q: Some(q),
            facet: Facet::Search(q),
        };
        render_page(inv, &ctx, count_query, query, offset).await
    }

    fn name(&self) -> &str {
        "cms-search"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:search")
            .summary(
                "Search the room: cards for resources whose dc:title contains the `q` \
                 term (case-insensitive), paged. A view is a query.",
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

/// `urn:cms:type:{type}` — cards for resources of a kind (`book` | `bookmark`), capped at
/// a browse sample; narrow further by tag or search. Renders through the card stylesheets.
struct TypeView;

#[async_trait]
impl Endpoint for TypeView {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let ty = inv
            .bindings
            .get("type")
            .map(|s| s.to_string())
            .ok_or_else(|| Error::MissingArgument("type".to_string()))?;
        // Only the known kinds (never build an arbitrary `cms:{X}` class off a client string).
        let class = cms_class(&ty).ok_or_else(|| Error::Endpoint(format!("no type `{ty}`")))?;
        let offset = page_offset(inv);
        let order = order_by(inv, "?t");
        let count_query = format!(
            "PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
             SELECT (COUNT(DISTINCT ?s) AS ?n) WHERE {{ ?s a cms:{class} }}"
        );
        let paged = paged_subquery(
            "?s ?t ?u",
            &format!("?s a cms:{class} ; dc:title ?t ; dc:identifier ?u ."),
            &order,
            offset,
        );
        let query = format!(
            "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
             PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
             CONSTRUCT {{ ?s dc:title ?t ; dc:identifier ?u ; dc:subject ?tag ; dc:creator ?c{KIND_CONSTRUCT} }} \
             WHERE {{ {paged} \
             OPTIONAL {{ ?s dc:subject ?tag }} OPTIONAL {{ ?s dc:creator ?c }}{KIND_WHERE} }}"
        );
        let view_iri = format!("urn:cms:type:{ty}");
        let ctx = ViewCtx {
            iri: &view_iri,
            style: card_style(inv),
            dir: dir_arg(inv),
            type_scope: None,
            q: None,
            facet: Facet::Type(&ty),
        };
        render_page(inv, &ctx, count_query, query, offset).await
    }

    fn name(&self) -> &str {
        "cms-type"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:type")
            .summary(
                "Cards for resources of a kind (book | bookmark), paged. Narrow further by \
                 tag or search. A view is a query.",
            )
            .verb(Verb::Source)
    }
}

/// `urn:cms:types` — the type index: each kind (`book`, `bookmark`) with a count, a
/// clickable chip opening its type view. Like the tag index, navigation is a query.
struct TypesIndex;

#[async_trait]
impl Endpoint for TypesIndex {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        // CMS content kinds only (cms:Book, cms:Bookmark) — a source's schema triples can
        // leave stray rdf:Property/rdfs:Class/owl:Ontology types in the graph.
        let query = "PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
             SELECT ?label (COUNT(?s) AS ?n) WHERE { \
               ?s a ?type . \
               FILTER(STRSTARTS(STR(?type), \"https://ikigai-rs.dev/ns/cms#\")) \
               BIND(LCASE(REPLACE(STR(?type), \"^.*#\", \"\")) AS ?label) \
             } GROUP BY ?label ORDER BY DESC(?n)"
            .to_string();
        render(inv, "urn:sparql:select", query, "xml", "types").await
    }

    fn name(&self) -> &str {
        "cms-types"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:types")
            .summary(
                "The type index: each kind (book, bookmark) with a count, a clickable chip \
                 that opens its type view. A view is a query.",
            )
            .verb(Verb::Source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ikigai_resolve::Resolver;

    /// A minimal Zotero RDF library: one book with an author and a tag, in the exact
    /// shape the real export uses (relative `#item` IRI, Seq of foaf:Person, AutomaticTag).
    const ZOTERO_FIXTURE: &str = r##"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
  xmlns:bib="http://purl.org/net/biblio#"
  xmlns:dc="http://purl.org/dc/elements/1.1/"
  xmlns:z="http://www.zotero.org/namespaces/export#"
  xmlns:foaf="http://xmlns.com/foaf/0.1/">
  <bib:Book rdf:about="#item_1">
    <z:itemType>book</z:itemType>
    <dc:title>Rust in Action</dc:title>
    <bib:authors><rdf:Seq><rdf:li><foaf:Person>
      <foaf:surname>McNamara</foaf:surname><foaf:givenName>Tim</foaf:givenName>
    </foaf:Person></rdf:li></rdf:Seq></bib:authors>
    <dc:subject><z:AutomaticTag><rdf:value>Rust</rdf:value></z:AutomaticTag></dc:subject>
  </bib:Book>
</rdf:RDF>"##;

    /// Write a tiny bookmarks fixture (and optionally a Zotero library) at the exact paths
    /// the kernel expects, then prove the full spine: fs read → transrept →
    /// `urn:cms:graph` → SPARQL.
    fn fixture_kernel(with_books: bool) -> (tempfile::TempDir, Kernel) {
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
        let zotero = with_books.then(|| {
            let z = dir.path().join("zotero.rdf");
            std::fs::write(&z, ZOTERO_FIXTURE).unwrap();
            z
        });
        let kernel = build_cms_kernel(dir.path().to_path_buf(), zotero);
        (dir, kernel)
    }

    fn kernel_over_fixture() -> (tempfile::TempDir, Kernel) {
        fixture_kernel(false)
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
        // External bookmark links open in a new tab so the reading room (and its session)
        // is never left — clicking a bookmark must not blow away the passkey login.
        assert!(
            html.contains("target='_blank'"),
            "title opens a new tab: {html}"
        );
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
    fn a_card_view_carries_a_context_header_and_a_restyle_sort_toolbar() {
        let (_dir, kernel) = kernel_over_fixture();
        let html = resolve_html(&kernel, "urn:cms:view:quic", &[("style", "mosaic")]);
        // Context header: the tag label + a clear control back to the tag index.
        assert!(
            html.contains("class=\"cms-facet-label\">#quic<"),
            "facet shows the tag: {html}"
        );
        assert!(
            html.contains("class=\"cms-facet-clear\" hx-get=\"/r/urn:cms:tags\""),
            "clear returns to the tag index: {html}"
        );
        // Toolbar: the current style is inert; an alternative overrides only `style` via its own
        // hx-vals (dir/type/q ride the inherited wrapper), and re-requests the same view IRI.
        assert!(
            html.contains("<span class=\"cms-seg-btn is-active\">Mosaic</span>"),
            "the active style is inert: {html}"
        );
        assert!(
            html.contains(
                "<button class=\"cms-seg-btn\" hx-get=\"/r/urn:cms:view:quic\" \
                 hx-vals='{\"style\":\"catalog\"}'>Catalog</button>"
            ),
            "restyle overrides only style: {html}"
        );
        // Sort: default asc is active; Z→A overrides only `dir`.
        assert!(
            html.contains("<span class=\"cms-seg-btn is-active\">A\u{2192}Z</span>"),
            "ascending is the default active sort: {html}"
        );
        assert!(
            html.contains("hx-vals='{\"dir\":\"desc\"}'>Z\u{2192}A</button>"),
            "resort overrides only dir: {html}"
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
    fn deep_pagination_past_the_page_size_does_not_panic() {
        // Regression: a single `LIMIT 60 OFFSET 120` panicked oxigraph's sparopt optimizer
        // (`length - start` underflow) — so page 3+ crashed the connection. paged_subquery
        // splits OFFSET/LIMIT across two slices to dodge it.
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        let n = PAGE_SIZE * 2 + 10; // 130 → page 3 (offset 120) holds 10
        let mut org = String::from("* Bookmarks\n");
        for i in 0..n {
            org.push_str(&format!(
                "** [[https://ex.com/{i:03}][Item {i:03}]]\n   :PROPERTIES:\n   :TAGS: deep\n   :END:\n"
            ));
        }
        std::fs::write(&bm, org).unwrap();
        let kernel = build_cms_kernel(dir.path().to_path_buf(), None);

        let off = (PAGE_SIZE * 2).to_string();
        let p3 = resolve_html(
            &kernel,
            "urn:cms:view:deep",
            &[("style", "catalog"), ("offset", &off)],
        );
        assert_eq!(
            p3.matches("class='cms-card'").count(),
            n - PAGE_SIZE * 2,
            "page 3 is the 10-item remainder (and did not panic)"
        );
        assert!(
            p3.contains(&format!("121–{n} of {n}")),
            "range on page 3: {p3}"
        );
        assert!(
            p3.contains("Item 120") && !p3.contains("Item 119"),
            "correct slice: {p3}"
        );
    }

    #[test]
    fn the_pager_reflects_position_in_the_result_set() {
        let iri = "urn:cms:view:x";
        // Fits on one page → no pager at all.
        assert_eq!(pager_html(iri, 0, PAGE_SIZE), "");
        assert_eq!(pager_html(iri, 0, 5), "");
        // First page of many: first+prev dimmed, next+last live (htmx hx-get), range shown.
        let first = pager_html(iri, 0, 200);
        assert!(
            first.contains("cms-page cms-page-off\">« first")
                && first.contains("cms-page cms-page-off\">‹ prev"),
            "first + prev disabled on page 1: {first}"
        );
        assert!(
            first.contains(&format!("hx-get=\"/r/{iri}?offset={PAGE_SIZE}\">next")),
            "next jumps a page over htmx: {first}"
        );
        // last-page offset for 200 items at size 60 → 180.
        assert!(
            first.contains(&format!("hx-get=\"/r/{iri}?offset=180\">last »")),
            "last jumps to the final page: {first}"
        );
        assert!(first.contains(&format!("1–{PAGE_SIZE} of 200")), "{first}");
        // A middle page: every edge live (first→0, prev→0, next→120, last→180).
        let mid = pager_html(iri, PAGE_SIZE, 200);
        assert!(
            mid.contains(&format!("hx-get=\"/r/{iri}?offset=0\">« first")),
            "{mid}"
        );
        assert!(
            mid.contains(&format!("hx-get=\"/r/{iri}?offset=0\">‹ prev")),
            "{mid}"
        );
        assert!(
            mid.contains(&format!(
                "hx-get=\"/r/{iri}?offset={}\">next",
                PAGE_SIZE * 2
            )),
            "{mid}"
        );
        assert!(
            mid.contains(&format!("hx-get=\"/r/{iri}?offset=180\">last »")),
            "{mid}"
        );
        // The last page: next+last dimmed, first live.
        let last = pager_html(iri, 180, 200);
        assert!(
            last.contains("cms-page cms-page-off\">next")
                && last.contains("cms-page cms-page-off\">last »"),
            "next + last disabled on the last page: {last}"
        );
        assert!(
            last.contains(&format!("hx-get=\"/r/{iri}?offset=0\">« first")),
            "first live: {last}"
        );
        assert!(last.contains("181–200 of 200"), "{last}");
    }

    #[test]
    fn a_large_tag_view_pages_through_its_resources() {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        let n = PAGE_SIZE + 5; // 65 → two pages, a short second one
        let mut org = String::from("* Bookmarks\n");
        for i in 0..n {
            org.push_str(&format!(
                "** [[https://ex.com/{i:03}][Item {i:03}]]\n   :PROPERTIES:\n   :TAGS: paged\n   :END:\n"
            ));
        }
        std::fs::write(&bm, org).unwrap();
        let kernel = build_cms_kernel(dir.path().to_path_buf(), None);

        // Page 1: a full page of cards; the pager offers next but not prev.
        let p1 = resolve_html(&kernel, "urn:cms:view:paged", &[("style", "catalog")]);
        assert_eq!(
            p1.matches("class='cms-card'").count(),
            PAGE_SIZE,
            "page 1 is a full page"
        );
        assert!(
            p1.contains(&format!("1–{PAGE_SIZE} of {n}")),
            "range on page 1: {p1}"
        );
        assert!(
            p1.contains(&format!(
                "hx-get=\"/r/urn:cms:view:paged?offset={PAGE_SIZE}\">next"
            )),
            "next offered over htmx: {p1}"
        );
        assert!(
            p1.contains("cms-page cms-page-off\">‹ prev"),
            "no prev on page 1"
        );

        // Page 2: the remainder; a prev, no next.
        let off = PAGE_SIZE.to_string();
        let p2 = resolve_html(
            &kernel,
            "urn:cms:view:paged",
            &[("style", "catalog"), ("offset", &off)],
        );
        assert_eq!(
            p2.matches("class='cms-card'").count(),
            n - PAGE_SIZE,
            "page 2 is the remainder"
        );
        assert!(
            p2.contains("hx-get=\"/r/urn:cms:view:paged?offset=0\">‹ prev"),
            "prev back to page 1: {p2}"
        );
        // The fragment carries the view context as an inherited hx-vals wrapper.
        assert!(
            p2.contains("class=\"cms-view\" hx-vals="),
            "context wrapper: {p2}"
        );
        assert!(
            p2.contains("cms-page cms-page-off\">next"),
            "no next on the last page"
        );
        // Title-ordered, non-overlapping: the first item is only on page 1.
        assert!(
            p1.contains("Item 000") && !p2.contains("Item 000"),
            "pages must not overlap"
        );
    }

    #[test]
    fn a_view_sorts_by_title_ascending_or_descending() {
        let dir = tempfile::tempdir().unwrap();
        let bm = dir.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n\
             ** [[https://a.example][Alpha]]\n   :PROPERTIES:\n   :TAGS: sorted\n   :END:\n\
             ** [[https://z.example][Zulu]]\n   :PROPERTIES:\n   :TAGS: sorted\n   :END:\n\
             ** [[https://m.example][Mike]]\n   :PROPERTIES:\n   :TAGS: sorted\n   :END:\n",
        )
        .unwrap();
        let kernel = build_cms_kernel(dir.path().to_path_buf(), None);

        // Ascending (the default): Alpha renders before Zulu.
        let asc = resolve_html(
            &kernel,
            "urn:cms:view:sorted",
            &[("style", "catalog"), ("dir", "asc")],
        );
        assert!(
            asc.find("Alpha").unwrap() < asc.find("Zulu").unwrap(),
            "asc puts Alpha before Zulu"
        );
        // Descending: the same graph, order flipped.
        let desc = resolve_html(
            &kernel,
            "urn:cms:view:sorted",
            &[("style", "catalog"), ("dir", "desc")],
        );
        assert!(
            desc.find("Zulu").unwrap() < desc.find("Alpha").unwrap(),
            "desc puts Zulu before Alpha"
        );
    }

    #[test]
    fn presentations_are_browsable_by_type_and_venue_tag() {
        // A minimal bookmarks source so the union graph assembles.
        let src = tempfile::tempdir().unwrap();
        let bm = src.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n** [[https://x][X]]\n   :PROPERTIES:\n   :TAGS: misc\n   :END:\n",
        )
        .unwrap();
        // A deck under a venue path → a free `#uberconf` tag, plus an authored topic tag
        // from its built JSON-LD (`genomics`). The title slide's H1 supplies the title; its
        // `.tag` span is deliberately NOT read (that's presentation, not semantics).
        let pres = tempfile::tempdir().unwrap();
        let deck = pres.path().join("conferences/nfjs/uberconf/2026/quant-bio");
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::create_dir_all(deck.join("dist")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        std::fs::write(
            deck.join("slides/00-title.md"),
            "# Quantitative Biology\n\n<span class=\"tag ink\">ignored-span</span>\n",
        )
        .unwrap();
        std::fs::write(
            deck.join("dist/index.html"),
            "<script type=\"application/ld+json\">{\
             \"@context\":{\"dc\":\"http://purl.org/dc/elements/1.1/\",\
             \"cms\":\"https://ikigai-rs.dev/ns/cms#\",\"title\":\"dc:title\",\
             \"tags\":{\"@id\":\"dc:subject\",\"@container\":\"@set\"}},\
             \"@type\":\"cms:Presentation\",\"title\":\"Quantitative Biology\",\
             \"tags\":[\"genomics\"]}</script>",
        )
        .unwrap();

        let kernel = build_cms_kernel_with(
            src.path().to_path_buf(),
            None,
            Some(crate::presentations::Presentations {
                root: pres.path().to_path_buf(),
                base_url: None,
            }),
            None,
        );

        // The type facet lists the deck as a Presentation.
        let by_type = resolve_html(
            &kernel,
            "urn:cms:type:presentation",
            &[("style", "catalog")],
        );
        assert!(
            by_type.contains("Quantitative Biology"),
            "deck in the presentations type view: {by_type}"
        );
        // The venue tag reaches it — the Uberconf navigation path, free from the directory.
        let by_venue = resolve_html(&kernel, "urn:cms:view:uberconf", &[("style", "catalog")]);
        assert!(
            by_venue.contains("Quantitative Biology"),
            "deck reachable by #uberconf: {by_venue}"
        );
        // An authored (JSON-LD) topic tag reaches it; the ignored `.tag` span does not.
        let by_tag = resolve_html(&kernel, "urn:cms:view:genomics", &[("style", "catalog")]);
        assert!(
            by_tag.contains("Quantitative Biology"),
            "deck reachable by its authored JSON-LD tag: {by_tag}"
        );
        let by_span = resolve_html(
            &kernel,
            "urn:cms:view:ignored-span",
            &[("style", "catalog")],
        );
        assert!(
            !by_span.contains("Quantitative Biology"),
            "the .tag span must NOT be a browsable tag: {by_span}"
        );
    }

    #[test]
    fn a_tag_view_scoped_to_a_type_filters_to_that_type() {
        // A bookmark and a presentation share the tag "science". A scoped tag view must
        // show only the scoped kind — the label↔query bug was `type=presentation` silently
        // dropping the filter (showing the bookmark too) while the header said "Presentations".
        let src = tempfile::tempdir().unwrap();
        let bm = src.path().join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n** [[https://sci.example][Science Bookmark]]\n   \
             :PROPERTIES:\n   :TAGS: science\n   :END:\n",
        )
        .unwrap();
        let pres = tempfile::tempdir().unwrap();
        let deck = pres.path().join("conferences/nfjs/sci-talk");
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::create_dir_all(deck.join("dist")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        std::fs::write(deck.join("slides/00-title.md"), "# Science Talk\n").unwrap();
        std::fs::write(
            deck.join("dist/index.html"),
            "<script type=\"application/ld+json\">{\
             \"@context\":{\"dc\":\"http://purl.org/dc/elements/1.1/\",\
             \"cms\":\"https://ikigai-rs.dev/ns/cms#\",\"title\":\"dc:title\",\
             \"tags\":{\"@id\":\"dc:subject\",\"@container\":\"@set\"}},\
             \"@type\":\"cms:Presentation\",\"title\":\"Science Talk\",\
             \"tags\":[\"science\"]}</script>",
        )
        .unwrap();

        let kernel = build_cms_kernel_with(
            src.path().to_path_buf(),
            None,
            Some(crate::presentations::Presentations {
                root: pres.path().to_path_buf(),
                base_url: None,
            }),
            None,
        );

        // Unscoped: both the bookmark and the presentation.
        let all = resolve_html(&kernel, "urn:cms:view:science", &[("style", "catalog")]);
        assert!(
            all.contains("Science Bookmark") && all.contains("Science Talk"),
            "unscoped shows both: {all}"
        );
        // Each card carries its kind (cms:kind → data-kind) for per-type colouring.
        assert!(
            all.contains("data-kind='presentation'") && all.contains("data-kind='bookmark'"),
            "cards are tagged by kind: {all}"
        );
        // Scoped to presentation: ONLY the presentation (the fix).
        let as_pres = resolve_html(
            &kernel,
            "urn:cms:view:science",
            &[("style", "catalog"), ("type", "presentation")],
        );
        assert!(
            as_pres.contains("Science Talk"),
            "presentation present: {as_pres}"
        );
        assert!(
            !as_pres.contains("Science Bookmark"),
            "the bookmark is filtered out when scoped to presentation: {as_pres}"
        );
        // Scoped to bookmark: only the bookmark.
        let as_bm = resolve_html(
            &kernel,
            "urn:cms:view:science",
            &[("style", "catalog"), ("type", "bookmark")],
        );
        assert!(
            as_bm.contains("Science Bookmark") && !as_bm.contains("Science Talk"),
            "bookmark scope shows only the bookmark: {as_bm}"
        );
    }

    #[test]
    fn the_bookmarks_source_path_is_overridable() {
        // A bookmarks file at a NON-default sub-path (CMS_BOOKMARKS points here).
        let src = tempfile::tempdir().unwrap();
        let bm = src.path().join("custom/my-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(
            &bm,
            "* Bookmarks\n** [[https://ovr.example][Override Works]]\n   \
             :PROPERTIES:\n   :TAGS: overridden\n   :END:\n",
        )
        .unwrap();
        // Nothing at the default `old-org/pinboard-bookmarks.org`; the override supplies it.
        let kernel = build_cms_kernel_with(
            src.path().to_path_buf(),
            None,
            None,
            Some("custom/my-bookmarks.org".to_string()),
        );
        let html = resolve_html(&kernel, "urn:cms:view:overridden", &[("style", "catalog")]);
        assert!(
            html.contains("Override Works"),
            "reads the overridden bookmarks path: {html}"
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

    fn render_via_xslt(kernel: &Kernel, content: &str, style: &str) -> String {
        let request = Request::new(Verb::Source, Iri::parse("urn:xslt:transform").unwrap())
            .with_arg("content", ArgRef::Inline(content.as_bytes().to_vec()))
            .with_arg(
                "stylesheet",
                ArgRef::Inline(format!("urn:cms:style:{style}").into_bytes()),
            );
        let (repr, _status) = Resolver::issue(kernel, request).expect("xslt resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    #[test]
    fn the_recent_stylesheet_renders_the_trail_and_the_empty_state() {
        let (_dir, kernel) = kernel_over_fixture();
        // A trail item (custom urn:cms:recent# namespace) → a clickable row re-opening it.
        let html = render_via_xslt(
            &kernel,
            "<recent xmlns=\"urn:cms:recent#\"><item iri=\"urn:cms:view:quic\">#quic</item></recent>",
            "recent",
        );
        assert!(html.contains("urn:cms:view:quic"), "row links back: {html}");
        assert!(html.contains("#quic"), "row is labeled: {html}");
        // The empty trail is its own element → a hint, no conditionals needed.
        let empty = render_via_xslt(
            &kernel,
            "<recent xmlns=\"urn:cms:recent#\"><empty/></recent>",
            "recent",
        );
        assert!(empty.contains("Nothing viewed yet"), "empty hint: {empty}");
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

    #[test]
    fn books_join_the_graph_typed_and_render_with_authors() {
        let (_dir, kernel) = fixture_kernel(true);
        // Zotero → normalized into the unified graph as cms:Book with title + creator.
        let json = select(
            &kernel,
            "SELECT ?t ?c WHERE { \
               ?b a <https://ikigai-rs.dev/ns/cms#Book> ; \
                  <http://purl.org/dc/elements/1.1/title> ?t ; \
                  <http://purl.org/dc/elements/1.1/creator> ?c }",
        );
        assert!(
            json.contains("Rust in Action"),
            "book title in graph: {json}"
        );
        assert!(json.contains("McNamara"), "author in graph: {json}");
        // Found by title search and rendered with the author line (books have a lookup
        // dc:identifier, so they ride the same card path as bookmarks).
        let html = resolve_html(&kernel, "urn:cms:search", &[("q", "rust in action")]);
        assert!(
            html.contains("Rust in Action"),
            "book found by search: {html}"
        );
        assert!(
            html.contains("class='cms-author'"),
            "author rendered: {html}"
        );
        assert!(html.contains("McNamara"), "author name shown: {html}");
        // The lookup rides as a string literal (like a bookmark URL), so it lands in the
        // href — an empty href would reload the room and drop the session.
        assert!(
            html.contains("href='https://openlibrary.org/search"),
            "book title has a real lookup href, not empty: {html}"
        );
        // Its Zotero tag (Rust → slug `rust`) joins the shared tag axis, so the book
        // shows up under a tag view alongside any bookmarks.
        let tagview = view(&kernel, "rust", "catalog");
        assert!(
            tagview.contains("Rust in Action"),
            "book under its slug tag: {tagview}"
        );
    }

    #[test]
    fn without_a_library_the_graph_is_bookmarks_only() {
        let (_dir, kernel) = fixture_kernel(false);
        let json = select(
            &kernel,
            "SELECT (COUNT(?b) AS ?n) WHERE { ?b a <https://ikigai-rs.dev/ns/cms#Book> }",
        );
        assert!(json.contains("\"0\""), "no books without a library: {json}");
    }

    #[test]
    fn the_type_facet_indexes_and_browses_kinds() {
        let (_dir, kernel) = fixture_kernel(true);
        // Bookmarks are typed now too (books already were), so both kinds are explicit.
        let json = select(
            &kernel,
            "SELECT (COUNT(?s) AS ?n) WHERE { ?s a <https://ikigai-rs.dev/ns/cms#Bookmark> }",
        );
        assert!(!json.contains("\"0\""), "bookmarks are typed: {json}");
        // The type index lists both kinds, each linking to its type view.
        let idx = resolve_html(&kernel, "urn:cms:types", &[]);
        assert!(idx.contains("urn:cms:type:book"), "book chip: {idx}");
        assert!(
            idx.contains("urn:cms:type:bookmark"),
            "bookmark chip: {idx}"
        );
        // A type view renders cards of that kind — the book view shows authors.
        let books = resolve_html(&kernel, "urn:cms:type:book", &[]);
        assert!(
            books.contains("class=\"cms-facet-label\">Books<"),
            "type header is the capitalized plural label: {books}"
        );
        assert!(
            books.contains("Rust in Action"),
            "book in book view: {books}"
        );
        assert!(
            books.contains("class='cms-author'"),
            "book view shows authors: {books}"
        );
        let bookmarks = resolve_html(&kernel, "urn:cms:type:bookmark", &[]);
        assert!(
            bookmarks.contains("quicwg.org"),
            "bookmark in bookmark view: {bookmarks}"
        );
        // An unknown kind is rejected (no arbitrary cms:{X} class off a client string).
        let iri = Iri::parse("urn:cms:type:widget").unwrap();
        assert!(
            Resolver::issue(&kernel, Request::new(Verb::Source, iri)).is_err(),
            "unknown type rejected"
        );
    }

    #[test]
    fn a_tag_view_scopes_to_a_type() {
        let (_dir, kernel) = fixture_kernel(true);
        // The fixture book "Rust in Action" is tagged rust; no bookmark is.
        let in_books = resolve_html(&kernel, "urn:cms:view:rust", &[("type", "book")]);
        assert!(
            in_books.contains("Rust in Action"),
            "rust scoped to books shows the book: {in_books}"
        );
        let in_bookmarks = resolve_html(&kernel, "urn:cms:view:rust", &[("type", "bookmark")]);
        assert!(
            !in_bookmarks.contains("Rust in Action"),
            "rust scoped to bookmarks excludes the book: {in_bookmarks}"
        );
    }
}
