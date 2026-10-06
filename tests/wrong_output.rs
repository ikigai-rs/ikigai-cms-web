//! Wrong output that outlives its cause — the second set of the data-loss arc.
//!
//! - The durable-key migration refused every run once a file held a durable key, after earlier
//!   files were already rewritten (a partial rekey), so tags stayed on per-export keys and landed
//!   on the wrong book after a re-export.
//! - One carriage return in a deck title made `urn:cms:graph` unparseable, and every view with it.
//! - A failed library read cached an empty books graph that the documented cut could not reach.
//!
//! Every test uses a scratch directory; nothing here touches a real path.

use std::path::Path;

use ikigai_cms_web::build_cms_kernel_with;
use ikigai_cms_web::tagstore::{entries, TagPaths};
use ikigai_core::{Capability, Iri, Kernel, Request, Verb};

const ZI: &str = "https://ikigai-rs.dev/ns/cms#zoteroItem";
const DC_SUBJECT: &str = "http://purl.org/dc/elements/1.1/subject";
const SUGGESTED: &str = "https://ikigai-rs.dev/ns/cms#suggestedTag";

fn run(k: &Kernel, iri: &str) -> Result<String, String> {
    futures::executor::block_on(k.issue(
        Request::new(Verb::Source, Iri::parse(iri).unwrap()),
        &Capability::root(),
    ))
    .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
    .map_err(|e| e.to_string())
}

// ---- the durable-key migration ----------------------------------------------------------------

/// The documented re-run after a better link sweep: book A's tag is already durable from the first
/// run, book B is newly matched. The re-run must move B and leave A alone.
#[test]
fn a_rerun_after_a_better_link_sweep_moves_the_new_matches() {
    let dir = tempfile::tempdir().unwrap();
    let tags = TagPaths::in_dir(dir.path());
    std::fs::write(
        &tags.zotero_links,
        format!(
            "<urn:cms:book:aaa> <{ZI}> <urn:zotero:item:XXXX> .\n\
             <urn:cms:book:bbb> <{ZI}> <urn:zotero:item:YYYY> .\n"
        ),
    )
    .unwrap();
    std::fs::write(
        &tags.approved,
        format!(
            "<urn:cms:book:bbb> <{DC_SUBJECT}> \"tag-b\" .\n\
             <urn:zotero:item:XXXX> <{DC_SUBJECT}> \"tag-a\" .\n"
        ),
    )
    .unwrap();

    let report = tags
        .migrate_to_durable_keys()
        .unwrap_or_else(|e| panic!("the re-run was refused: {e}"));
    let mut stored: Vec<String> = entries(&tags.approved).into_iter().map(|e| e.iri).collect();
    stored.sort();
    assert_eq!(stored, ["urn:zotero:item:XXXX", "urn:zotero:item:YYYY"]);
    assert_eq!((report.moved, report.already), (1, 1), "{report:?}");
}

/// A durable key whose book left today's export is PARKED by design ("the human decision stays in
/// the file, waiting for the book to come back"). It must not block the migration, and it must
/// survive it. And no run may leave some files rekeyed and others not.
#[test]
fn migration_tolerates_a_parked_durable_key_and_is_never_partial() {
    let dir = tempfile::tempdir().unwrap();
    let tags = TagPaths::in_dir(dir.path());
    std::fs::write(
        &tags.zotero_links,
        format!("<urn:cms:book:A> <{ZI}> <urn:zotero:item:K1> .\n"),
    )
    .unwrap();
    std::fs::write(
        &tags.approved,
        format!("<urn:cms:book:A> <{DC_SUBJECT}> \"rust\" .\n"),
    )
    .unwrap();
    std::fs::write(
        &tags.suggestions,
        format!(
            "<urn:cms:book:A> <{SUGGESTED}> \"systems\" .\n\
             <urn:zotero:item:K2> <{SUGGESTED}> \"parked\" .\n"
        ),
    )
    .unwrap();

    let report = tags
        .migrate_to_durable_keys()
        .unwrap_or_else(|e| panic!("the migration was refused over a parked key: {e}"));
    assert_eq!(report.rewritten.len(), 2, "{report:?}");
    let approved: Vec<String> = entries(&tags.approved).into_iter().map(|e| e.iri).collect();
    assert_eq!(approved, ["urn:zotero:item:K1"]);
    let mut sug: Vec<(String, String)> = entries(&tags.suggestions)
        .into_iter()
        .map(|e| (e.iri, e.tag))
        .collect();
    sug.sort();
    assert_eq!(
        sug,
        [
            ("urn:zotero:item:K1".to_string(), "systems".to_string()),
            ("urn:zotero:item:K2".to_string(), "parked".to_string()),
        ],
        "the parked decision is kept, the movable one moved"
    );
}

/// A re-run with nothing to move is a no-op: no backup, no write, no mtime churn.
#[test]
fn a_rerun_with_nothing_to_move_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let tags = TagPaths::in_dir(dir.path());
    std::fs::write(
        &tags.zotero_links,
        format!("<urn:cms:book:A> <{ZI}> <urn:zotero:item:K1> .\n"),
    )
    .unwrap();
    std::fs::write(
        &tags.approved,
        format!("<urn:zotero:item:K1> <{DC_SUBJECT}> \"rust\" .\n"),
    )
    .unwrap();
    let report = tags
        .migrate_to_durable_keys()
        .expect("an idempotent re-run");
    assert!(
        report.rewritten.is_empty() && report.backups.is_empty(),
        "{report:?}"
    );
    assert_eq!(report.already, 1);
}

// ---- one carriage return ----------------------------------------------------------------------

fn deck_room(dir: &Path, decks: &Path) -> Kernel {
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(&bm, "* Bookmarks\n** [[https://sci.example/][Science]]\n   :PROPERTIES:\n   :TAGS: science\n   :END:\n").unwrap();
    build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        Some(ikigai_cms_web::Presentations {
            root: decks.to_path_buf(),
            base_url: None,
        }),
        None,
        TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    )
}

/// A deck title carrying a CR (and a tag carrying one) must not make the union unparseable: the
/// tag index — every view — answers, and the deck is in it.
#[test]
fn a_carriage_return_in_a_deck_title_does_not_take_the_room_down() {
    let src = tempfile::tempdir().unwrap();
    let decks = tempfile::tempdir().unwrap();
    let deck = decks.path().join("talks/crlf");
    std::fs::create_dir_all(deck.join("dist")).unwrap();
    std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
    std::fs::write(
        deck.join("dist/index.html"),
        "<script type=\"application/ld+json\">{\"title\":\"Part One\\rPart Two\",\"tags\":[\"rust\",\"line\\r\\nbreak\"]}</script>",
    )
    .unwrap();
    let k = deck_room(src.path(), decks.path());
    let index = run(&k, "urn:cms:tags");
    assert!(
        index
            .as_ref()
            .is_ok_and(|h| h.contains("rust") && h.contains("science")),
        "the tag index failed: {index:?}"
    );
    let graph = run(&k, "urn:cms:graph").expect("the union resolves");
    assert!(
        graph.contains("Part One"),
        "the deck is in the room:\n{graph}"
    );
}

// ---- a failed library read ----------------------------------------------------------------------

const ZOTERO: &str = r##"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
  xmlns:bib="http://purl.org/net/biblio#"
  xmlns:dc="http://purl.org/dc/elements/1.1/"
  xmlns:z="http://www.zotero.org/namespaces/export#">
  <bib:Book rdf:about="#item_1">
    <z:itemType>book</z:itemType>
    <dc:title>Rust in Action</dc:title>
  </bib:Book>
</rdf:RDF>"##;

fn books_room(dir: &Path, zotero: std::path::PathBuf) -> Kernel {
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("old-org")).unwrap();
    std::fs::write(src.join("old-org/pinboard-bookmarks.org"), "* Bookmarks\n").unwrap();
    build_cms_kernel_with(src, Some(zotero), None, None, TagPaths::in_dir(dir), None)
}

/// The export is mid-rewrite (truncated) at the first read; once it is whole, the documented cut on
/// `urn:cms:src:zotero` must bring the books back — not a restart.
#[test]
fn a_cut_recovers_the_books_after_a_truncated_export() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("zotero.rdf");
    std::fs::write(&z, &ZOTERO[..ZOTERO.len() / 2]).unwrap();
    let k = books_room(dir.path(), z.clone());
    assert!(!run(&k, "urn:cms:graph:books")
        .unwrap()
        .contains("Rust in Action"));
    std::fs::write(&z, ZOTERO).unwrap();
    k.cut("urn:cms:src:zotero");
    let after = run(&k, "urn:cms:graph:books").unwrap();
    assert!(
        after.contains("Rust in Action"),
        "the empty fallback outlived the cut: {after:?}"
    );
}

/// The export is briefly absent at the first read (a replace in progress): the same.
#[test]
fn a_cut_recovers_the_books_after_an_absent_export() {
    let dir = tempfile::tempdir().unwrap();
    let z = dir.path().join("zotero.rdf");
    let k = books_room(dir.path(), z.clone());
    assert!(!run(&k, "urn:cms:graph:books")
        .unwrap()
        .contains("Rust in Action"));
    std::fs::write(&z, ZOTERO).unwrap();
    k.cut("urn:cms:src:zotero");
    let after = run(&k, "urn:cms:graph:books").unwrap();
    assert!(
        after.contains("Rust in Action"),
        "the empty fallback outlived the cut: {after:?}"
    );
}

/// The same escaping on the query side: a search for text pasted with a line break is a search,
/// not a SPARQL syntax error.
#[test]
fn a_search_with_a_line_break_is_not_a_syntax_error() {
    let src = tempfile::tempdir().unwrap();
    let decks = tempfile::tempdir().unwrap();
    let k = deck_room(src.path(), decks.path());
    let req = Request::new(Verb::Source, Iri::parse("urn:cms:search").unwrap()).with_arg(
        "q",
        ikigai_core::ArgRef::Inline(b"Science\r\nfiction".to_vec()),
    );
    let out = futures::executor::block_on(k.issue(req, &Capability::root()));
    assert!(out.is_ok(), "the search failed: {:?}", out.err());
}
