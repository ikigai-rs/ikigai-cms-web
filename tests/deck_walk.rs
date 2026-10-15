//! Which decks exist is a resource, `urn:cms:decks`, and the presentations graph composes it
//! (ledger #286). Before, the graph walked the decks root with `std::fs` itself: nothing hung
//! the walk from a thread, so a deck directory created after the first read never reached the
//! room until the process restarted. And the deck files, cached under their names, had no
//! watcher in the serving process to cut them, so an edited deck was stale until a restart too.
//!
//! - [`a_new_deck_reaches_the_graph_when_the_listing_is_cut`]: the listing's thread is one the
//!   graph hangs from. Fails before the change (the cut named nothing the graph depended on).
//! - [`the_listing_names_decks_and_title_slides_in_iri_text`]: the listing's shape.
//! - [`the_listing_names_only_what_the_caller_may_read`]: the walk is gated through the deck jail
//!   under the CALLER's grants, including the room's own serving entitlement.
//! - [`the_watch_brings_a_new_deck_and_an_edit_without_a_restart`] (`server`): the platform
//!   watch the serving host runs is what cuts them.

use std::path::Path;

use ikigai_cms_web::tagstore::TagPaths;
use ikigai_cms_web::{build_cms_kernel_with, Presentations};
use ikigai_core::{Capability, Iri, Kernel, Request, Verb};

fn deck(root: &Path, rel: &str, title_slide: Option<(&str, &str)>) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("deck.toml"), "title = \"x\"\n").unwrap();
    if let Some((name, text)) = title_slide {
        std::fs::create_dir_all(dir.join("slides")).unwrap();
        std::fs::write(dir.join("slides").join(name), text).unwrap();
    }
}

/// The room's kernel over a scratch source jail and the decks under `root`.
fn kernel(src: &Path, root: &Path) -> Kernel {
    build_cms_kernel_with(
        src.to_path_buf(),
        None,
        Some(Presentations {
            root: root.to_path_buf(),
            base_url: None,
        }),
        None,
        TagPaths::in_dir(src),
        None,
    )
}

fn read(kernel: &Kernel, iri: &str, cap: &Capability) -> ikigai_core::Result<String> {
    let request = Request::new(Verb::Source, Iri::parse(iri).unwrap());
    futures::executor::block_on(kernel.issue(request, cap))
        .map(|r| String::from_utf8(r.bytes).unwrap())
}

fn graph(kernel: &Kernel) -> String {
    read(kernel, "urn:cms:graph:presentations", &Capability::root()).expect("graph resolves")
}

#[test]
fn a_new_deck_reaches_the_graph_when_the_listing_is_cut() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    deck(
        root.path(),
        "talks/alpha",
        Some(("00-title.md", "# Alpha Talk\n")),
    );
    let kernel = kernel(src.path(), root.path());
    assert!(graph(&kernel).contains("Alpha Talk"));

    deck(
        root.path(),
        "talks/beta",
        Some(("00-title.md", "# Beta Talk\n")),
    );
    // Cached: nothing has said the tree changed.
    assert!(!graph(&kernel).contains("Beta Talk"), "served from cache");
    // The listing's thread is what a watch cuts when a directory appears.
    kernel.cut("urn:cms:decks");
    let after = graph(&kernel);
    assert!(
        after.contains("Beta Talk"),
        "the new deck is listed: {after}"
    );
    assert!(after.contains("Alpha Talk"), "the old one stays: {after}");
}

#[test]
fn the_listing_names_decks_and_title_slides_in_iri_text() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    // An explicit title slide; a lexically-first fallback in a directory an IRI cannot carry
    // raw; a deck with no slides at all; and a directory that is not a deck.
    deck(root.path(), "a/explicit", Some(("00-title.md", "# One\n")));
    deck(
        root.path(),
        "b/my talk",
        Some(("01-intro deck.md", "# Two\n")),
    );
    std::fs::write(root.path().join("b/my talk/slides/02-more.md"), "# no\n").unwrap();
    deck(root.path(), "c/bare", None);
    std::fs::create_dir_all(root.path().join("_assets/images")).unwrap();
    let kernel = kernel(src.path(), root.path());

    let listing = read(&kernel, "urn:cms:decks", &Capability::root()).expect("listing");
    assert_eq!(
        listing,
        "a/explicit slides/00-title.md\n\
         b/my%20talk slides/01-intro%20deck.md\n\
         c/bare\n"
    );
    // And the graph reads the title from the slide the listing named.
    let ttl = graph(&kernel);
    assert!(
        ttl.contains("dc:title \"Two\""),
        "fallback title slide: {ttl}"
    );
}

#[test]
fn the_listing_names_only_what_the_caller_may_read() {
    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    deck(
        root.path(),
        "public/open",
        Some(("00-title.md", "# Open Talk\n")),
    );
    deck(
        root.path(),
        "private/closed",
        Some(("00-title.md", "# Closed Talk\n")),
    );
    let kernel = kernel(src.path(), root.path());
    let r = root.path().display();

    // The room's serving entitlement: a read of the source jail and of the decks root.
    let room = Capability::scoped([
        format!("urn:cap:fs:read:{}", src.path().display()),
        format!("urn:cap:fs:read:{r}"),
    ]);
    let ttl = read(&kernel, "urn:cms:graph:presentations", &room).expect("graph");
    assert!(
        ttl.contains("Open Talk") && ttl.contains("Closed Talk"),
        "{ttl}"
    );

    // A deny on one subtree: that deck is not even named.
    let narrowed = Capability::scoped([
        format!("urn:cap:fs:read:{r}"),
        format!("urn:cap:fs:read:-{r}/private"),
    ]);
    let listing = read(&kernel, "urn:cms:decks", &narrowed).expect("listing");
    assert_eq!(listing, "public/open slides/00-title.md\n");

    // A read grant elsewhere passes the floor but reaches no deck.
    let elsewhere = Capability::scoped([format!("urn:cap:fs:read:{}", src.path().display())]);
    assert_eq!(
        read(&kernel, "urn:cms:decks", &elsewhere).expect("listing"),
        ""
    );

    // No grant at all: refused at the floor, as the graph is.
    let none = Capability::scoped(Vec::<String>::new());
    assert!(read(&kernel, "urn:cms:decks", &none).is_err());
    assert!(read(&kernel, "urn:cms:graph:presentations", &none).is_err());
}

/// The serving host's watch, on the real platform watcher: a deck directory created after the
/// first read, then an edit to an existing deck's title slide, each reach the graph once the
/// watch has applied the platform's notifications. Driven by the watcher's own events (with a
/// deadline as an upper bound on platform latency), never by a sleep.
#[cfg(feature = "server")]
#[test]
fn the_watch_brings_a_new_deck_and_an_edit_without_a_restart() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let src = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    deck(
        root.path(),
        "talks/alpha",
        Some(("00-title.md", "# Alpha Talk\n")),
    );
    let kernel = Arc::new(kernel(src.path(), root.path()));
    assert!(graph(&kernel).contains("Alpha Talk"));
    let watch = ikigai_cms_web::DeckWatch::start(Arc::clone(&kernel), root.path()).expect("watch");

    let until = |what: &str, done: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let ttl = graph(&kernel);
            if done(&ttl) {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "{what}: not seen within the deadline: {ttl}"
            );
            watch.apply_next(left);
        }
    };

    deck(
        root.path(),
        "talks/beta",
        Some(("00-title.md", "# Beta Talk\n")),
    );
    until("a new deck", &|ttl| ttl.contains("Beta Talk"));

    std::fs::write(
        root.path().join("talks/alpha/slides/00-title.md"),
        "# Alpha Revised\n",
    )
    .unwrap();
    until("an edited deck", &|ttl| {
        ttl.contains("Alpha Revised") && !ttl.contains("Alpha Talk")
    });
}
