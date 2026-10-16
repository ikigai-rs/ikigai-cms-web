//! Everything a person activates in the room can be reached by keyboard (ledger #1640, SC 2.1.1).
//!
//! The tag chips, type chips and recent-trail items were `<a hx-get>` with no `href`. A link
//! without an `href` is not in the tab order and does not answer Enter, so opening a tag or a
//! type, the room's main navigation, needed a mouse: 0 of 4 chips in the tag index and 0 of 120
//! on a page of cards took focus.
//!
//! The fix is progressive enhancement: each such link carries an `href` naming the SAME URL as its
//! `hx-get`. htmx cancels the browser's navigation on click (and Enter is a click) for an anchor
//! whose `href` does not begin with `#`, so the in-place swap is unchanged, and the `href` gives
//! the link its focus and its keyboard activation.
//!
//! The class test below renders every fragment that carries a chip and checks each start tag that
//! asks htmx to GET something: it must be a `<button>`, a `<form>`, or an `<a>` whose `href`
//! equals its `hx-get`. And no `<a>` may lack an `href`. A new chip added without one fails here.
//!
//! The same views also hold this file's pointer check, the target size of the suggestion buttons
//! (ledger #1672, SC 2.5.8), at the end.

use std::path::Path;

use super::resolve_http;
use ikigai_cms_web::session::RecentLog;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_resolve::Resolver;

const ORG: &str = "* Bookmarks\n\
** [[https://quic.example][QUIC Notes]]\n   :PROPERTIES:\n   :TAGS: quic networking\n   :END:\n\
** [[https://cpp.example][Plus Plus Bookmark]]\n   :PROPERTIES:\n   :TAGS: c++\n   :END:\n";

fn room(dir: &Path) -> Kernel {
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(&bm, ORG).unwrap();
    ikigai_cms_web::build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        None,
        None,
        ikigai_cms_web::tagstore::TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    )
}

/// `GET /r/{target}` as a signed-in principal, sharing one recency trail across calls.
fn get(kernel: &Kernel, recent: &RecentLog, target: &str) -> String {
    let (status, _ct, body) =
        resolve_http(kernel, recent, &Capability::root(), Some("reader"), target);
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, "200 OK", "{target}: {body}");
    body
}

/// The value of `name="…"` or `name='…'` in one start tag, if present. The stylesheets' output
/// quotes with `'` (xrust's serializer) and `native.rs` with `"`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    ['"', '\''].into_iter().find_map(|q| {
        let key = format!(" {name}={q}");
        let start = tag.find(&key)? + key.len();
        let len = tag[start..].find(q)?;
        Some(&tag[start..start + len])
    })
}

/// Every start tag in `html` that a keyboard cannot reach although htmx makes it act on a click,
/// and every `<a>` with no `href`; also the number of `<a hx-get>` links seen, so a caller can
/// tell a pass from a fragment with nothing in it.
fn unreachable(html: &str) -> (Vec<String>, usize) {
    let mut bad = Vec::new();
    let mut links = 0;
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        rest = &rest[close + 1..];
        let name: String = tag
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() {
            continue; // an end tag, a comment, a doctype
        }
        let hx_get = attr(tag, "hx-get");
        let href = attr(tag, "href");
        match name.as_str() {
            "a" => {
                if hx_get.is_some() {
                    links += 1;
                }
                if href.is_none() || (hx_get.is_some() && href != hx_get) {
                    bad.push(format!("<{tag}>"));
                }
            }
            "button" | "form" => {}
            _ if hx_get.is_some() => bad.push(format!("<{tag}>")),
            _ => {}
        }
    }
    (bad, links)
}

#[test]
fn every_chip_the_room_renders_is_a_link_with_an_href() {
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    let recent = RecentLog::default();

    let mut fragments = vec![
        ("the tag index", get(&k, &recent, "urn:cms:tags")),
        ("the type index", get(&k, &recent, "urn:cms:types")),
    ];
    for style in ["catalog", "mosaic", "agenda"] {
        fragments.push((
            style,
            get(&k, &recent, &format!("urn:cms:view:quic?style={style}")),
        ));
    }
    // Opening a tag as a signed-in reader put it on the trail.
    fragments.push(("the recent trail", get(&k, &recent, "urn:cms:recent")));

    // A promoted suggestion comes back as a tag chip of its own (native.rs, TagApprove).
    let approve = Request::new(Verb::Sink, Iri::parse("urn:cms:tag-approve").unwrap())
        .with_arg("book", ArgRef::Inline(b"urn:cms:bookmark:x".to_vec()))
        .with_arg("tag", ArgRef::Inline(b"promoted".to_vec()));
    let (chip, _) = Resolver::issue_as(&k, approve, &Capability::root()).expect("approve");
    fragments.push((
        "a promoted suggestion",
        String::from_utf8(chip.bytes).unwrap(),
    ));

    for (what, html) in &fragments {
        let (bad, links) = unreachable(html);
        assert!(links > 0, "{what} rendered no chip to check: {html}");
        assert!(
            bad.is_empty(),
            "{what}: not reachable by keyboard (no href, or an href that is not its hx-get): {bad:#?}"
        );
    }
}

#[test]
fn the_reachability_check_sees_a_link_with_no_href() {
    let (bad, links) = unreachable(
        "<section><a class=\"cms-tag\" hx-get=\"/r/urn:cms:view:x\">#x</a>\
         <a class=\"cms-tag\" href=\"/r/urn:cms:view:y\" hx-get=\"/r/urn:cms:view:z\">#y</a>\
         <span hx-get='/r/urn:cms:tags'>t</span>\
         <a class='cms-tag' hx-get='/r/urn:cms:view:v'>#v</a>\
         <a href=\"/r/urn:cms:view:w\" hx-get=\"/r/urn:cms:view:w\">#w</a>\
         <button hx-get=\"/r/urn:cms:tags\">tags</button></section>",
    );
    assert_eq!(links, 4);
    assert_eq!(bad.len(), 4, "{bad:#?}");
}

// ---- Target size (ledger #1672, SC 2.5.8) ----
//
// The suggestion chip's promote (`+`) and dismiss (`x`) buttons were 13 by 12 px, beside tag
// chips that are links and so targets of their own: axe's `target-size` flagged them in Chrome.
// A target passes SC 2.5.8 at 24 by 24 CSS px whatever is around it, so the stylesheets now give
// each one a `min-width` and `min-height` of 24px. The check below reads that from the markup a
// card view renders: for every `<button>` the fragment's own `<style>` styles, the size its rules
// guarantee. A button styled only by the page shell (`dist/index.html`: the toolbar, the pager,
// the context header's clear button) takes its size from padding and font and is not read here;
// those measured 24 px or more in the browser runs ACCESSIBILITY.md records.

/// The px value of the last `prop:` declaration in a declaration block, if it is in px.
fn px(decls: &str, prop: &str) -> Option<f32> {
    decls
        .split(';')
        .filter_map(|d| d.split_once(':'))
        .filter(|(p, _)| p.trim() == prop)
        .filter_map(|(_, v)| v.trim().strip_suffix("px")?.trim().parse().ok())
        .next_back()
}

/// The `(selectors, declarations)` of every rule in `html`'s `<style>` elements, in source order.
fn style_rules(html: &str) -> Vec<(String, String)> {
    let mut rules = Vec::new();
    let mut rest = html;
    while let Some(open) = rest.find("<style>") {
        rest = &rest[open + "<style>".len()..];
        let Some(close) = rest.find("</style>") else {
            break;
        };
        for rule in rest[..close].split('}') {
            if let Some((sel, decls)) = rule.split_once('{') {
                rules.push((sel.trim().to_string(), decls.to_string()));
            }
        }
        rest = &rest[close..];
    }
    rules
}

/// Every `<button>` in `html` whose own fragment styles it and does not guarantee it 24 by 24 CSS
/// px, with the size it does guarantee; also the number of buttons checked. A rule applies when
/// its selector list names one of the button's classes as a plain class selector (`.cms-sug-no`,
/// not `.cms-sug-no:hover`); the last declaration wins, and the guaranteed size is the larger of
/// `width` and `min-width` (and of `height` and `min-height`), 0 when neither is in px.
fn undersized_buttons(html: &str) -> (Vec<String>, usize) {
    let rules = style_rules(html);
    let mut bad = Vec::new();
    let mut checked = 0;
    let mut rest = html;
    while let Some(open) = rest.find("<button") {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('>') else { break };
        let tag = &rest[..close];
        let classes: Vec<&str> = attr(tag, "class")
            .unwrap_or("")
            .split_whitespace()
            .collect();
        let mut styled = false;
        const PROPS: [&str; 4] = ["width", "min-width", "height", "min-height"];
        let mut dims: [Option<f32>; 4] = [None; 4];
        for (sel, decls) in &rules {
            let names_it = sel.split(',').any(|s| {
                s.trim()
                    .strip_prefix('.')
                    .is_some_and(|c| classes.contains(&c))
            });
            if !names_it {
                continue;
            }
            styled = true;
            for (slot, prop) in dims.iter_mut().zip(PROPS) {
                if let Some(v) = px(decls, prop) {
                    *slot = Some(v);
                }
            }
        }
        if !styled {
            continue;
        }
        checked += 1;
        let size = |a: Option<f32>, b: Option<f32>| a.unwrap_or(0.0).max(b.unwrap_or(0.0));
        let (w, h) = (size(dims[0], dims[1]), size(dims[2], dims[3]));
        if w < 24.0 || h < 24.0 {
            bad.push(format!("<{tag}> guarantees {w} by {h} px"));
        }
    }
    (bad, checked)
}

#[test]
fn every_suggestion_button_is_at_least_24_px_square() {
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    let recent = RecentLog::default();

    // Seed a suggestion on the quic bookmark (its skolemized subject is what an overlay targets).
    let select = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
        .with_arg(
            "query",
            ArgRef::Inline(
                b"SELECT ?s WHERE { ?s <http://purl.org/dc/elements/1.1/subject> \"quic\" } LIMIT 1"
                    .to_vec(),
            ),
        )
        .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let (rows, _) = Resolver::issue_as(&k, select, &Capability::root()).expect("select");
    let rows: serde_json::Value = serde_json::from_slice(&rows.bytes).unwrap();
    let book = rows["results"]["bindings"][0]["s"]["value"]
        .as_str()
        .expect("the quic bookmark")
        .to_string();
    ikigai_cms_web::tagstore::TagPaths::in_dir(dir.path()).add_suggestion(&book, "suggested");

    for style in ["catalog", "mosaic", "agenda"] {
        let html = get(&k, &recent, &format!("urn:cms:view:quic?style={style}"));
        assert!(
            html.contains("cms-sug-yes"),
            "{style}: no suggestion: {html}"
        );
        let (bad, checked) = undersized_buttons(&html);
        assert!(
            checked >= 2,
            "{style}: the + and x buttons were not checked: {html}"
        );
        assert!(
            bad.is_empty(),
            "{style}: a target under 24 by 24 CSS px (SC 2.5.8): {bad:#?}"
        );
    }
}

#[test]
fn the_size_check_sees_a_small_button() {
    // The rules as they were before ledger #1672, and one button no rule of the fragment names.
    let (bad, checked) = undersized_buttons(
        "<style>.cms-sug-yes,.cms-sug-no{font:12px ui-monospace,monospace;padding:0 3px;line-height:1}\n\
         .cms-sug-yes:hover,.cms-sug-no:hover{min-width:40px;min-height:40px}\n\
         .big{width:30px;min-height:24px}</style>\
         <button class=\"cms-sug-yes\" hx-post=\"/tag/approve\">+</button>\
         <button class='cms-sug-no'>x</button>\
         <button class=\"big\">b</button>\
         <button class=\"cms-seg-btn\">shell</button>",
    );
    assert_eq!(checked, 3);
    assert_eq!(bad.len(), 2, "{bad:#?}");
}
