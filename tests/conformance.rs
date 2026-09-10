//! The module recipe as one test: `ikigai-conformance` walks every endpoint the
//! reading room's kernel binds and reports every violation at once.
//!
//! ## The fixture is a scratch room, never the live one
//!
//! `build_cms_kernel_with` over one tempdir: a two-bookmark org file in a scratch source
//! jail, a one-book Zotero export, one lectern deck under a scratch presentations root, and
//! the four overlays in a scratch store — each seeded with one row, so every Turtle face
//! the walk parses carries a triple (a face that parses to nothing passes SKOLEM-RDF and
//! VOCABULARY without seeing anything: the suite's PENDING #26). Nothing here reads the
//! config home, the data home, or `cms.toml`; the WebTransport/passkey door is out of
//! scope — this file tests the kernel, not the server.
//!
//! ## What the walk sees, and what this file waives
//!
//! The kernel composes four other crates' endpoints (`file` from ikigai-fs, `bookmarks`
//! from ikigai-cms, `sparql-*` from ikigai-sparql, `xslt-transform` from ikigai-xslt), and
//! the suite walks everything bound (PENDING #17). Each of those adopted the suite in its
//! own repo, but the adopted releases are not on crates.io yet, so at the pinned versions
//! they report findings this crate cannot fix. [`assert_findings`] therefore holds this
//! module's own ids to ZERO findings other than NAMES, and every other finding to the pinned
//! [`INHERITED`] set — a finding against an id neither list knows is red. When the
//! dependencies are bumped to their conformant releases, the inherited count in the printed
//! report drops on its own.
//!
//! NAMES: every id here is a full IRI (`urn:cms:graph`), which is not kebab-case. Not
//! renamed — the ids are live MCP tool names, renamed in one coordinated pass (wave two,
//! `ikigai-core-PENDING.md` §1). The suite cannot opt one id out of NAMES (PENDING #6), so
//! the check runs and [`assert_findings`] pins the NAMES findings to exactly the full-IRI ids.
//!
//! Declared to the suite, stated once:
//!
//! - `cacheable` on `urn:cms:graph:bookmarks`, `urn:cms:graph:books`,
//!   `urn:cms:graph:presentations` and `cms-style`: each marks `.cacheable()`, and the
//!   declaration turns a sub-resolution that silently downgraded the effective expiry into
//!   a red test. THAT is the ~2000× incident (#71): an uncacheable overlay joined into the
//!   books graph took a read from ~20µs to ~1.0s and 68 tests passed. The join belongs in
//!   `urn:cms:graph`, the union that is live BY DESIGN — see
//!   [`the_books_graph_stays_cached_beside_the_live_union`].
//! - `pure` on `cms-style` (stylesheets embedded at build time) and on ikigai-cms's
//!   `bookmarks` (a by-value transreptor; its own adoption declares it pure).
//! - `opt_out` of `urn:cms:src:zotero`'s Source: Zotero's own export served verbatim, so
//!   its blank nodes and `bib:`/`z:` terms are the export's — skolemized onto `cms:Book`
//!   by `urn:cms:graph:books`. The opt-out also drops ENFORCED and CACHEABLE for it
//!   (PENDING #21), so [`the_zotero_source_is_cached_under_its_own_thread`] pins both.
//! - Two namespaces: `cms:` (`https://ikigai-rs.dev/ns/cms#`), the vocabulary this crate
//!   coins for the room (named in the README), and Dublin Core Elements 1.1, which the
//!   suite does not list as well-known (PENDING #102).
//!
//! ## What the suite cannot hold and this file pins by hand
//!
//! - **The books graph is cached; the union is live; the overlays join the union**
//!   ([`the_books_graph_stays_cached_beside_the_live_union`]): `Expiry::Never` with the
//!   library thread, a cache hit, and — after a tag overlay write — still a hit while the
//!   union reflects the write. Timing-free; a future join into the books graph fails here.
//! - **The library thread is real** ([`a_cut_on_the_library_thread_recomputes_the_books_graph`]):
//!   `urn:cms:src:zotero` never carried a thread before, so a re-export was invisible until
//!   a restart; a cut now recomputes, and the suite's second resolution is the hit, never a
//!   recomputation (PENDING #64).
//! - **Every Sink declares `content` and its write scope, and is `Denied` without a grant**
//!   ([`every_sink_declares_content_and_is_denied_without_a_grant`]) — including the
//!   pipe form actually promoting a tag.
//! - **Declared outputs are the media types served** ([`declared_outputs_are_the_media_types_served`]):
//!   0.1.0 compares nothing that is not an RDF face (PENDING #11/#31/#79).
//! - **The union's Turtle face, in full** ([`the_union_is_skolemized_on_the_cms_vocabulary`]):
//!   VOCABULARY sees predicates and `rdf:type` objects only (PENDING #96); subjects and the
//!   exact `cms:` terms the room emits are checked here.
//! - **Required is required** ([`required_is_required`]): PENDING #49/#99.
//! - **The fixture id is the description id** ([`the_fixture_id_is_the_description_id`]):
//!   a `Fixture` matching no description is silently inert (PENDING #57).
//!
//! Under `--features maintenance` the serving kernel also binds the review, the purges and
//! the per-link actions, and [`the_maintenance_kernel_conforms`] walks the pass kernel
//! (link-check, tag-suggest, zotero-links) over a canned transport and a scratch keystore.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ikigai_cms_web::tagstore::TagPaths;
use ikigai_cms_web::{build_cms_kernel_with, Presentations};
use ikigai_conformance::{rdf, Check, Fixture, Report, Suite};
use ikigai_core::{
    ArgRef, Capability, Error, Expiry, Iri, Kernel, Representation, Request, Result, Verb,
};

/// The vocabulary this crate coins for the room, registered with the suite.
const CMS: &str = "https://ikigai-rs.dev/ns/cms#";
/// Dublin Core Elements 1.1: what the bookmarks and the Zotero export speak.
const DC: &str = "http://purl.org/dc/elements/1.1/";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

const FS_READ: &str = "urn:cap:fs:read:*";
const FS_WRITE: &str = "urn:cap:fs:write:*";

/// The bookmarks file, as a sub-path of the scratch jail (overriding the crate's default).
const BOOKMARKS: &str = "bookmarks.org";
const BOOKMARKS_IRI: &str = "urn:cms:src:bookmarks.org";
const ZOTERO_IRI: &str = "urn:cms:src:zotero";
/// A file present in BOTH jails (`urn:cms:src:*` and `urn:cms:deck:*`), because the suite
/// applies a `file` fixture's binding to every entry that shares the id (PENDING #2).
const SCRATCH_FILE: &str = "conformance.txt";

/// The bookmarks: two entries, two tags each, so the tag axis has something to join.
const ORG: &str = "\
* Bookmarks
** [[https://webassembly.org][WebAssembly]]
   :PROPERTIES:
   :TAGS: wasm web
   :END:
** [[https://quicwg.org][QUIC Working Group]]
   :PROPERTIES:
   :TAGS: quic networking
   :END:
";

/// A minimal Zotero export in the shape the real one has: a relative `#item` IRI, a Seq of
/// `foaf:Person` (blank nodes — the export's, not ours), an AutomaticTag.
const ZOTERO: &str = r##"<?xml version="1.0"?>
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

/// A second book, appended to prove a cut recomputes the books graph.
const SECOND_BOOK: &str = r##"  <bib:Book rdf:about="#item_2">
    <z:itemType>book</z:itemType>
    <dc:title>Programming Rust</dc:title>
  </bib:Book>
</rdf:RDF>"##;

/// The deck's built JSON-LD: the agreed interface between lectern and the room.
const DECK_HTML: &str = "<script type=\"application/ld+json\">{\
 \"@context\":{\"dc\":\"http://purl.org/dc/elements/1.1/\",\
 \"cms\":\"https://ikigai-rs.dev/ns/cms#\",\"title\":\"dc:title\",\
 \"tags\":{\"@id\":\"dc:subject\",\"@container\":\"@set\"}},\
 \"@type\":\"cms:Presentation\",\"title\":\"Quantitative Biology\",\
 \"tags\":[\"genomics\"]}</script>";
const DECK: &str = "conferences/nfjs/uberconf/2026/quant-bio";
const DECK_IRI: &str = "urn:cms:deck:conferences/nfjs/uberconf/2026/quant-bio/dist/index.html";

/// The room's own description ids, in the serving kernel. A new binding without a line
/// here fails [`conforms`]: the walk's endpoint count is pinned to this list.
const OURS: &[&str] = &[
    "urn:cms:src:zotero",
    "urn:cms:graph:bookmarks",
    "urn:cms:graph:books",
    "urn:cms:graph:zotero-links",
    "urn:cms:graph:presentations",
    "urn:cms:graph:tags-approved",
    "urn:cms:graph:suggestions",
    "urn:cms:graph:dismissed",
    "urn:cms:tag-approve",
    "urn:cms:tag-reject",
    "urn:cms:graph",
    "urn:cms:view",
    "urn:cms:search",
    "urn:cms:tags",
    "urn:cms:type",
    "urn:cms:types",
    "cms-style",
];

/// The maintenance views the serving kernel binds beside the room when the feature is on.
#[cfg(feature = "maintenance")]
const OURS_MAINTENANCE: &[&str] = &[
    "urn:cms:linkstatus",
    "urn:cms:review",
    "urn:cms:purge",
    "urn:cms:purge-domains",
    "urn:cms:purge-unreachable",
    "urn:cms:link-remove",
    "urn:cms:link-keep",
];

/// The dependencies' ids the walk also sees, at the versions the lock pins. Their findings
/// are theirs (each has adopted the suite; the releases are not published yet).
const INHERITED: &[&str] = &[
    "file",
    "bookmarks",
    "sparql-select",
    "sparql-ask",
    "sparql-describe",
    "sparql-construct",
    "xslt-transform",
];

/// The scratch room: the kernel, and the paths a test reaches around it to.
struct Room {
    _dir: tempfile::TempDir,
    kernel: Kernel,
    tags: TagPaths,
    zotero: PathBuf,
}

/// Lay the fixture out under `dir` and return what a kernel needs from it.
fn scratch(dir: &Path) -> (PathBuf, PathBuf, Presentations, TagPaths, PathBuf) {
    let src = dir.join("src");
    std::fs::create_dir_all(&src).expect("src jail");
    std::fs::write(src.join(BOOKMARKS), ORG).expect("bookmarks");
    std::fs::write(src.join(SCRATCH_FILE), "scratch").expect("scratch file");

    let zotero = dir.join("zotero.rdf");
    std::fs::write(&zotero, ZOTERO).expect("zotero export");

    let decks = dir.join("decks");
    let deck = decks.join(DECK);
    std::fs::create_dir_all(deck.join("dist")).expect("deck");
    std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").expect("deck.toml");
    std::fs::write(deck.join("dist/index.html"), DECK_HTML).expect("deck html");
    std::fs::write(decks.join(SCRATCH_FILE), "scratch").expect("scratch file");
    let presentations = Presentations {
        root: decks,
        base_url: Some("https://decks.example".to_string()),
    };

    // The overlays, one row each, so every overlay face carries a triple. The subjects
    // are bookmark skolems the graph will hold (the same FNV-1a of the URL ikigai-cms
    // mints) — any IRI serves the walk; these make the union join.
    let overlays = dir.join("overlays");
    std::fs::create_dir_all(&overlays).expect("overlay store");
    let tags = TagPaths::in_dir(&overlays);
    tags.approve("urn:cms:bookmark:seed-approved", "approved-tag");
    tags.add_suggestion("urn:cms:bookmark:seed-suggested", "suggested-tag");
    tags.dismiss("urn:cms:bookmark:seed-dismissed", "dismissed-tag");
    std::fs::write(
        &tags.zotero_links,
        "<urn:cms:book:seed> <https://ikigai-rs.dev/ns/cms#zoteroItem> <urn:zotero:item:K1> .\n\
         <urn:cms:book:seed> <https://ikigai-rs.dev/ns/cms#zoteroAttachment> <urn:zotero:item:A1> .\n\
         <urn:cms:book:seed> <https://ikigai-rs.dev/ns/cms#readerUrl> \"https://www.zotero.org/u/items/A1/reader\" .\n",
    )
    .expect("zotero link overlay");

    let status = dir.join("cms-linkstatus.json");
    (src, zotero, presentations, tags, status)
}

fn room() -> Room {
    let dir = tempfile::tempdir().expect("tempdir");
    let (src, zotero, presentations, tags, status) = scratch(dir.path());
    let kernel = build_cms_kernel_with(
        src,
        Some(zotero.clone()),
        Some(presentations),
        Some(BOOKMARKS.to_string()),
        tags.clone(),
        Some(status),
    );
    Room {
        _dir: dir,
        kernel,
        tags,
        zotero,
    }
}

/// The suite, configured for this module (the declarations the module docs state).
fn suite() -> Suite {
    Suite::new()
        .namespace(CMS)
        .namespace(DC)
        .fixture(Fixture::new("file", Verb::Source).binding("path", SCRATCH_FILE))
        .pure("bookmarks")
        .pure("cms-style")
        .cacheable("cms-style")
        .cacheable("urn:cms:graph:bookmarks")
        .cacheable("urn:cms:graph:books")
        .cacheable("urn:cms:graph:presentations")
        .opt_out(
            ZOTERO_IRI,
            Some(Verb::Source),
            "Zotero's own export, served verbatim: its blank nodes and bib:/z: terms are the \
             export's, skolemized onto cms:Book by urn:cms:graph:books; cache and thread \
             pinned by hand",
        )
}

fn iri(s: &str) -> Iri {
    Iri::parse(s).unwrap_or_else(|e| panic!("`{s}` is a valid IRI: {e}"))
}

fn request(verb: Verb, target: &str, args: &[(&str, &str)]) -> Request {
    let mut request = Request::new(verb, iri(target));
    for (name, value) in args {
        request = request.with_arg(*name, ArgRef::Inline(value.as_bytes().to_vec()));
    }
    request
}

fn issue(kernel: &Kernel, request: Request, capability: &Capability) -> Result<Representation> {
    futures::executor::block_on(kernel.issue(request, capability))
}

fn resolve(kernel: &Kernel, request: Request) -> Representation {
    issue(kernel, request, &Capability::root()).unwrap_or_else(|e| panic!("resolution failed: {e}"))
}

fn source(kernel: &Kernel, target: &str) -> Representation {
    resolve(kernel, request(Verb::Source, target, &[]))
}

fn text(repr: &Representation) -> String {
    String::from_utf8(repr.bytes.clone()).expect("UTF-8")
}

fn threads(repr: &Representation) -> BTreeSet<String> {
    repr.threads().iter().map(|t| t.to_string()).collect()
}

fn no_grants() -> Capability {
    Capability::scoped(Vec::<String>::new())
}

/// Every id the kernel's catalog describes.
fn walked_ids(kernel: &Kernel) -> BTreeSet<String> {
    kernel
        .entries()
        .expect("an enumerable root")
        .iter()
        .filter(|e| !e.pattern.starts_with("urn:kernel:"))
        .map(|e| {
            kernel
                .describe_pattern(&e.pattern)
                .unwrap_or_else(|| panic!("`{}` describes itself", e.pattern))
                .id
        })
        .collect()
}

/// The walk's verdict on THIS module: no finding against one of `ours` except NAMES,
/// exactly the full-IRI ids under NAMES, every other finding against a pinned dependency
/// id, the endpoint count equal to both lists together, and nothing skipped.
fn assert_findings(report: &Report, kernel: &Kernel, ours: &[&str], inherited: &[&str]) {
    let ours: BTreeSet<&str> = ours.iter().copied().collect();
    let inherited: BTreeSet<&str> = inherited.iter().copied().collect();
    let mut names = BTreeSet::new();
    let mut theirs = 0;
    for finding in &report.findings {
        let id = finding.endpoint.as_str();
        if ours.contains(id) {
            assert_eq!(
                finding.check,
                Check::Names,
                "a finding against this module: {finding}"
            );
            names.insert(id);
        } else {
            assert!(
                inherited.contains(id),
                "a finding against an endpoint this file does not list: {finding}"
            );
            theirs += 1;
        }
    }
    let full_iri: BTreeSet<&str> = ours.iter().copied().filter(|id| id.contains(':')).collect();
    assert_eq!(
        names, full_iri,
        "NAMES is exactly the full-IRI ids, held for the wave-two rename"
    );
    eprintln!(
        "{} inherited finding(s) against {} dependency endpoint(s); {} NAMES finding(s) held \
         for the wave-two rename",
        theirs,
        inherited.len(),
        names.len()
    );

    let expected: BTreeSet<String> = ours.union(&inherited).map(|s| s.to_string()).collect();
    assert_eq!(
        walked_ids(kernel),
        expected,
        "the catalog is exactly the two lists"
    );
    assert_eq!(report.endpoints, expected.len(), "{report}");
    assert_eq!(
        report.checks.skipped().count(),
        0,
        "every check runs: {report}"
    );
}

/// This module's ids in the serving kernel, feature-dependent.
fn ours() -> Vec<&'static str> {
    #[allow(unused_mut)]
    let mut ids = OURS.to_vec();
    #[cfg(feature = "maintenance")]
    ids.extend_from_slice(OURS_MAINTENANCE);
    ids
}

#[test]
fn conforms() {
    let room = room();
    let report = suite().run_blocking(&room.kernel);
    // Printed even when clean (`--nocapture`): the report is the record.
    eprintln!("{report}");
    assert_findings(&report, &room.kernel, &ours(), INHERITED);
}

/// THE ~2000× WITNESS, timing-free. The books graph — the one expensive step, a 4.4 MB
/// export parse in the live room — is `Expiry::Never` under the library's thread and a cache
/// hit; the union `urn:cms:graph` is live (`Expiry::Always`) BY DESIGN, because that is where
/// mutable state joins: the tag overlays, which are rewritten outside any thread. A tag
/// write lands in the union on its next read while the books graph is STILL a hit. Joining an
/// overlay into the books graph instead (what #71 did) makes its effective expiry `Always`,
/// and the `is_cached` assertions here go red — the 68 passing tests of the incident did not.
#[test]
fn the_books_graph_stays_cached_beside_the_live_union() {
    let room = room();
    let kernel = &room.kernel;

    let books = source(kernel, "urn:cms:graph:books");
    assert_eq!(books.expiry, Expiry::Never, "the expensive parse is cached");
    assert_eq!(
        threads(&books),
        BTreeSet::from([ZOTERO_IRI.to_string()]),
        "cached under the library's thread, and nothing else's"
    );
    assert!(kernel.is_cached(
        &request(Verb::Source, "urn:cms:graph:books", &[]),
        &Capability::root()
    ));
    assert!(text(&books).contains("Rust in Action"), "{}", text(&books));

    let bookmarks = source(kernel, "urn:cms:graph:bookmarks");
    assert_eq!(bookmarks.expiry, Expiry::Never);
    assert_eq!(
        threads(&bookmarks),
        BTreeSet::from([BOOKMARKS_IRI.to_string()]),
        "the bookmarks graph is threaded to the org file it reads"
    );

    let presentations = source(kernel, "urn:cms:graph:presentations");
    assert_eq!(presentations.expiry, Expiry::Never);
    assert!(
        threads(&presentations).contains(DECK_IRI),
        "the presentations graph is threaded to the deck files it reads: {:?}",
        threads(&presentations)
    );

    let union = source(kernel, "urn:cms:graph");
    assert_eq!(
        union.expiry,
        Expiry::Always,
        "the union is live: it is where the overlays join"
    );
    assert!(!kernel.is_cached(
        &request(Verb::Source, "urn:cms:graph", &[]),
        &Capability::root()
    ));
    assert!(
        text(&union).contains("approved-tag"),
        "the overlay is in the union"
    );

    // A tag write: the union sees it next read; the books graph is untouched and still a hit.
    let subject = "urn:cms:bookmark:seed-suggested";
    resolve(
        kernel,
        request(
            Verb::Sink,
            "urn:cms:tag-approve",
            &[("book", subject), ("tag", "suggested-tag")],
        ),
    );
    let after = source(kernel, "urn:cms:graph");
    assert!(
        text(&after).contains(&format!("<{subject}> <{DC}subject> \"suggested-tag\"")),
        "the promoted tag is a dc:subject in the union:\n{}",
        text(&after)
    );
    assert!(
        kernel.is_cached(
            &request(Verb::Source, "urn:cms:graph:books", &[]),
            &Capability::root()
        ),
        "an overlay write must not touch the books graph's cache"
    );
    assert_eq!(source(kernel, "urn:cms:graph:books").bytes, books.bytes);
}

/// The thread on `urn:cms:src:zotero` is a real cut point: cutting it recomputes the books
/// graph from the file as it is now. Before this arc the source declared no thread, so the
/// export was served forever and a re-export needed a restart. The suite's second resolution
/// IS the cache hit, so it never sees a recomputation (PENDING #64) — this does.
#[test]
fn a_cut_on_the_library_thread_recomputes_the_books_graph() {
    let room = room();
    let kernel = &room.kernel;
    let books = request(Verb::Source, "urn:cms:graph:books", &[]);

    let first = resolve(kernel, books.clone());
    assert!(!text(&first).contains("Programming Rust"));
    let mut export = ZOTERO.to_string();
    export.truncate(export.len() - "</rdf:RDF>".len());
    export.push_str(SECOND_BOOK);
    std::fs::write(&room.zotero, export).expect("re-export");
    assert_eq!(
        resolve(kernel, books.clone()).bytes,
        first.bytes,
        "no cut: the cached graph is served (the file changed underneath, as a re-export does)"
    );

    kernel.cut(ZOTERO_IRI);
    assert!(!kernel.is_cached(&books, &Capability::root()));
    let second = resolve(kernel, books.clone());
    assert!(
        text(&second).contains("Programming Rust"),
        "recomputed from the re-exported file:\n{}",
        text(&second)
    );
    assert!(
        kernel.is_cached(&books, &Capability::root()),
        "and cached again"
    );
}

/// The opted-out action, by hand: `urn:cms:src:zotero` is cached under its own IRI as the
/// thread (the ikigai-fs convention), a second read is a hit, the face is the declared
/// RDF/XML, and — declaring `urn:cap:fs:read:*` — it is `Denied` under no grants.
#[test]
fn the_zotero_source_is_cached_under_its_own_thread() {
    let room = room();
    let kernel = &room.kernel;
    let spec = kernel
        .describe(&iri(ZOTERO_IRI))
        .expect("the library describes itself")
        .action_specs()
        .into_iter()
        .find(|a| a.verb == Verb::Source)
        .expect("Source is declared");
    assert!(
        spec.requires.iter().any(|r| r == FS_READ),
        "a file read declares the jail's read scope: {:?}",
        spec.requires
    );
    let first = source(kernel, ZOTERO_IRI);
    assert_eq!(first.expiry, Expiry::Never);
    assert_eq!(threads(&first), BTreeSet::from([ZOTERO_IRI.to_string()]));
    assert!(kernel.is_cached(&request(Verb::Source, ZOTERO_IRI, &[]), &Capability::root()));
    assert_eq!(source(kernel, ZOTERO_IRI).bytes, first.bytes);
    assert_eq!(
        rdf::bare_media_type(&first.repr_type.media_type),
        "application/rdf+xml"
    );
    assert!(
        text(&first).contains("xml:base=\"http://zotero.local/\""),
        "the base is injected so `#item_N` resolves"
    );
    match issue(kernel, request(Verb::Source, ZOTERO_IRI, &[]), &no_grants()) {
        Err(Error::Denied(_)) => {}
        other => panic!("a file read under no grants is Denied, got {other:?}"),
    }
}

/// Every Sink: declares `content` (the pipe's value and a `sink`'s body) unless it takes no
/// body at all, declares `urn:cap:fs:write:*`, and is refused with a typed `Denied` under a
/// capability holding no grants — before it runs. The purges take no body (the set is the
/// persisted status), and say so by declaring no by-value input. Plus the pipe form, landing:
/// `content` alone promotes a tag, and neither spelling is `MissingArgument("tag")`.
#[test]
fn every_sink_declares_content_and_is_denied_without_a_grant() {
    let room = room();
    let kernel = &room.kernel;
    let mut sinks = vec!["urn:cms:tag-approve", "urn:cms:tag-reject"];
    #[cfg(feature = "maintenance")]
    sinks.extend(["urn:cms:link-remove", "urn:cms:link-keep"]);
    #[cfg(feature = "maintenance")]
    let purges = [
        "urn:cms:purge",
        "urn:cms:purge-domains",
        "urn:cms:purge-unreachable",
    ];
    #[cfg(not(feature = "maintenance"))]
    let purges: [&str; 0] = [];

    for target in sinks.iter().chain(purges.iter()) {
        let description = kernel
            .describe(&iri(target))
            .unwrap_or_else(|| panic!("{target} describes itself"));
        let sink = description
            .action_specs()
            .into_iter()
            .find(|a| a.verb == Verb::Sink)
            .unwrap_or_else(|| panic!("{target} declares Sink"));
        assert!(
            sink.requires.iter().any(|r| r == FS_WRITE),
            "{target} declares the write scope: {:?}",
            sink.requires
        );
        let has_content = sink.inputs.iter().any(|i| i.name == "content");
        if purges.contains(target) {
            assert!(
                sink.inputs.is_empty(),
                "{target} takes no body: no by-value input at all"
            );
        } else {
            assert!(has_content, "{target} declares `content`");
        }
        for input in &sink.inputs {
            assert!(input.class.is_some(), "{target}: `{}` is typed", input.name);
        }
        match issue(
            kernel,
            request(Verb::Sink, target, &[("content", "x")]),
            &no_grants(),
        ) {
            Err(Error::Denied(_)) => {}
            other => panic!("{target} under no grants is Denied, got {other:?}"),
        }
    }

    // `book` is held to its class: a value that is not an IRI is refused, never written —
    // written, it would poison the whole union (the walk's BEFORE run proved it: the
    // then-ungated Sink ran under ENFORCED with `book=x` and every later face failed to parse).
    match issue(
        kernel,
        request(
            Verb::Sink,
            "urn:cms:tag-approve",
            &[("book", "x"), ("tag", "t")],
        ),
        &Capability::root(),
    ) {
        Err(Error::InvalidArgument { name, .. }) => assert_eq!(name, "book"),
        other => panic!("a non-IRI book is InvalidArgument(\"book\"), got {other:?}"),
    }
    assert!(
        !ikigai_cms_web::tagstore::entries(&room.tags.approved)
            .iter()
            .any(|e| e.iri == "x"),
        "nothing was written for the refused book"
    );

    // The pipe form lands: `content` is the tag.
    let subject = "urn:cms:bookmark:seed-suggested";
    resolve(
        kernel,
        request(
            Verb::Sink,
            "urn:cms:tag-approve",
            &[("book", subject), ("content", "suggested-tag")],
        ),
    );
    assert!(
        ikigai_cms_web::tagstore::entries(&room.tags.approved)
            .iter()
            .any(|e| e.iri == subject && e.tag == "suggested-tag"),
        "promoted through `content`"
    );
    match issue(
        kernel,
        request(Verb::Sink, "urn:cms:tag-approve", &[("book", subject)]),
        &Capability::root(),
    ) {
        Err(Error::MissingArgument(name)) => assert_eq!(name, "tag"),
        other => panic!("neither spelling: MissingArgument(\"tag\"), got {other:?}"),
    }
}

/// Every action of this module: the bare media type it serves with minimal inputs is one of
/// its declared outputs, and it declares at least one. The suite compares only RDF faces,
/// and only once declared (PENDING #11/#31/#79).
#[test]
fn declared_outputs_are_the_media_types_served() {
    let room = room();
    let kernel = &room.kernel;
    // (the IRI to resolve, the verb, the args; the description reached is the id's)
    type Call<'a> = (&'a str, Verb, Vec<(&'a str, &'a str)>);
    let mut calls: Vec<Call<'_>> = vec![
        (ZOTERO_IRI, Verb::Source, vec![]),
        ("urn:cms:graph:bookmarks", Verb::Source, vec![]),
        ("urn:cms:graph:books", Verb::Source, vec![]),
        ("urn:cms:graph:zotero-links", Verb::Source, vec![]),
        ("urn:cms:graph:presentations", Verb::Source, vec![]),
        ("urn:cms:graph:tags-approved", Verb::Source, vec![]),
        ("urn:cms:graph:suggestions", Verb::Source, vec![]),
        ("urn:cms:graph:dismissed", Verb::Source, vec![]),
        ("urn:cms:graph", Verb::Source, vec![]),
        ("urn:cms:view:quic", Verb::Source, vec![]),
        ("urn:cms:search", Verb::Source, vec![("q", "rust")]),
        ("urn:cms:tags", Verb::Source, vec![]),
        ("urn:cms:type:book", Verb::Source, vec![]),
        ("urn:cms:types", Verb::Source, vec![]),
        ("urn:cms:style:catalog", Verb::Source, vec![]),
        (
            "urn:cms:tag-reject",
            Verb::Sink,
            vec![
                ("book", "urn:cms:bookmark:seed-suggested"),
                ("tag", "suggested-tag"),
            ],
        ),
        (
            "urn:cms:tag-approve",
            Verb::Sink,
            vec![
                ("book", "urn:cms:bookmark:seed-suggested"),
                ("tag", "suggested-tag"),
            ],
        ),
    ];
    #[cfg(feature = "maintenance")]
    calls.extend([
        ("urn:cms:linkstatus", Verb::Source, vec![]),
        ("urn:cms:review", Verb::Source, vec![]),
        ("urn:cms:purge", Verb::Source, vec![]),
        ("urn:cms:purge-domains", Verb::Source, vec![]),
        ("urn:cms:purge-unreachable", Verb::Source, vec![]),
        (
            "urn:cms:link-remove",
            Verb::Sink,
            vec![("url", "https://nowhere.example/")],
        ),
        (
            "urn:cms:link-keep",
            Verb::Sink,
            vec![("url", "https://nowhere.example/")],
        ),
    ]);
    let mut seen = BTreeSet::new();
    for (target, verb, args) in calls {
        let description = kernel
            .describe(&iri(target))
            .unwrap_or_else(|| panic!("{target} describes itself"));
        seen.insert(description.id.clone());
        let spec = description
            .action_specs()
            .into_iter()
            .find(|a| a.verb == verb)
            .unwrap_or_else(|| panic!("{target} declares {verb:?}"));
        let declared: BTreeSet<String> = spec
            .outputs
            .iter()
            .map(|o| rdf::bare_media_type(o))
            .collect();
        assert!(!declared.is_empty(), "{target} {verb:?} declares an output");
        assert!(
            !spec.inputs.iter().any(|i| i.name == "as"),
            "{target}: one face per action, no `as`"
        );
        let served = resolve(kernel, request(verb, target, &args));
        let got = rdf::bare_media_type(&served.repr_type.media_type);
        assert!(
            declared.contains(&got),
            "{target} {verb:?} served `{got}`, declared {declared:?}"
        );
    }
    let all: BTreeSet<String> = ours().iter().map(|s| s.to_string()).collect();
    assert_eq!(seen, all, "every id of this module was served");
}

/// The union's Turtle face, in full: parses, no blank node, every subject a skolem under one
/// of the room's three schemes, every term Dublin Core or `cms:` — and the `cms:` terms are
/// exactly the ones the README names. VOCABULARY holds the predicates and classes over the
/// fixture; subjects and the exact term set are checked here (PENDING #96).
#[test]
fn the_union_is_skolemized_on_the_cms_vocabulary() {
    let room = room();
    let repr = source(&room.kernel, "urn:cms:graph");
    let ttl = text(&repr);
    let triples = rdf::parse(&repr.repr_type.media_type, &repr.bytes)
        .unwrap_or_else(|e| panic!("the union parses: {e}\n{ttl}"));
    assert!(triples.len() >= 20, "a real graph:\n{ttl}");
    assert!(rdf::blank_nodes(&triples).is_empty(), "skolemized:\n{ttl}");

    let mut kinds = BTreeSet::new();
    let mut cms_terms = BTreeSet::new();
    for triple in &triples {
        let subject = triple.subject.to_string();
        assert!(
            [
                "<urn:cms:bookmark:",
                "<urn:cms:book:",
                "<urn:cms:presentation:"
            ]
            .iter()
            .any(|scheme| subject.starts_with(scheme)),
            "every subject is a skolem under a room scheme: {subject}"
        );
        let predicate = triple.predicate.as_str();
        assert!(
            predicate.starts_with(DC) || predicate.starts_with(CMS) || predicate == RDF_TYPE,
            "Dublin Core or cms: only: {predicate}"
        );
        if let Some(term) = predicate.strip_prefix(CMS) {
            cms_terms.insert(term.to_string());
        }
        if predicate == RDF_TYPE {
            let class = triple.object.to_string();
            let class = class.trim_matches(|c| c == '<' || c == '>');
            let kind = class
                .strip_prefix(CMS)
                .unwrap_or_else(|| panic!("every class is a cms: kind: {class}"));
            kinds.insert(kind.to_string());
            cms_terms.insert(kind.to_string());
        }
    }
    assert_eq!(
        kinds,
        BTreeSet::from([
            "Book".to_string(),
            "Bookmark".to_string(),
            "Presentation".to_string()
        ]),
        "the three content kinds, each typed at its source"
    );
    assert_eq!(
        cms_terms,
        [
            "Book",
            "Bookmark",
            "Presentation",
            "suggestedTag",
            "dismissedTag",
            "zoteroItem",
            "zoteroAttachment",
            "readerUrl",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>(),
        "the cms: terms the README names (cms:isbn joins when a book has one):\n{ttl}"
    );
}

/// The stylesheet binding's `one_of` is the closed set: every declared name resolves to the
/// declared XSLT face, and a name outside it is an error, never a fallback.
#[test]
fn the_stylesheets_are_the_declared_names() {
    let room = room();
    let kernel = &room.kernel;
    let description = kernel
        .describe(&iri("urn:cms:style:catalog"))
        .expect("the stylesheet describes itself");
    assert_eq!(description.id, "cms-style");
    let name = description
        .inputs
        .iter()
        .find(|i| i.name == "name")
        .expect("`name` is declared");
    assert_eq!(name.one_of.len(), 7, "{:?}", name.one_of);
    for style in &name.one_of {
        let repr = source(kernel, &format!("urn:cms:style:{style}"));
        assert_eq!(
            rdf::bare_media_type(&repr.repr_type.media_type),
            "application/xslt+xml"
        );
        assert_eq!(
            repr.expiry,
            Expiry::Never,
            "{style}: embedded, pure, cached"
        );
        assert!(text(&repr).contains("xsl:stylesheet"), "{style}");
    }
    assert!(issue(
        kernel,
        request(Verb::Source, "urn:cms:style:nope", &[]),
        &Capability::root()
    )
    .is_err());
}

/// Required means required (PENDING #49/#99): a call missing a required by-value input is a
/// typed `MissingArgument` naming it, never a placeholder result.
#[test]
fn required_is_required() {
    let room = room();
    let kernel = &room.kernel;
    let missing = |verb: Verb, target: &str, args: &[(&str, &str)]| -> String {
        match issue(kernel, request(verb, target, args), &Capability::root()) {
            Err(Error::MissingArgument(name)) => name,
            other => panic!("{target}: expected MissingArgument, got {other:?}"),
        }
    };
    assert_eq!(missing(Verb::Source, "urn:cms:search", &[]), "q");
    assert_eq!(
        missing(Verb::Sink, "urn:cms:tag-approve", &[("tag", "x")]),
        "book"
    );
    #[cfg(feature = "maintenance")]
    assert_eq!(missing(Verb::Sink, "urn:cms:link-remove", &[]), "url");
}

/// `Fixture::new(id, …)` is keyed on the DESCRIPTION id, not `name()` — the fs jail's is
/// `file`, at both of its patterns (PENDING #57).
#[test]
fn the_fixture_id_is_the_description_id() {
    let room = room();
    for target in [
        format!("urn:cms:src:{SCRATCH_FILE}"),
        format!("urn:cms:deck:{SCRATCH_FILE}"),
    ] {
        let description = room
            .kernel
            .describe(&iri(&target))
            .unwrap_or_else(|| panic!("{target} describes itself"));
        assert_eq!(description.id, "file", "{target}");
    }
    assert_eq!(
        room.kernel
            .describe(&iri("urn:cms:graph"))
            .expect("described")
            .id,
        "urn:cms:graph"
    );
}

/// The pass kernel (`maintenance_kernel`): the room's spaces plus ikigai-http, ikigai-llm
/// and ikigai-secret over a canned transport and a scratch file keystore, and the three
/// passes. Walked the same way: this module's ids clean but NAMES, the dependencies'
/// findings pinned to their ids. The passes run under root here — link-check HEADs the two
/// fixture bookmarks against the canned 200, tag-suggest asks the canned LLM, zotero-links
/// sweeps the canned API with a key read as `urn:secret:zotero-api-key` — and every write
/// lands in the scratch dir.
#[cfg(feature = "maintenance")]
#[test]
fn the_maintenance_kernel_conforms() {
    use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};
    use ikigai_secret::Backend;
    use std::sync::Arc;

    /// Every origin the passes reach, canned: routed by URL, always 200.
    struct Canned;
    #[async_trait::async_trait]
    impl HttpTransport for Canned {
        async fn send(&self, req: HttpRequest) -> std::result::Result<HttpResponse, String> {
            let body: &[u8] = if req.url.contains("openlibrary") {
                br#"{"ISBN:9781617294556":{"subjects":[{"name":"Rust (Computer program language)"}]}}"#
            } else if req.url.contains("chat/completions") {
                br#"{"choices":[{"message":{"content":"rust, systems-programming"}}]}"#
            } else if req.url.contains("/keys/current") {
                br#"{"userID":1,"username":"conformance","access":{"user":{"library":true,"files":true}}}"#
            } else if req.url.contains("itemType=book") {
                br#"[{"key":"BOOKKEY1","data":{"title":"Rust in Action","ISBN":"978-1-61729-455-6","creators":[{"lastName":"McNamara","firstName":"Tim"}]}}]"#
            } else if req.url.contains("itemType=attachment") {
                br#"[{"key":"REALEPUB","links":{"alternate":{"href":"https://www.zotero.org/u/items/REALEPUB"}},"data":{"parentItem":"BOOKKEY1","linkMode":"imported_file","contentType":"application/epub+zip"}}]"#
            } else {
                b"{}"
            };
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: body.to_vec(),
            })
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let (src, zotero, _presentations, tags, status) = scratch(dir.path());
    let secrets = ikigai_secret::FileBackend::new(dir.path().join("secrets"));
    secrets
        .set("zotero-api-key", b"s3cret")
        .expect("seed the key");
    secrets
        .set("conformance", b"scratch")
        .expect("seed the walk's secret");
    let registry = ikigai_llm::Registry::single(ikigai_llm::OpenAiConfig::ollama("llama3.2"));
    let kernel = ikigai_cms_web::maintenance::maintenance_kernel(
        src,
        Some(zotero),
        Some(BOOKMARKS.to_string()),
        status,
        tags,
        Arc::new(Canned),
        Arc::new(secrets),
        registry,
        "ollama",
    );

    let report = suite()
        .fixture(Fixture::new("urn:secret", Verb::Source).binding("name", "conformance"))
        // The pass kernel never has a presentations root (`maintenance_kernel` passes
        // `None`), so its presentations graph is the constant empty document: a pure
        // function of nothing, cached forever correctly. Over the SERVING kernel the same
        // endpoint reads deck files and is threaded to them — a declaration certifies
        // behavior over the kernel passed (the suite's PENDING #18/#30).
        .pure("urn:cms:graph:presentations")
        .run_blocking(&kernel);
    eprintln!("{report}");

    let mut ours = OURS.to_vec();
    ours.extend_from_slice(OURS_MAINTENANCE);
    ours.extend([
        "urn:cms:linkcheck",
        "urn:cms:tag-suggest",
        "urn:cms:zotero-links",
    ]);
    let mut inherited = INHERITED.to_vec();
    inherited.extend([
        "httpGet",
        "httpHead",
        "httpPost",
        "httpPut",
        "httpPatch",
        "httpDelete",
        "urn:llm:ask",
        "urn:llm:config",
        "urn:llm:models",
        "urn:llm:select",
        "urn:llm:ollama:ask",
        "urn:llm:ollama:up",
        "urn:llm:ollama:installed",
        "urn:secret",
    ]);
    assert_findings(&report, &kernel, &ours, &inherited);

    // The passes declare their gates and are refused under no grants, before any read.
    for pass in [
        "urn:cms:linkcheck",
        "urn:cms:tag-suggest",
        "urn:cms:zotero-links",
    ] {
        match issue(&kernel, request(Verb::Source, pass, &[]), &no_grants()) {
            Err(Error::Denied(_)) => {}
            other => panic!("{pass} under no grants is Denied, got {other:?}"),
        }
    }
}
