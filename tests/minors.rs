//! The minor set of the data-loss arc: each a wrong answer or a silent drop, none losing a
//! decision. Library-level here; the page face's cases are in `src/bin/server_tests/chips.rs`
//! and the config's in `src/config.rs`.
//!
//! Every test uses a scratch directory; nothing here touches a real path.

use ikigai_cms_web::build_cms_kernel_with;
use ikigai_cms_web::tagstore::TagPaths;
use ikigai_core::{Capability, Iri, Request, Verb};
use ikigai_resolve::Resolver;

/// A deck under a directory whose name an IRI cannot carry raw (a space) is in the room, beside
/// one that can — read through the jail under a capability scoped to the deck root, so the path
/// the jail judges is the real one.
#[test]
fn a_deck_in_a_directory_with_a_space_is_in_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("old-org")).unwrap();
    std::fs::write(src.join("old-org/pinboard-bookmarks.org"), "* Bookmarks\n").unwrap();
    let root = dir.path().join("decks");
    for (rel, title) in [
        ("talks/plain-deck", "Plain Deck"),
        ("talks/spaced deck", "Spaced Deck"),
    ] {
        let deck = root.join(rel);
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        std::fs::write(deck.join("slides/00-title.md"), format!("# {title}\n")).unwrap();
    }
    let kernel = build_cms_kernel_with(
        src.clone(),
        None,
        Some(ikigai_cms_web::Presentations {
            root: root.clone(),
            base_url: None,
        }),
        None,
        TagPaths::in_dir(dir.path()),
        None,
    );
    let cap = Capability::scoped(vec![
        format!("urn:cap:fs:read:{}", src.display()),
        format!("urn:cap:fs:read:{}", root.display()),
    ]);
    let req = Request::new(
        Verb::Source,
        Iri::parse("urn:cms:graph:presentations").unwrap(),
    );
    let ttl = String::from_utf8(Resolver::issue_as(&kernel, req, &cap).unwrap().0.bytes).unwrap();
    assert!(ttl.contains("Plain Deck"), "control: {ttl}");
    assert!(
        ttl.contains("Spaced Deck"),
        "the deck under `spaced deck/` is missing from the graph:\n{ttl}"
    );
}

#[cfg(feature = "maintenance")]
mod passes {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
    use ikigai_http::{HttpRequest, HttpResponse, HttpTransport};

    /// Counts sends; answers 200 everywhere, and for the LLM a different tag list per ask.
    struct Counting {
        sends: Arc<AtomicU32>,
        asks: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl HttpTransport for Counting {
        async fn send(&self, req: HttpRequest) -> Result<HttpResponse, String> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            let body: Vec<u8> = if req.url.contains("chat/completions") {
                let n = self.asks.fetch_add(1, Ordering::SeqCst);
                let content = if n == 0 {
                    "rust, systems, memory"
                } else {
                    "concurrency, safety, performance"
                };
                format!(r#"{{"choices":[{{"message":{{"content":"{content}"}}}}]}}"#).into_bytes()
            } else {
                b"{}".to_vec() // the /models probe says up; OpenLibrary has no subjects
            };
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body,
            })
        }
    }

    fn pass_kernel(
        dir: &std::path::Path,
        org: &str,
        zotero: Option<&str>,
    ) -> (Kernel, Arc<AtomicU32>, Arc<AtomicU32>) {
        let bm = dir.join("old-org/pinboard-bookmarks.org");
        std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
        std::fs::write(&bm, org).unwrap();
        let z = zotero.map(|rdf| {
            let z = dir.join("z.rdf");
            std::fs::write(&z, rdf).unwrap();
            z
        });
        let (sends, asks) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let kernel = ikigai_cms_web::maintenance::maintenance_kernel(
            dir.to_path_buf(),
            z,
            None,
            dir.join("st.json"),
            ikigai_cms_web::tagstore::TagPaths::in_dir(dir),
            Arc::new(Counting {
                sends: Arc::clone(&sends),
                asks: Arc::clone(&asks),
            }),
            Arc::new(ikigai_secret::FileBackend::new(dir.join("secrets"))),
            ikigai_cms_web::maintenance::default_llm_registry(),
            "ollama",
        );
        (kernel, sends, asks)
    }

    fn pass(kernel: &Kernel, target: &str, limit: Option<&str>) -> Result<String, String> {
        let mut req = Request::new(Verb::Source, Iri::parse(target).unwrap());
        if let Some(l) = limit {
            req = req.with_arg("limit", ArgRef::Inline(l.as_bytes().to_vec()));
        }
        futures::executor::block_on(kernel.issue(req, &Capability::root()))
            .map(|r| String::from_utf8_lossy(&r.bytes).into_owned())
            .map_err(|e| e.to_string())
    }

    /// A bound must refuse, not truncate: `limit=2x` (a typo for 2) ran the whole link-check
    /// pass, and tag-suggest silently used its default.
    #[test]
    fn a_malformed_limit_is_refused_not_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut org = String::from("* Bookmarks\n");
        for i in 0..20 {
            org.push_str(&format!("** [[https://site{i}.example/][Site {i}]]\n"));
        }
        let (kernel, sends, _) = pass_kernel(dir.path(), &org, None);
        for target in ["urn:cms:linkcheck", "urn:cms:tag-suggest"] {
            let out = pass(&kernel, target, Some("2x"));
            assert!(
                out.as_ref().is_err_and(|e| e.contains("limit")),
                "{target} accepted limit=`2x`: {out:?}"
            );
        }
        assert_eq!(sends.load(Ordering::SeqCst), 0, "nothing ran");
        // A well-formed bound still bounds.
        pass(&kernel, "urn:cms:linkcheck", Some("2")).expect("limit=2 runs");
        assert!(sends.load(Ordering::SeqCst) <= 4, "two checks, HEAD (+GET)");
    }

    const TWO_AUTHORS: &str = r##"<?xml version="1.0"?>
<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:bib="http://purl.org/net/biblio#"
  xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:z="http://www.zotero.org/namespaces/export#"
  xmlns:foaf="http://xmlns.com/foaf/0.1/">
  <bib:Book rdf:about="urn:isbn:9781492052593">
    <z:itemType>book</z:itemType>
    <dc:title>Programming Rust</dc:title>
    <bib:authors><rdf:Seq>
      <rdf:li><foaf:Person><foaf:surname>Blandy</foaf:surname><foaf:givenName>Jim</foaf:givenName></foaf:Person></rdf:li>
      <rdf:li><foaf:Person><foaf:surname>Orendorff</foaf:surname><foaf:givenName>Jason</foaf:givenName></foaf:Person></rdf:li>
    </rdf:Seq></bib:authors>
  </bib:Book>
</rdf:RDF>"##;

    /// A two-author book is one book: one LLM ask, at most the per-book cap of suggestions.
    #[test]
    fn a_two_author_book_is_suggested_for_once() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, _, asks) = pass_kernel(dir.path(), "* Bookmarks\n", Some(TWO_AUTHORS));
        let summary = pass(&kernel, "urn:cms:tag-suggest", None).expect("tag-suggest runs");
        let stored = ikigai_cms_web::tagstore::entries(
            &ikigai_cms_web::tagstore::TagPaths::in_dir(dir.path()).suggestions,
        );
        assert_eq!(
            asks.load(Ordering::SeqCst),
            1,
            "one book, one ask; summary: {summary}"
        );
        assert!(stored.len() <= 4, "the per-book cap: {stored:?}");
    }
}
