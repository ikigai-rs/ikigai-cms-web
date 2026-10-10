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
///   CALLER's capability, so they can read nothing the caller could not read directly. The
///   transform is public only with the room's own stylesheets: the door refuses a signed-out
///   caller any other `stylesheet=`, by value or by reference (ledger #1039).
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

/// A stylesheet that proves it ran: its whole output is this marker.
const INLINE_RAN: &str = "INLINE-STYLESHEET-RAN";

fn inline_stylesheet() -> String {
    format!(
        "<xsl:stylesheet version=\"1.0\" xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\">\
         <xsl:output method=\"text\"/><xsl:template match=\"/\">{INLINE_RAN}</xsl:template>\
         </xsl:stylesheet>"
    )
}

/// Every byte but an ASCII letter or digit, percent-encoded: a value as a `/r/` query carries it.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Ledger #1039 (Brian's decision on ledger #916): a signed-out caller may style only with the
/// room's own named stylesheets. Reproduced on 74f77c2, where an anonymous
/// `GET /r/urn:xslt:transform` carrying the stylesheet by value ran it and answered the marker.
/// Every other spelling of "a stylesheet the room does not own" is refused the same way.
#[test]
fn a_signed_out_visitor_cannot_supply_a_stylesheet() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, _) = room(dir.path());
    let inline = enc(&inline_stylesheet());
    for target in [
        // By value: the reproduction.
        format!("urn:xslt:transform?stylesheet={inline}&content=%3Ca%2F%3E"),
        // By value after leading whitespace, which the endpoint still reads as a document.
        format!("urn:xslt:transform?stylesheet=%20%0A{inline}&content=%3Ca%2F%3E"),
        // A room style first and the caller's second: the request carries the last.
        format!(
            "urn:xslt:transform?stylesheet=urn:cms:style:review\
             &stylesheet={inline}&content=%3Ca%2F%3E"
        ),
        // By reference to a public resource that is not a room style.
        "urn:xslt:transform?stylesheet=urn:cms:bookmarks&content=%3Ca%2F%3E".to_string(),
        // A case variant of the door (the kernel does not resolve it; the door refuses it first).
        format!("URN:XSLT:TRANSFORM?stylesheet={inline}&content=%3Ca%2F%3E"),
    ] {
        let body = public_get(&kernel, &target);
        assert!(
            !body.contains(INLINE_RAN),
            "a signed-out GET ran its own stylesheet: {target} -> {body}"
        );
        assert!(
            body.contains("cms-error") && body.contains("denied"),
            "{target}: refused, but not as a denial: {body}"
        );
    }
}

/// The typed form of that refusal, and what is NOT refused: no `stylesheet` at all is the
/// endpoint's own missing-argument answer, and every other resource passes the door untouched.
#[test]
fn the_door_refuses_a_foreign_stylesheet_as_denied() {
    let xslt = |stylesheet: Option<ikigai_core::ArgRef>| {
        let mut r = Request::new(Verb::Source, Iri::parse("urn:xslt:transform").unwrap())
            .with_arg("content", ikigai_core::ArgRef::Inline(b"<a/>".to_vec()));
        if let Some(s) = stylesheet {
            r = r.with_arg("stylesheet", s);
        }
        r
    };
    let inline = |s: &str| Some(ikigai_core::ArgRef::Inline(s.as_bytes().to_vec()));
    for refused in [
        inline(&inline_stylesheet()),
        inline("urn:sparql:construct"),
        inline("https://example.org/evil.xsl"),
        inline(" urn:cms:style:review"),
        Some(ikigai_core::ArgRef::Reference(
            Iri::parse("urn:cms:style:review").unwrap(),
        )),
        Some(ikigai_core::ArgRef::Inline(vec![0xff, 0xfe])),
    ] {
        let shown = format!("{refused:?}");
        match super::refuse_anonymous_stylesheet(&xslt(refused)) {
            Err(Error::Denied(m)) => assert!(m.contains("urn:cms:style:"), "{m}"),
            other => panic!("{shown}: expected Denied, got {other:?}"),
        }
    }
    for admitted in [
        None,
        inline("urn:cms:style:review"),
        inline("urn:cms:style:catalog"),
    ] {
        let shown = format!("{admitted:?}");
        assert!(
            super::refuse_anonymous_stylesheet(&xslt(admitted)).is_ok(),
            "{shown} was refused"
        );
    }
    let other = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap()).with_arg(
        "stylesheet",
        ikigai_core::ArgRef::Inline(inline_stylesheet().into_bytes()),
    );
    assert!(super::refuse_anonymous_stylesheet(&other).is_ok());
}

/// What the rule must not cost: the room's own stylesheets still style a signed-out caller's
/// document, and a signed-in session may still send its own.
#[test]
fn named_styles_stay_public_and_a_signed_in_reader_may_send_a_stylesheet() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, entitlement) = room(dir.path());
    let review = public_get(
        &kernel,
        "urn:xslt:transform?stylesheet=urn:cms:style:review\
         &content=%3Crev%3Areview%20xmlns%3Arev%3D%22urn%3Acms%3Areview%23%22%2F%3E",
    );
    assert!(
        review.contains(".cms-review") && !review.contains("cms-error"),
        "a room style no longer serves a signed-out caller: {review}"
    );

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
    let (_s, _c, body) = resolve_http(
        &kernel,
        &RecentLog::default(),
        &cap,
        who.as_deref(),
        &format!(
            "urn:xslt:transform?stylesheet={}&content=%3Ca%2F%3E",
            enc(&inline_stylesheet())
        ),
    );
    let body = String::from_utf8_lossy(&body);
    assert_eq!(body.trim(), INLINE_RAN, "signed in: {body}");
}

/// The wire's anonymous door is the same door: a never-signed-in WebTransport session is refused
/// its own stylesheet on `Issue` and on `IssueAs`, and a signed-in one is served.
#[test]
fn an_anonymous_wire_session_cannot_supply_a_stylesheet() {
    use ikigai_wire::{decode, encode, Call, Reply};
    let dir = tempfile::tempdir().unwrap();
    let (kernel, entitlement) = room(dir.path());
    let rp = ikigai_cms_web::session::Rp::new(
        "localhost",
        "http://localhost:8080",
        std::sync::Arc::new(ikigai_secret::FileBackend::new(dir.path().join("keys"))),
    )
    .expect("a relying party over a scratch keystore");
    let req = Request::new(Verb::Source, Iri::parse("urn:xslt:transform").unwrap())
        .with_arg(
            "stylesheet",
            ikigai_core::ArgRef::Inline(inline_stylesheet().into_bytes()),
        )
        .with_arg("content", ikigai_core::ArgRef::Inline(b"<a/>".to_vec()));
    let ask = |session: &mut super::Session, call: Call| {
        let reply = super::handle(
            &kernel,
            &rp,
            &HttpAuth::default(),
            &entitlement,
            &RecentLog::default(),
            session,
            &encode(&call).unwrap(),
            ikigai_cms_web::config::DEFAULT_ANONYMOUS_SPARQL_BUDGET,
        );
        decode::<Reply>(&reply).unwrap()
    };
    let mut anonymous = super::Session {
        ceiling: Capability::scoped(Vec::<String>::new()),
        auth_state: None,
        reg_state: None,
        principal: None,
    };
    for call in [
        Call::Issue(req.clone()),
        Call::IssueAs(req.clone(), Capability::root()),
    ] {
        match ask(&mut anonymous, call) {
            Reply::Error(e) => assert!(e.contains("denied"), "{e}"),
            other => panic!("an anonymous wire session ran its own stylesheet: {other:?}"),
        }
    }
    let mut signed_in = super::Session {
        ceiling: Capability::scoped(entitlement.clone()),
        auth_state: None,
        reg_state: None,
        principal: Some("reader".into()),
    };
    match ask(&mut signed_in, Call::Issue(req)) {
        Reply::Resolved(repr, _) => {
            assert_eq!(String::from_utf8_lossy(&repr.bytes).trim(), INLINE_RAN)
        }
        other => panic!("signed in: {other:?}"),
    }
}

/// The public door still hands the XSLT engine a CALLER'S DOCUMENT (`content=`) under a room
/// style, so the room must run an engine that bounds caller XML: ikigai-xslt 0.2.1 (ledger #916).
/// Below it, ~114 nested elements abort the release process and ~24 the debug one. Here a
/// signed-out caller's 300-deep document, and a signed-in caller's stylesheet nesting 40
/// brackets in one XPath, are each answered with a refusal rather than taking the room down.
#[test]
fn caller_xml_too_deep_is_refused_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let (kernel, entitlement) = room(dir.path());
    let deep = format!("{}{}", "%3Ca%3E".repeat(300), "%3C%2Fa%3E".repeat(300));
    let body = public_get(
        &kernel,
        &format!("urn:xslt:transform?stylesheet=urn:cms:style:review&content={deep}"),
    );
    assert!(body.contains("cms-error"), "{body}");

    let brackets = format!("{}1{}", "(".repeat(40), ")".repeat(40));
    let style = format!(
        "<xsl:stylesheet version=\"1.0\" xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\">\
         <xsl:template match=\"/\"><xsl:value-of select=\"{brackets}\"/></xsl:template>\
         </xsl:stylesheet>"
    );
    let cap = Capability::scoped(entitlement);
    let (_s, _c, body) = resolve_http(
        &kernel,
        &RecentLog::default(),
        &cap,
        Some("reader"),
        &format!(
            "urn:xslt:transform?stylesheet={}&content=%3Ca%2F%3E",
            enc(&style)
        ),
    );
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("cms-error"), "{body}");
}
