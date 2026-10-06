//! A chip opens the tag it names, and the trail re-opens it where it was opened.
//!
//! - **`+`.** The `/r/` path was decoded with FORM rules, so the `c++` chip
//!   (`hx-get="/r/urn:cms:view:c++"`) asked for tag `c  ` and opened an empty view.
//! - **A space.** The browser sends `machine%20learning`; decoded, that is an IRI with a space,
//!   which `Iri::parse` refuses — a tag the index lists could never be opened.
//! - **The presentation scope.** `recordable` knew `book | bookmark`, so a tag opened inside
//!   Presentations was recorded unscoped and re-opened across every kind.

use std::path::Path;

use super::{path_iri, recordable, resolve_http};
use ikigai_cms_web::session::RecentLog;
use ikigai_core::{Capability, Kernel};

fn room(dir: &Path, org: &str, decks: Option<&Path>) -> Kernel {
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(&bm, org).unwrap();
    ikigai_cms_web::build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        decks.map(|root| ikigai_cms_web::Presentations {
            root: root.to_path_buf(),
            base_url: None,
        }),
        None,
        ikigai_cms_web::tagstore::TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    )
}

fn get(kernel: &Kernel, target: &str) -> (&'static str, String) {
    let (status, _ct, body) = resolve_http(
        kernel,
        &RecentLog::default(),
        &Capability::root(),
        None,
        target,
    );
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[test]
fn a_tag_with_a_plus_opens_from_its_chip() {
    let dir = tempfile::tempdir().unwrap();
    let k = room(
        dir.path(),
        "* Bookmarks\n** [[https://cpp.example][Plus Plus Bookmark]]\n   :PROPERTIES:\n   :TAGS: c++\n   :END:\n",
        None,
    );
    let (_, index) = get(&k, "urn:cms:tags");
    assert!(index.contains("/r/urn:cms:view:c++"), "the chip: {index}");
    // What htmx sends for that href: `+` is legal in a path and goes out as is.
    let (status, view) = get(&k, "urn:cms:view:c++?style=catalog");
    assert!(
        status == "200 OK" && view.contains("Plus Plus Bookmark"),
        "the c++ chip opened {status}: {view}"
    );
}

#[test]
fn a_tag_with_a_space_opens_from_its_chip() {
    let dir = tempfile::tempdir().unwrap();
    let decks = tempfile::tempdir().unwrap();
    let deck = decks.path().join("talks/ml");
    std::fs::create_dir_all(deck.join("dist")).unwrap();
    std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
    std::fs::write(
        deck.join("dist/index.html"),
        "<script type=\"application/ld+json\">{\"title\":\"ML Talk\",\"tags\":[\"Machine Learning\"]}</script>",
    )
    .unwrap();
    let k = room(dir.path(), "* Bookmarks\n", Some(decks.path()));
    let (_, index) = get(&k, "urn:cms:tags");
    assert!(index.contains("machine learning"), "listed: {index}");
    // The browser percent-encodes the space in the chip's href.
    let (status, view) = get(&k, "urn:cms:view:machine%20learning?style=catalog");
    assert!(
        status == "200 OK" && view.contains("ML Talk"),
        "the listed tag cannot be opened: {status} {view}"
    );
}

#[test]
fn a_path_names_the_iri_it_spells() {
    for (path, iri) in [
        ("urn:cms:view:c++", "urn:cms:view:c++"),
        (
            "urn:cms:view:machine%20learning",
            "urn:cms:view:machine%20learning",
        ),
        (
            "urn:cms:view:machine learning",
            "urn:cms:view:machine%20learning",
        ),
        ("urn:cms:view:caf%C3%A9", "urn:cms:view:café"),
        ("urn:cms:view:100%25", "urn:cms:view:100%25"),
        ("urn:cms:view:%3Cb%3E", "urn:cms:view:%3Cb%3E"),
        ("urn%3Acms%3Atags", "urn:cms:tags"),
    ] {
        assert_eq!(path_iri(path), iri, "{path}");
    }
}

#[test]
fn a_tag_opened_inside_presentations_keeps_its_scope() {
    for kind in ["book", "bookmark", "presentation"] {
        let (label, scope) = recordable("urn:cms:view:genomics", Some(kind)).expect("recordable");
        assert_eq!(scope.as_deref(), Some(kind));
        assert_eq!(label, format!("{kind} · #genomics"));
    }
    // The label shows the tag, not its escape.
    let (label, _) = recordable("urn:cms:view:machine%20learning", None).unwrap();
    assert_eq!(label, "#machine learning");
}
