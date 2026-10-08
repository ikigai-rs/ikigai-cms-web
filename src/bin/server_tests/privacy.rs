//! A signed-out visitor reads nothing private through the HTTP face.
//!
//! Every test here drives the `/r/` route's own two steps — [`session_cap`] with no cookie and
//! `dev_open` off, then [`resolve_http`] (or the same `Resolver::issue_as` it calls, where the
//! typed error matters) — so what is asserted is what `GET /r/{iri}` does, not what a kernel
//! test would like it to do.
//!
//! Two shapes leaked before this file existed, both because a resource declared nothing for the
//! verb `/r/` issues, and the kernel's floor admits an undeclared verb:
//!
//! - **Reads.** The tag overlays, the Zotero link overlay, the link-check status, the review and
//!   the purge prompts read their files with `std::fs`, so neither the kernel's floor nor the fs
//!   jail's per-path gate stood in front of them.
//! - **Writes through a GET.** `urn:cms:tag-approve`, `urn:cms:tag-reject` and
//!   `urn:cms:link-keep` are Sinks that never looked at the verb, so a `Source` (which is all
//!   `/r/` issues, and which carries no declaration) ran the write with no grant at all.
//!
//! [`every_resource_the_http_face_serves_is_refused_to_a_signed_out_visitor`] is the class
//! test: it walks the catalog, so a resource added later without a declaration fails here.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{resolve_http, session_cap, HttpAuth, HttpSession};
use ikigai_cms_web::session::RecentLog;
use ikigai_cms_web::tagstore::TagPaths;
use ikigai_core::{Capability, Error, Iri, Kernel, Request, Verb};
use ikigai_resolve::Resolver;

/// The resources a signed-out visitor may read, each for a stated reason. Anything the kernel
/// binds that is not here must refuse the public ceiling: adding a resource means either giving
/// it a `requires` or adding it here with its reason, in review.
///
/// - `urn:cms:style:{name}`: the reading-room stylesheets, embedded at build time.
/// - `urn:cms:bookmarks`: ikigai-cms's by-value transreptor (org text in, Turtle out).
/// - `urn:sparql:{select,ask,describe,construct}` and `urn:xslt:transform`: pure compute over
///   what the caller hands them; every `graph=` / `stylesheet=` they resolve is issued under the
///   CALLER's capability, so they can read nothing the caller could not read directly.
/// - `urn:kernel:actions` and `urn:kernel:validate`: the kernel's own operations that core leaves
///   open. The manifold is filtered by the CALLER's capability, so the public ceiling is offered
///   only this list; validation is a pre-flight that runs nothing. Every other `urn:kernel:*`
///   operation declares its own requirement, and a core release that opens a new one fails here
///   until someone decides it belongs on this list.
const PUBLIC: &[&str] = &[
    "urn:kernel:actions",
    "urn:kernel:validate",
    "urn:cms:style:{name}",
    "urn:cms:bookmarks",
    "urn:sparql:select",
    "urn:sparql:ask",
    "urn:sparql:describe",
    "urn:sparql:construct",
    "urn:xslt:transform",
];

/// The private strings the fixture plants, one per store a leak could come from.
const SECRET_TAG: &str = "private-approved-tag";
const SECRET_SUGGESTION: &str = "private-suggested-tag";
const SECRET_DISMISSAL: &str = "private-dismissed-tag";
const SECRET_ZOTERO: &str = "SECRETITEM";
const SECRET_URL: &str = "https://private.example/secret-bookmark";

/// A room over `dir` with every optional binding present: the Zotero library, a deck root, the
/// overlays and the status cache all live in the tempdir (never the data home), each seeded with
/// one private row. Returns the kernel and the signed-in entitlement the server would grant.
fn room(dir: &Path) -> (Kernel, Vec<String>) {
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(
        &bm,
        format!(
            "* Bookmarks\n** [[{SECRET_URL}][Private Bookmark]]\n   \
             :PROPERTIES:\n   :TAGS: science\n   :END:\n"
        ),
    )
    .unwrap();
    let zotero = dir.join("library.rdf");
    std::fs::write(
        &zotero,
        "<?xml version=\"1.0\"?>\n<rdf:RDF \
         xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"></rdf:RDF>\n",
    )
    .unwrap();
    let decks = dir.join("decks");
    std::fs::create_dir_all(&decks).unwrap();

    let tags = TagPaths::in_dir(dir);
    tags.add_suggestion("urn:cms:bookmark:secret-item", SECRET_SUGGESTION);
    tags.approve("urn:cms:bookmark:secret-item", SECRET_TAG);
    tags.dismiss("urn:cms:bookmark:secret-item", SECRET_DISMISSAL);
    std::fs::write(
        &tags.zotero_links,
        format!(
            "<urn:cms:book:x> <https://ikigai-rs.dev/ns/cms#readerUrl> \
             \"https://www.zotero.org/u/items/{SECRET_ZOTERO}\" .\n"
        ),
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        status_path(dir),
        format!(
            "[{{\"url\":\"{SECRET_URL}\",\"subject\":\"urn:cms:bookmark:s\",\
             \"title\":\"Private Bookmark\",\"status\":\"gone\",\"reason\":\"404\",\
             \"checked_at\":{now},\"first_broken_at\":{},\"broken_count\":9}}]",
            now - 30 * 86400
        ),
    )
    .unwrap();

    let kernel = ikigai_cms_web::build_cms_kernel_with(
        dir.to_path_buf(),
        Some(zotero),
        Some(ikigai_cms_web::Presentations {
            root: decks.clone(),
            base_url: None,
        }),
        None,
        tags,
        Some(status_path(dir)),
    );
    let entitlement = vec![
        format!("urn:cap:fs:read:{}", dir.display()),
        format!("urn:cap:fs:read:{}", decks.display()),
    ];
    (kernel, entitlement)
}

fn status_path(dir: &Path) -> PathBuf {
    dir.join("cms-linkstatus.json")
}

/// `GET /r/{target}` with no cookie and `dev_open` off: the body the visitor receives.
fn public_get(kernel: &Kernel, target: &str) -> String {
    let (cap, principal) = session_cap(&HttpAuth::default(), None, &[], false);
    let (_s, _c, body) = resolve_http(
        kernel,
        &RecentLog::default(),
        &cap,
        principal.as_deref(),
        target,
    );
    String::from_utf8_lossy(&body).into_owned()
}

/// The same request, with the typed outcome instead of the rendered fragment.
fn public_source(kernel: &Kernel, iri: &str) -> ikigai_core::Result<String> {
    let (cap, _) = session_cap(&HttpAuth::default(), None, &[], false);
    let request = Request::new(Verb::Source, Iri::parse(iri).expect("valid IRI"));
    Resolver::issue_as(kernel, request, &cap)
        .map(|(repr, _)| String::from_utf8_lossy(&repr.bytes).into_owned())
}

/// Every file under `dir`, path → bytes: what a refused request must leave exactly as it was.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(at: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(at).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else {
                out.insert(path.clone(), std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

/// The leak as reproduced: each private store, read over `/r/` by a visitor with no session.
/// The bookmark graph beside them is the control, gated all along.
#[test]
fn a_signed_out_visitor_cannot_read_the_private_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _) = room(dir.path());

    let control = public_get(&kernel, "urn:cms:graph:bookmarks");
    assert!(
        !control.contains("secret-bookmark"),
        "control: the bookmarks graph is gated: {control}"
    );

    let mut leaks = Vec::new();
    for (iri, secret) in [
        ("urn:cms:graph:tags-approved", SECRET_TAG),
        ("urn:cms:graph:suggestions", SECRET_SUGGESTION),
        ("urn:cms:graph:dismissed", SECRET_DISMISSAL),
        ("urn:cms:graph:zotero-links", SECRET_ZOTERO),
        ("urn:cms:review", "private.example/secret-bookmark"),
        // The indicator and the purge prompt carry counts, not URLs: the tally of a private
        // collection is still the collection's.
        ("urn:cms:linkstatus", "links:"),
        ("urn:cms:purge", "Remove <b>"),
        ("urn:cms:purge-domains", "cms-purge"),
        ("urn:cms:purge-unreachable", "cms-purge"),
        // The same overlays one hop away: a public SPARQL engine resolves `graph=` under the
        // caller's capability, so it reads exactly what the visitor could read directly.
        (
            "urn:sparql:select?query=SELECT%20%3Ft%20WHERE%20%7B%20%3Fs%20%3Fp%20%3Ft%20%7D\
             &graph=urn:cms:graph:tags-approved",
            SECRET_TAG,
        ),
    ] {
        let body = public_get(&kernel, iri);
        if body.contains(secret) {
            leaks.push(format!(
                "{iri} -> {}",
                body.chars().take(160).collect::<String>()
            ));
        }
    }
    assert!(
        leaks.is_empty(),
        "signed-out /r/ leaked:\n{}",
        leaks.join("\n")
    );
}

/// `/r/` issues `Source`, and a Sink that never looked at its verb ran its write for it.
#[test]
fn a_signed_out_get_cannot_run_a_sink() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _) = room(dir.path());
    let before = snapshot(dir.path());
    for target in [
        "urn:cms:tag-approve?book=urn:cms:bookmark:x&tag=planted",
        "urn:cms:tag-reject?book=urn:cms:bookmark:secret-item&tag=private-suggested-tag",
        "urn:cms:link-keep?url=https%3A%2F%2Fprivate.example%2Fsecret-bookmark",
        "urn:cms:link-remove?url=https%3A%2F%2Fprivate.example%2Fsecret-bookmark",
    ] {
        let body = public_get(&kernel, target);
        assert!(
            body.contains("cms-error"),
            "{target} answered as though it ran: {body}"
        );
    }
    assert!(
        before == snapshot(dir.path()),
        "a signed-out GET changed a file"
    );
}

/// The class: every resource the kernel binds, walked from the catalog. A non-public resource
/// must DECLARE a requirement for every verb it serves (so the manifold stops offering it to
/// the public ceiling), must REFUSE the visitor's `Source` — `Denied` by the floor where it
/// serves Source, refused as not served where it does not — and must refuse every verb it does
/// not declare, even to root, all without touching a file.
#[test]
fn every_resource_the_http_face_serves_is_refused_to_a_signed_out_visitor() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _) = room(dir.path());
    let before = snapshot(dir.path());
    let entries = kernel.entries().expect("the room's root space enumerates");

    let mut seen_public = Vec::new();
    let mut problems = Vec::new();
    for entry in &entries {
        let pattern = entry.pattern.as_str();
        let description = kernel
            .describe_pattern(pattern)
            .unwrap_or_else(|| panic!("{pattern} is bound but describes nothing"));
        let specs = description.action_specs();
        let source = specs.iter().find(|s| s.verb == Verb::Source);
        // A concrete IRI for a template: the floor runs before the endpoint sees its variables.
        let iri = fill(pattern);

        if PUBLIC.contains(&pattern) {
            seen_public.push(pattern.to_string());
            continue;
        }
        for spec in &specs {
            if spec.verb != Verb::Meta && spec.requires.is_empty() {
                problems.push(format!("{pattern}: {:?} declares no requires", spec.verb));
            }
        }
        match (source.is_some(), public_source(&kernel, &iri)) {
            (_, Ok(body)) => problems.push(format!(
                "{pattern}: a signed-out Source resolved: {}",
                body.chars().take(120).collect::<String>()
            )),
            (true, Err(Error::Denied(_))) => {}
            (false, Err(e)) if not_served(&e) => {}
            // Since ikigai-core 0.1.85 the floor covers EVERY verb (ledger #750), so a Source to a
            // resource that declares only Sink is refused by its declared `requires` before the
            // endpoint ever sees it: a refusal, and an earlier one than "does not serve".
            (false, Err(Error::Denied(_))) => {}
            // The kernel's own operations refuse an undeclared verb themselves (as unresolved).
            (false, Err(_)) if pattern.starts_with("urn:kernel:") => {}
            (true, Err(other)) => problems.push(format!(
                "{pattern}: refused, but not by the floor (Denied): {other}"
            )),
            // A missing argument is not a refusal: it is the endpoint getting as far as reading
            // its arguments, which a well-formed request would carry past.
            (false, Err(other)) => problems.push(format!(
                "{pattern}: declares no Source, yet Source reached the endpoint: {other}"
            )),
        }
        // And over the wire, where a client picks the verb: a verb the description does not
        // declare carries no requirement, so the floor admits it to anyone, the public ceiling
        // included. Issued here under ROOT so only the endpoint's own refusal can stop it.
        for verb in [Verb::Source, Verb::Sink, Verb::Exists, Verb::Delete] {
            if specs.iter().any(|s| s.verb == verb) || pattern.starts_with("urn:kernel:") {
                continue;
            }
            let request = Request::new(verb, Iri::parse(iri.as_str()).expect("valid IRI"));
            match Resolver::issue_as(&kernel, request, &Capability::root()) {
                Err(e) if not_served(&e) => {}
                Err(other) => problems.push(format!(
                    "{pattern}: undeclared {verb:?} reached the endpoint: {other}"
                )),
                Ok(_) => problems.push(format!("{pattern}: undeclared {verb:?} was served")),
            }
        }
    }
    assert!(
        problems.is_empty(),
        "resources a signed-out visitor is not refused on:\n{}",
        problems.join("\n")
    );
    // The public set is a list of real bindings, not a wish list: a stale entry would let a
    // future resource reuse the name unexamined.
    for name in PUBLIC {
        assert!(
            seen_public.iter().any(|p| p == name),
            "{name} is listed as public but not bound"
        );
    }
    assert!(before == snapshot(dir.path()), "the walk changed a file");
}

/// The refusal `serves` gives a verb an endpoint does not declare (an `Endpoint` error: the
/// kernel has no "method not allowed" variant, and a wrong verb is not a capability denial).
fn not_served(e: &Error) -> bool {
    matches!(e, Error::Endpoint(m) if m.contains("does not serve"))
}

/// `urn:cms:view:{tag}` → `urn:cms:view:probe`.
fn fill(pattern: &str) -> String {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}').map_or(rest.len(), |c| open + c + 1);
        out.push_str("probe");
        rest = &rest[close..];
    }
    out.push_str(rest);
    out
}

/// What the gate must not cost: a signed-in reader still reads every overlay, the review and the
/// indicator through the same route, and the union still carries the overlay rows.
#[test]
fn a_signed_in_reader_still_reads_the_overlays_and_the_review() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, entitlement) = room(dir.path());
    let auth = HttpAuth::default();
    auth.sessions.lock().unwrap().insert(
        "sid".into(),
        HttpSession {
            scopes: Some(entitlement.clone()),
            principal: Some("reader".into()),
            pending: None,
            wire_token: None,
        },
    );
    let (cap, who) = session_cap(&auth, Some("sid"), &entitlement, false);
    let recent = RecentLog::default();
    let get = |target: &str| {
        let (_s, _c, body) = resolve_http(&kernel, &recent, &cap, who.as_deref(), target);
        String::from_utf8_lossy(&body).into_owned()
    };
    for (iri, expect) in [
        ("urn:cms:graph:tags-approved", SECRET_TAG),
        ("urn:cms:graph:suggestions", SECRET_SUGGESTION),
        ("urn:cms:graph:dismissed", SECRET_DISMISSAL),
        ("urn:cms:graph:zotero-links", SECRET_ZOTERO),
        ("urn:cms:graph", SECRET_TAG),
        ("urn:cms:review", "private.example/secret-bookmark"),
        ("urn:cms:linkstatus", "links:"),
        ("urn:cms:purge", "Remove <b>"),
        ("urn:cms:view:science?style=catalog", "Private Bookmark"),
    ] {
        let body = get(iri);
        assert!(body.contains(expect), "{iri} under the session: {body}");
    }
    // The session's ceiling is read-only: the Sinks stay refused to it over `/r/` too, which
    // is why the action routes run elevated behind their own signed-in check.
    let before = snapshot(dir.path());
    assert!(get("urn:cms:tag-approve?book=urn:cms:bookmark:x&tag=planted").contains("cms-error"));
    assert!(before == snapshot(dir.path()));
}

/// `dev_open` elevates an anonymous request to the room's entitlement (localhost dev): the
/// gate must follow the entitlement, not the cookie, or dev mode would show an empty room.
#[test]
fn dev_open_still_reads_the_overlays() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, entitlement) = room(dir.path());
    let (cap, _) = session_cap(&HttpAuth::default(), None, &entitlement, true);
    let (_s, _c, body) = resolve_http(
        &kernel,
        &RecentLog::default(),
        &cap,
        None,
        "urn:cms:graph:tags-approved",
    );
    assert!(String::from_utf8_lossy(&body).contains(SECRET_TAG));
}
