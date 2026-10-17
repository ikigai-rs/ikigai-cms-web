//! What library data may put into the room's markup, and the policy the page is served under
//! (ledger #1793, ledger #1794). Each case was reproduced on 0b60096 with planted data:
//!
//! - **A link from data with no scheme check.** A bookmark is `[[url][title]]` and its URL became
//!   the card's `href` as is, so `[[javascript:…][…]]` in the bookmarks file (or an imported
//!   Zotero item, whose reader URL rides the same path) put a script URL on a card in every card
//!   style, on the origin where a signed-in session acts as root.
//! - **No Content-Security-Policy.** Nothing limited what such a link, or any other injected
//!   markup, could run.
//! - **`hx-vals` built by concatenation.** The suggestion buttons carried
//!   `{"book":"…","tag":"…"}` assembled inside the stylesheet. Attribute escaping protects the
//!   HTML, not the JSON, so a tag holding `", "x": "y` sent an extra key and lost its own value.
//! - **A tag in a `/r/` path was not percent-encoded.** A `?` or `#` in a tag ended the path, so
//!   the chip opened some other view.

use std::path::Path;

use super::{handle_tag, resolve_http, HttpAuth};
use ikigai_cms_web::session::RecentLog;
use ikigai_core::{ArgRef, Capability, Iri, Kernel, Request, Verb};
use ikigai_resolve::Resolver;

/// Schemes a link must never carry, each planted as a bookmark the room renders.
const HOSTILE: [(&str, &str); 6] = [
    ("javascript:alert(1)", "Script Link"),
    (" JaVaScRiPt:alert(2)", "Mixed Case Link"),
    ("data:text/html,<b>x</b>", "Data Link"),
    ("vbscript:msgbox(1)", "VB Link"),
    ("//evil.example/", "Scheme Relative Link"),
    ("/\\evil.example/", "Backslash Link"),
];

/// A room whose bookmarks file holds one ordinary link, one same-origin relative link, and every
/// [`HOSTILE`] link, all under the tag `planted`.
fn room(dir: &Path) -> Kernel {
    let mut org = String::from(
        "* Bookmarks\n\
         ** [[https://ok.example/a?b=1&c=2][Ordinary Link]]\n   :PROPERTIES:\n   :TAGS: planted\n   :END:\n\
         ** [[/r/urn:cms:tags][Relative Link]]\n   :PROPERTIES:\n   :TAGS: planted\n   :END:\n",
    );
    for (url, title) in HOSTILE {
        org.push_str(&format!(
            "** [[{url}][{title}]]\n   :PROPERTIES:\n   :TAGS: planted\n   :END:\n"
        ));
    }
    let bm = dir.join("old-org/pinboard-bookmarks.org");
    std::fs::create_dir_all(bm.parent().unwrap()).unwrap();
    std::fs::write(&bm, org).unwrap();
    ikigai_cms_web::build_cms_kernel_with(
        dir.to_path_buf(),
        None,
        None,
        None,
        ikigai_cms_web::tagstore::TagPaths::in_dir(dir),
        Some(dir.join("cms-linkstatus.json")),
    )
}

/// `GET /r/{target}` as a signed-in reader.
fn get(kernel: &Kernel, target: &str) -> String {
    let (status, _ct, body) = resolve_http(
        kernel,
        &RecentLog::default(),
        &Capability::root(),
        Some("reader"),
        target,
    );
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, "200 OK", "{target}: {body}");
    body
}

/// Undo the entity escapes either serializer writes into an attribute value.
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The value of `name` in one start tag, unescaped. Both quote styles: the stylesheets' output
/// quotes with `'` (xrust's serializer), `native.rs` with `"`.
fn attr(tag: &str, name: &str) -> Option<String> {
    for q in ['\'', '"'] {
        let key = format!(" {name}={q}");
        if let Some(i) = tag.find(&key) {
            let rest = &tag[i + key.len()..];
            return rest.find(q).map(|end| unescape(&rest[..end]));
        }
    }
    None
}

/// Every start tag named `element` in `html`.
fn start_tags<'a>(html: &'a str, element: &str) -> Vec<&'a str> {
    let open = format!("<{element}");
    let mut tags = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find(&open) {
        rest = &rest[i + open.len()..];
        if !rest.starts_with([' ', '>', '/']) {
            continue;
        }
        let end = rest.find('>').unwrap_or(rest.len());
        tags.push(&rest[..end]);
        rest = &rest[end..];
    }
    tags
}

/// Every `href` in `html`, unescaped.
fn hrefs(html: &str) -> Vec<String> {
    start_tags(html, "a")
        .into_iter()
        .filter_map(|t| attr(t, "href"))
        .collect()
}

#[test]
fn a_link_from_data_renders_only_with_a_safe_scheme() {
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    for style in ["catalog", "mosaic", "agenda"] {
        let html = get(&k, &format!("urn:cms:view:planted?style={style}"));
        let links = hrefs(&html);
        for (url, title) in HOSTILE {
            assert!(
                !links.iter().any(|h| h.trim() == url.trim()),
                "{style}: {url:?} is a link: {links:#?}"
            );
            assert!(
                html.contains(title),
                "{style}: the {title:?} card still shows its title: {html}"
            );
        }
        assert!(
            links.iter().any(|h| h == "https://ok.example/a?b=1&c=2"),
            "{style}: an https link still links: {links:#?}"
        );
        assert!(
            links.iter().any(|h| h == "/r/urn:cms:tags"),
            "{style}: a same-origin relative link still links: {links:#?}"
        );
    }
}

/// The `Content-Security-Policy` header of a raw response, if it has one.
fn policy(resp: &str) -> Option<String> {
    let head = resp.split("\r\n\r\n").next().unwrap_or("");
    head.lines()
        .find_map(|l| l.strip_prefix("Content-Security-Policy: "))
        .map(str::to_string)
}

#[tokio::test]
async fn every_response_carries_the_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<h1>room</h1>").unwrap();
    // The page face's own test server (http_face.rs), dev-open so `/r/` answers a view.
    let port = super::http_face::start(dir.path(), "http://localhost:8080", true).await;
    // The page, a view, the tag index and an error fragment are HTML; the 404 is not, and carries
    // the policy anyway.
    for (target, html) in [
        ("/", true),
        ("/index.html", true),
        ("/r/urn:cms:view:science?style=catalog", true),
        ("/r/urn:cms:tags", true),
        ("/r/urn:cms:nope", true),
        ("/nope.html", false),
    ] {
        let resp = super::http_face::send(
            port,
            &format!("GET {target} HTTP/1.1\r\nHost: localhost:8080"),
        )
        .await;
        assert_eq!(
            resp.contains("Content-Type: text/html"),
            html,
            "{target}: {resp}"
        );
        let csp = policy(&resp).unwrap_or_else(|| panic!("{target}: no policy: {resp}"));
        let directives: Vec<&str> = csp.split(';').map(str::trim).collect();
        for want in [
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
        ] {
            assert!(directives.contains(&want), "{target}: no {want:?} in {csp}");
        }
        let script = directives
            .iter()
            .find(|d| d.starts_with("script-src "))
            .unwrap_or_else(|| panic!("{target}: no script-src in {csp}"));
        assert!(script.contains("'self'"), "{target}: {script}");
        for loose in [
            "'unsafe-inline'",
            "'unsafe-eval'",
            "*",
            "data:",
            "http:",
            "https:",
        ] {
            assert!(
                !script.split_whitespace().any(|s| s == loose),
                "{target}: script-src allows {loose}: {script}"
            );
        }
        assert!(
            !csp.contains("'unsafe-inline'"),
            "{target}: no directive allows inline content wholesale: {csp}"
        );
    }
}

/// What htmx sends for the `+` or `x` button (`which` = its class) of each suggestion chip in
/// `html`: the keys of its `hx-vals` JSON, then the hidden inputs of the chip it `hx-include`s.
/// A value that does not parse as a JSON object is reported as the key `!unparsed`.
fn sent_by(html: &str, which: &str) -> Vec<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(chip) = rest.find("<span class='cms-sug'") {
        rest = &rest[chip..];
        let end = rest.find("</span>").unwrap_or(rest.len());
        let span = &rest[..end];
        rest = &rest[end..];
        for button in start_tags(span, "button") {
            if !attr(button, "class").is_some_and(|c| c.split_whitespace().any(|c| c == which)) {
                continue;
            }
            let mut params = Vec::new();
            if let Some(vals) = attr(button, "hx-vals") {
                match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&vals) {
                    Ok(map) => params.extend(map.into_iter().map(|(k, v)| {
                        (k, v.as_str().map(str::to_string).unwrap_or(v.to_string()))
                    })),
                    Err(_) => params.push(("!unparsed".into(), vals)),
                }
            }
            if attr(button, "hx-include").as_deref() == Some("closest .cms-sug") {
                for input in start_tags(span, "input") {
                    if let (Some(name), Some(value)) = (attr(input, "name"), attr(input, "value")) {
                        params.push((name, value));
                    }
                }
            }
            out.push(params);
        }
    }
    out
}

/// The skolemized subject of the bookmark titled `title`.
fn subject(k: &Kernel, title: &str) -> String {
    let query = format!(
        "SELECT ?s WHERE {{ ?s <http://purl.org/dc/elements/1.1/title> \"{title}\" }} LIMIT 1"
    );
    let select = Request::new(Verb::Source, Iri::parse("urn:sparql:select").unwrap())
        .with_arg("query", ArgRef::Inline(query.into_bytes()))
        .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let (rows, _) = Resolver::issue_as(k, select, &Capability::root()).expect("select");
    let rows: serde_json::Value = serde_json::from_slice(&rows.bytes).unwrap();
    rows["results"]["bindings"][0]["s"]["value"]
        .as_str()
        .expect("the bookmark")
        .to_string()
}

/// Form-encode `params` as htmx does (`encodeURIComponent` on each name and value).
fn form_body(params: &[(String, String)]) -> Vec<u8> {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z'
                | b'a'..=b'z'
                | b'0'..=b'9'
                | b'-'
                | b'_'
                | b'.'
                | b'!'
                | b'~'
                | b'*'
                | b'\''
                | b'('
                | b')' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    params
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

#[test]
fn a_suggested_tag_holding_json_posts_as_one_value() {
    const TAG: &str = "\", \"x\": \"y";
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    let book = subject(&k, "Ordinary Link");
    let tags = ikigai_cms_web::tagstore::TagPaths::in_dir(dir.path());
    tags.add_suggestion(&book, TAG);
    for style in ["catalog", "mosaic", "agenda"] {
        let html = get(&k, &format!("urn:cms:view:planted?style={style}"));
        for which in ["cms-sug-yes", "cms-sug-no"] {
            let sent = sent_by(&html, which);
            assert_eq!(sent.len(), 1, "{style} {which}: one chip: {html}");
            let mut params = sent[0].clone();
            params.sort();
            assert_eq!(
                params,
                vec![
                    ("book".to_string(), book.clone()),
                    ("tag".to_string(), TAG.to_string())
                ],
                "{style} {which}: htmx sends exactly the book and the whole tag"
            );
        }
    }
    // And the server reads back the one value: promoting it approves exactly that tag.
    let html = get(&k, "urn:cms:view:planted?style=catalog");
    let sent = sent_by(&html, "cms-sug-yes").remove(0);
    let (_, _, reply) = handle_tag(
        &k,
        &HttpAuth::default(),
        None,
        true,
        "urn:cms:tag-approve",
        &form_body(&sent),
    );
    assert!(
        String::from_utf8_lossy(&reply).contains("cms-tag"),
        "promoted: {}",
        String::from_utf8_lossy(&reply)
    );
    assert_eq!(tags.approved_tags(), vec![TAG.to_string()]);
}

/// The `/r/` target a browser requests for `href` on the room's page (what the URL parser keeps
/// of it: the path and the query, never the fragment).
fn requested(href: &str) -> String {
    let base = url::Url::parse("http://localhost:8080/").unwrap();
    let u = base.join(href).unwrap();
    let path = u
        .path()
        .strip_prefix("/r/")
        .expect("a /r/ link")
        .to_string();
    match u.query() {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

#[test]
fn a_tag_with_url_syntax_in_it_opens_from_every_chip() {
    const TAG: &str = "what? #1 / 100% \\ a&b";
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    let book = subject(&k, "Ordinary Link");
    let tags = ikigai_cms_web::tagstore::TagPaths::in_dir(dir.path());
    tags.add_suggestion(&book, TAG);
    assert!(tags.approve(&book, TAG), "approved");
    let chip = |html: &str| -> String {
        hrefs(html)
            .into_iter()
            .find(|h| h.starts_with("/r/urn:cms:view:") && !h.contains("planted"))
            .unwrap_or_else(|| panic!("no chip for the tag: {html}"))
    };
    // The chip in the tag index, on each card style's card, and the one a promote returns.
    let mut seen = vec![("index", chip(&get(&k, "urn:cms:tags")))];
    for style in ["catalog", "mosaic", "agenda"] {
        let html = get(&k, &format!("urn:cms:view:planted?style={style}"));
        seen.push((style, chip(&html)));
    }
    let (_, _, reply) = handle_tag(
        &k,
        &HttpAuth::default(),
        None,
        true,
        "urn:cms:tag-approve",
        &form_body(&[("book".into(), book.clone()), ("tag".into(), TAG.into())]),
    );
    seen.push(("promote", chip(&String::from_utf8_lossy(&reply))));
    for (from, href) in seen {
        let target = requested(&href);
        let view = get(&k, &format!("{target}?style=catalog"));
        assert!(
            view.contains("Ordinary Link") && !view.contains("Script Link"),
            "{from}: {href} opened something else: {view}"
        );
        // And the view's own controls (the pager, the restyle toolbar) name the same view.
        for control in start_tags(&view, "button") {
            if let Some(get) = attr(control, "hx-get") {
                if get.starts_with("/r/urn:cms:view:") {
                    assert_eq!(
                        requested(&get).split('?').next(),
                        target.split('?').next(),
                        "{from}: a control on the view names another view: {get}"
                    );
                }
            }
        }
    }
}

/// The body of every `<style>` element in `html`.
fn style_bodies(html: &str) -> Vec<&str> {
    let mut bodies = Vec::new();
    let mut rest = html;
    while let Some(open) = rest.find("<style>") {
        rest = &rest[open + "<style>".len()..];
        let Some(close) = rest.find("</style>") else {
            break;
        };
        bodies.push(&rest[..close]);
        rest = &rest[close..];
    }
    bodies
}

#[test]
fn the_policy_admits_every_style_block_a_view_renders() {
    let dir = tempfile::tempdir().unwrap();
    let k = room(dir.path());
    let csp = ikigai_cms_web::csp::policy(4433);
    let mut checked = 0;
    for target in [
        "urn:cms:tags",
        "urn:cms:types",
        "urn:cms:recent",
        "urn:cms:review",
        "urn:cms:view:planted?style=catalog",
        "urn:cms:view:planted?style=mosaic",
        "urn:cms:view:planted?style=agenda",
    ] {
        let html = get(&k, target);
        let bodies = style_bodies(&html);
        assert!(!bodies.is_empty(), "{target} renders its own style: {html}");
        for body in bodies {
            let hash = ikigai_cms_web::csp::style_hash(body);
            assert!(
                csp.contains(&hash),
                "{target}: the policy refuses its style {hash} (the view would render unstyled): {body}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 7, "one style block per view");
}
