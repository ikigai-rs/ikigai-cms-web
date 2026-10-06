//! A human decision is never silently lost — the file-safety set.
//!
//! The room's state lives in files with more than one writer: the bookmarks org file (the room's
//! purge and remove, org-capture, a Dropbox sync), the link status cache (the link-check pass, the
//! room's keep/remove/purge), and the tag overlays (the room's approve/dismiss, the tag-suggest
//! pass). Each test here is one way a decision used to vanish while every caller was told it
//! succeeded:
//!
//! - a keep (or a purge) made while a link-check pass runs was overwritten by the pass's stale copy
//!   of the status cache;
//! - concurrent tag decisions were whole-file rewrites with no lock, and most of them were lost;
//! - the bookmarks file was cached and unwatched, so a purge or remove struck a stale copy and
//!   wrote it back over an edit made outside the room — and backed up the same stale copy.
//!
//! Every test uses a scratch directory; nothing here touches a real path.

use std::path::{Path, PathBuf};
#[cfg(feature = "maintenance")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

use ikigai_cms_web::build_cms_kernel_with;
use ikigai_cms_web::tagstore::{entries, TagPaths};
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};

const BM: &str = "old-org/pinboard-bookmarks.org";

fn write_org(dir: &Path, org: &str) -> PathBuf {
    let bm = dir.join(BM);
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(&bm, org).unwrap();
    bm
}

fn room(dir: &Path) -> Kernel {
    build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        None,
        None,
        TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    )
}

fn run(k: &Kernel, req: Request) -> String {
    let r = futures::executor::block_on(k.issue(req, &Capability::root())).expect("resolves");
    String::from_utf8_lossy(&r.bytes).into_owned()
}

fn sink(target: &str, args: &[(&str, &str)]) -> Request {
    let mut req = Request::new(Verb::Sink, Iri::parse(target).unwrap());
    for (k, v) in args {
        req = req.with_arg(*k, ArgRef::Inline(v.as_bytes().to_vec()));
    }
    req
}

fn source(target: &str) -> Request {
    Request::new(Verb::Source, Iri::parse(target).unwrap())
}

fn edit_outside_the_room(file: &Path) {
    let mut t = std::fs::read_to_string(file).unwrap();
    t.push_str(
        "** [[https://new.example/][Captured Today]]\n   :PROPERTIES:\n   :TAGS: new\n   :END:\n",
    );
    std::fs::write(file, t).unwrap();
}

// ---- the bookmarks file: an edit made outside the room survives the room's writes ------------------

/// The room has read the bookmarks file (any render does), the user then captures a bookmark into
/// it from their editor, and a review card's "remove" strikes another link. The capture must
/// survive, and the backup must be the file as it was just before the strike — capture included.
#[cfg(feature = "maintenance")]
#[test]
fn removing_one_link_does_not_destroy_an_external_edit() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_org(
        dir.path(),
        "* Bookmarks\n** [[https://dead.example/][Dead]]\n** [[https://alive.example/][Alive]]\n",
    );
    let k = room(dir.path());
    // A double-clicked remove: the first strikes, the second is the documented no-op — which
    // used to leave the read it made cached, with nothing cutting it.
    run(
        &k,
        sink("urn:cms:link-remove", &[("url", "https://dead.example/")]),
    );
    run(
        &k,
        sink("urn:cms:link-remove", &[("url", "https://dead.example/")]),
    );
    edit_outside_the_room(&file);
    run(
        &k,
        sink("urn:cms:link-remove", &[("url", "https://alive.example/")]),
    );

    let after = std::fs::read_to_string(&file).unwrap();
    let bak = std::fs::read_to_string(dir.path().join(format!("{BM}.one.bak"))).unwrap();
    assert!(
        !after.contains("alive.example"),
        "the link was struck:\n{after}"
    );
    assert!(
        after.contains("Captured Today"),
        "removing one link erased a bookmark captured outside the room:\n{after}"
    );
    assert!(
        bak.contains("Captured Today") && bak.contains("alive.example"),
        "the backup is the file as it was just before the strike:\n{bak}"
    );
}

/// The bulk purge, the same way: a render, an outside edit, then a purge of the 404 set.
#[cfg(feature = "maintenance")]
#[test]
fn a_purge_does_not_destroy_an_external_edit() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_org(
        dir.path(),
        "* Bookmarks\n** [[https://dead.example/x][Dead One]]\n** [[https://alive.example/][Alive]]\n",
    );
    std::fs::write(
        dir.path().join("cms-linkstatus.json"),
        r#"[{"url":"https://dead.example/x","subject":"urn:cms:bookmark:d","title":"Dead One","status":"gone","reason":"HTTP 404/410 (GET-confirmed)","checked_at":1,"first_broken_at":1,"broken_count":3,"kept_at":0}]"#,
    )
    .unwrap();
    let k = room(dir.path());
    run(&k, source("urn:cms:graph:bookmarks"));
    edit_outside_the_room(&file);
    run(&k, sink("urn:cms:purge", &[]));

    let after = std::fs::read_to_string(&file).unwrap();
    let bak = std::fs::read_to_string(dir.path().join(format!("{BM}.bak"))).unwrap();
    assert!(
        !after.contains("dead.example"),
        "the purge struck the dead link:\n{after}"
    );
    assert!(
        after.contains("Captured Today") && after.contains("alive.example"),
        "the purge erased a bookmark captured outside the room:\n{after}"
    );
    assert!(
        bak.contains("Captured Today"),
        "the backup is the file as it was just before the purge:\n{bak}"
    );
}

/// The read side of the same root cause: once the room has rendered, an edit made outside it must
/// still reach the room — not wait for a restart.
#[test]
fn an_external_edit_reaches_the_room() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_org(
        dir.path(),
        "* Bookmarks\n** [[https://sci.example/][Science]]\n",
    );
    let k = room(dir.path());
    assert!(!run(&k, source("urn:cms:graph")).contains("Captured Today"));
    edit_outside_the_room(&file);
    assert!(
        run(&k, source("urn:cms:graph")).contains("Captured Today"),
        "the room still serves the bookmarks file as it was before the edit"
    );
}

// ---- the tag overlays: concurrent decisions are all kept ----------------------------------------

/// Sixteen `+` clicks at once — the page server runs each POST on its own task — split across TWO
/// kernels over the same overlay files, as the serving kernel and the server's maintenance kernel
/// are. Every approval was reported as done, so every approval must be on disk.
#[test]
fn concurrent_approvals_are_all_kept() {
    const N: usize = 16;
    let dir = tempfile::tempdir().unwrap();
    write_org(dir.path(), "* Bookmarks\n");
    let tags = TagPaths::in_dir(dir.path());
    for i in 0..N {
        tags.add_suggestion(&format!("urn:cms:bookmark:b{i}"), "t");
    }
    let kernels = [Arc::new(room(dir.path())), Arc::new(room(dir.path()))];
    let gate = Arc::new(Barrier::new(N));
    let threads: Vec<_> = (0..N)
        .map(|i| {
            let k = Arc::clone(&kernels[i % 2]);
            let gate = Arc::clone(&gate);
            std::thread::spawn(move || {
                gate.wait();
                run(
                    &k,
                    sink(
                        "urn:cms:tag-approve",
                        &[("book", &format!("urn:cms:bookmark:b{i}")), ("tag", "t")],
                    ),
                );
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(
        entries(&tags.approved).len(),
        N,
        "every approval returned OK, so every approval is on disk"
    );
    assert!(
        entries(&tags.suggestions).is_empty(),
        "every approved suggestion left the suggestions overlay: {:?}",
        entries(&tags.suggestions)
    );
}

/// The tag-suggest pass writing suggestions while the human approves and dismisses others: three
/// writers on the suggestions overlay at once, and each one's change must survive the others'.
#[test]
fn suggestions_written_during_decisions_are_all_kept() {
    const N: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let tags = TagPaths::in_dir(dir.path());
    for i in 0..N {
        tags.add_suggestion(&format!("urn:cms:bookmark:a{i}"), "t");
        tags.add_suggestion(&format!("urn:cms:bookmark:r{i}"), "t");
    }
    let gate = Arc::new(Barrier::new(3 * N));
    let mut threads = Vec::new();
    for i in 0..N {
        for role in 0..3 {
            let (tags, gate) = (tags.clone(), Arc::clone(&gate));
            threads.push(std::thread::spawn(move || {
                gate.wait();
                match role {
                    0 => assert!(tags.approve(&format!("urn:cms:bookmark:a{i}"), "t")),
                    1 => assert!(tags.reject(&format!("urn:cms:bookmark:r{i}"), "t")),
                    _ => tags.add_suggestion(&format!("urn:cms:bookmark:n{i}"), "t"),
                }
            }));
        }
    }
    for t in threads {
        t.join().unwrap();
    }
    let sug: Vec<String> = entries(&tags.suggestions)
        .into_iter()
        .map(|e| e.iri)
        .collect();
    assert_eq!(entries(&tags.approved).len(), N, "approvals kept");
    assert_eq!(entries(&tags.dismissed).len(), N, "dismissals kept");
    assert_eq!(
        sug.len(),
        N,
        "only the pass's new suggestions remain, all of them: {sug:?}"
    );
    assert!(sug.iter().all(|iri| iri.contains(":n")), "{sug:?}");
}

// ---- the status cache: a link-check pass never overwrites a decision made while it ran ------------

/// A transport that announces its first request and then holds every request until released, so
/// the test can act in the room while a pass is mid-flight. Every request answers 404.
#[cfg(feature = "maintenance")]
struct Gated {
    entered: Arc<AtomicBool>,
    go: Arc<AtomicBool>,
}

#[cfg(feature = "maintenance")]
#[async_trait::async_trait]
impl ikigai_http::HttpTransport for Gated {
    async fn send(
        &self,
        _req: ikigai_http::HttpRequest,
    ) -> std::result::Result<ikigai_http::HttpResponse, String> {
        self.entered.store(true, Ordering::SeqCst);
        while !self.go.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Ok(ikigai_http::HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }
}

/// Start a link-check pass on its own maintenance kernel (as cms-server runs it) and return once
/// its first check is in flight, with the switch that lets it finish.
#[cfg(feature = "maintenance")]
fn start_pass(dir: &Path) -> (std::thread::JoinHandle<Result<(), String>>, Arc<AtomicBool>) {
    let entered = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let maint = ikigai_cms_web::maintenance::maintenance_kernel(
        dir.to_path_buf(),
        None,
        None,
        dir.join("cms-linkstatus.json"),
        TagPaths::in_dir(dir),
        Arc::new(Gated {
            entered: Arc::clone(&entered),
            go: Arc::clone(&go),
        }),
        Arc::new(ikigai_secret::FileBackend::new(dir.join("secrets"))),
        ikigai_llm::Registry::single(ikigai_llm::OpenAiConfig::ollama("llama3.2")),
        "ollama",
    );
    let pass = std::thread::spawn(move || {
        futures::executor::block_on(maint.issue(source("urn:cms:linkcheck"), &Capability::root()))
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    while !entered.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    (pass, go)
}

#[cfg(feature = "maintenance")]
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A link flagged 404 by the last pass; the human opens it, it works (a bot-walled publisher),
/// and they click keep — while the next pass is running. The keep must survive the pass, and the
/// bulk 404 purge afterwards must leave the kept bookmark alone.
#[cfg(feature = "maintenance")]
#[test]
fn a_keep_during_a_link_check_pass_survives_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_org(
        dir.path(),
        "* Bookmarks\n** [[https://walled.example/page][Bot-walled but alive]]\n",
    );
    let day_ago = now() - 86_400;
    std::fs::write(
        dir.path().join("cms-linkstatus.json"),
        format!(
            r#"[{{"url":"https://walled.example/page","subject":"urn:cms:bookmark:x","title":"Bot-walled but alive","status":"gone","reason":"HTTP 404/410 (GET-confirmed)","checked_at":{day_ago},"first_broken_at":{day_ago},"broken_count":1,"kept_at":0}}]"#
        ),
    )
    .unwrap();
    let serving = room(dir.path());

    let (pass, go) = start_pass(dir.path());
    run(
        &serving,
        sink(
            "urn:cms:link-keep",
            &[("url", "https://walled.example/page")],
        ),
    );
    let status = dir.path().join("cms-linkstatus.json");
    assert!(
        ikigai_cms_web::maintenance::load_status(&status)["https://walled.example/page"].kept_at
            > 0,
        "the keep landed"
    );
    go.store(true, Ordering::SeqCst);
    pass.join().unwrap().expect("the pass runs");

    let after = ikigai_cms_web::maintenance::load_status(&status);
    let entry = &after["https://walled.example/page"];
    assert!(
        entry.kept_at > 0,
        "the keep made during the pass was overwritten by it"
    );
    assert_eq!(entry.status, "gone", "what the pass learned landed too");
    assert!(
        entry.checked_at >= now() - 60,
        "the pass's check is recorded"
    );

    run(&serving, sink("urn:cms:purge", &[]));
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("walled.example"),
        "the bulk purge struck the bookmark the human kept"
    );
}

/// A purge clicked while a pass runs: the purged links leave the status cache, and the pass —
/// which loaded them at its start and checked them — must not write them back, where they would
/// sit in the review as candidates for bookmarks that no longer exist.
#[cfg(feature = "maintenance")]
#[test]
fn a_purge_during_a_link_check_pass_is_not_undone() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_org(
        dir.path(),
        "* Bookmarks\n** [[https://dead.example/a][Dead A]]\n** [[https://alive.example/][Alive]]\n",
    );
    let day_ago = now() - 86_400;
    std::fs::write(
        dir.path().join("cms-linkstatus.json"),
        format!(
            r#"[{{"url":"https://dead.example/a","subject":"urn:cms:bookmark:a","title":"Dead A","status":"gone","reason":"HTTP 404/410 (GET-confirmed)","checked_at":{day_ago},"first_broken_at":{day_ago},"broken_count":2,"kept_at":0}}]"#
        ),
    )
    .unwrap();
    let serving = room(dir.path());

    let (pass, go) = start_pass(dir.path());
    run(&serving, sink("urn:cms:purge", &[]));
    assert!(!std::fs::read_to_string(&file)
        .unwrap()
        .contains("dead.example"));
    go.store(true, Ordering::SeqCst);
    pass.join().unwrap().expect("the pass runs");

    let after = ikigai_cms_web::maintenance::load_status(&dir.path().join("cms-linkstatus.json"));
    assert!(
        !after.contains_key("https://dead.example/a"),
        "the pass wrote a purged link back into the status cache: {:?}",
        after.keys().collect::<Vec<_>>()
    );
    assert!(
        after.contains_key("https://alive.example/"),
        "the pass still records what it learned about the links that remain: {:?}",
        after.keys().collect::<Vec<_>>()
    );
}
