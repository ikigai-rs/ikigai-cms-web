//! Every color pair the room draws meets the floor this ecosystem enforces, in both schemes:
//! `ikigai-a11y`'s own contrast math and thresholds over the tokens `dist/room.css` declares, the
//! file the browser loads. The views' stylesheets and the shell may only name colors through
//! those tokens, and any token used as a text color must have its pairs listed here, so a color
//! cannot reach the page without passing through this test (ledger #1641).

use ikigai_a11y::color::{ratio, Rgba};
use ikigai_a11y::config::DEFAULT_MIN;

const ROOM_CSS: &str = include_str!("../dist/room.css");
const VIEWS: [(&str, &str); 7] = [
    ("agenda.xsl", include_str!("../styles/agenda.xsl")),
    ("catalog.xsl", include_str!("../styles/catalog.xsl")),
    ("mosaic.xsl", include_str!("../styles/mosaic.xsl")),
    ("recent.xsl", include_str!("../styles/recent.xsl")),
    ("review.xsl", include_str!("../styles/review.xsl")),
    ("tags.xsl", include_str!("../styles/tags.xsl")),
    ("types.xsl", include_str!("../styles/types.xsl")),
];

/// Every (text color, ground) pair the room draws. All of it is text under 18pt, or could be, so
/// all of it holds the body-text floor.
const PAIRS: &[(&str, &[&str])] = &[
    ("--fg", &["--bg", "--card", "--accent-bg"]),
    ("--mut", &["--bg", "--card"]),
    ("--accent", &["--bg", "--card", "--accent-bg"]),
    // The sign-in panel's buttons: the first control anyone meets.
    ("--on-accent", &["--accent"]),
    // Purge buttons, the "gone" badge, failures, the suggestion's dismiss.
    ("--danger", &["--bg", "--card"]),
    // A purge button under the pointer.
    ("--on-danger", &["--danger"]),
    // The suggestion's promote, the wire indicator, the review's keep.
    ("--ok", &["--bg", "--card"]),
    ("--status-nxdomain", &["--bg", "--card"]),
    ("--status-unreachable", &["--bg", "--card"]),
    ("--status-refused", &["--bg", "--card"]),
    ("--kind-bookmark", &["--bg", "--card"]),
    ("--kind-book", &["--bg", "--card"]),
    ("--kind-presentation", &["--bg", "--card"]),
];

/// The `--name:value` declarations of the `:root` rule on the line starting with `prefix`.
fn tokens(css: &str, prefix: &str) -> Vec<(String, String)> {
    let line = css
        .lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("room.css has no line starting {prefix}"));
    let body = line.split_once(":root {").expect("a :root rule").1;
    let body = body.split('}').next().unwrap();
    body.split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn light() -> Vec<(String, String)> {
    tokens(ROOM_CSS, ":root {")
}

/// The dark scheme overrides the light tokens it names; the rest carry over, as in the browser.
fn dark() -> Vec<(String, String)> {
    let mut merged = light();
    for (k, v) in tokens(ROOM_CSS, "@media (prefers-color-scheme: dark) { :root {") {
        match merged.iter_mut().find(|(name, _)| *name == k) {
            Some(slot) => slot.1 = v,
            None => merged.push((k, v)),
        }
    }
    merged
}

/// Every pair of `PAIRS` under the floor in this palette, or a token it does not declare.
fn failures(palette: &[(String, String)]) -> Vec<String> {
    let color = |token: &str| {
        palette
            .iter()
            .find(|(k, _)| k == token)
            .map(|(_, v)| Rgba::parse(v).unwrap_or_else(|e| panic!("{token}: {v}: {e}")))
    };
    let mut out = Vec::new();
    for (fg, grounds) in PAIRS {
        for ground in *grounds {
            match (color(fg), color(ground)) {
                (Some(f), Some(g)) => {
                    let got = ratio(f, g);
                    if got < DEFAULT_MIN {
                        out.push(format!("{fg} on {ground} is {got:.2}:1"));
                    }
                }
                _ => out.push(format!("{fg} on {ground}: a token is not declared")),
            }
        }
    }
    out
}

/// The CSS of a view: what is inside its `<style>` element.
fn style_block(xsl: &str) -> &str {
    let start = xsl.find("<style>").expect("a style block") + "<style>".len();
    let end = xsl.find("</style>").expect("a closed style block");
    &xsl[start..end]
}

/// `css` without its `/* comments */`: a ledger number in one is not a color.
fn without_comments(css: &str) -> String {
    let mut out = String::new();
    let mut rest = css;
    while let Some((before, after)) = rest.split_once("/*") {
        out.push_str(before);
        rest = after.split_once("*/").expect("a closed comment").1;
    }
    out.push_str(rest);
    out
}

/// The room's stylesheets as (name, CSS), with `room.css`'s two token lines left out.
fn all_css() -> Vec<(String, String)> {
    let shell: Vec<&str> = ROOM_CSS
        .lines()
        .filter(|l| !l.contains(":root {"))
        .collect();
    let shell = without_comments(&shell.join("\n"));
    let mut out = vec![("room.css".to_string(), shell)];
    out.extend(
        VIEWS
            .iter()
            .map(|(name, xsl)| (name.to_string(), style_block(xsl).to_string())),
    );
    out
}

/// A color written as a literal: `#` and 3, 4, 6 or 8 hex digits, or a color function. `#room`,
/// the one id these stylesheets select, is not one.
fn literal_colors(css: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (i, _) in css.match_indices('#') {
        let digits: String = css[i + 1..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        if [3, 4, 6, 8].contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_hexdigit()) {
            found.push(format!("#{digits}"));
        }
    }
    for function in ["rgb(", "rgba(", "hsl(", "hsla("] {
        found.extend(css.match_indices(function).map(|(_, f)| f.to_string()));
    }
    found
}

/// The tokens a stylesheet uses as the `color` property (not `border-color` or `background`).
fn text_tokens(css: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (i, _) in css.match_indices("color:") {
        let is_property = css[..i]
            .chars()
            .next_back()
            .is_none_or(|c| matches!(c, '{' | ';' | ' ' | '\n'));
        let value = css[i + "color:".len()..].trim_start();
        if let (true, Some(rest)) = (is_property, value.strip_prefix("var(")) {
            let end = rest.find([',', ')']).expect("a closed var()");
            found.push(rest[..end].to_string());
        }
    }
    found
}

#[test]
fn every_pair_clears_the_floor_in_the_light_scheme() {
    let failing = failures(&light());
    assert!(failing.is_empty(), "{failing:#?}");
}

#[test]
fn every_pair_clears_the_floor_in_the_dark_scheme() {
    let failing = failures(&dark());
    assert!(failing.is_empty(), "{failing:#?}");
}

/// The floor is live: the colors PR 108 measured are refused, at the ratios it measured.
#[test]
fn the_floor_refuses_the_colors_that_failed_in_the_browser() {
    let pair = |fg: &str, bg: &str| ratio(Rgba::parse(fg).unwrap(), Rgba::parse(bg).unwrap());
    for (what, fg, bg, measured) in [
        ("light muted on the page", "#8a857b", "#faf8f3", 3.46),
        ("light muted on a card", "#8a857b", "#ffffff", 3.67),
        ("dark passkey button", "#ffffff", "#7fb2ee", 2.21),
        ("dark red on the page", "#c0392b", "#1a1917", 3.23),
        ("dark nxdomain badge", "#8e2f8e", "#1a1917", 2.47),
    ] {
        let got = pair(fg, bg);
        assert!(got < DEFAULT_MIN, "{what}: {got:.2}");
        assert!((got - measured).abs() < 0.02, "{what}: {got:.2}");
    }
    let planted = vec![
        ("--fg".to_string(), "#8a857b".to_string()),
        ("--bg".to_string(), "#faf8f3".to_string()),
    ];
    let failing = failures(&planted);
    assert!(failing.iter().any(|f| f.starts_with("--fg on --bg is 3.4")));
    assert!(failing.iter().any(|f| f.contains("not declared")));
}

#[test]
fn no_stylesheet_writes_a_color_outside_the_palette() {
    for (name, css) in all_css() {
        let literals = literal_colors(&css);
        assert!(literals.is_empty(), "{name}: {literals:?}");
    }
    assert_eq!(
        literal_colors(".a{color:#c0392b}#room{margin:0}"),
        ["#c0392b"]
    );
    assert_eq!(
        literal_colors("a{border:1px solid rgba(1,1,1,.5)}"),
        ["rgba("]
    );
}

#[test]
fn every_token_used_as_text_is_held_to_the_floor() {
    for (name, css) in all_css() {
        for token in text_tokens(&css) {
            assert!(
                PAIRS.iter().any(|(fg, _)| *fg == token),
                "{name} draws text in {token}, which no pair in this test holds to the floor"
            );
        }
    }
    assert_eq!(
        text_tokens("a{color:var(--mut)}b{border-left-color:var(--x);color:var(--fg,#000)}"),
        ["--mut", "--fg"]
    );
}
