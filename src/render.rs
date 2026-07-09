//! Render a SPARQL result set into an htmx reading-room fragment (cards).
//!
//! Interim: a Rust template. The structure it emits — semantic classes, no baked-in
//! CSS, tag chips as htmx links — is the contract the swappable XSLT stylesheet
//! resources (`urn:cms:style:*`) will reproduce, so the room can be restyled without a
//! rebuild.

/// One resource in a view: its URL (`dc:identifier`), title, and tags.
pub struct Row {
    pub url: String,
    pub title: String,
    pub tags: Vec<String>,
}

/// Parse SPARQL 1.1 Results JSON (a SELECT) into rows. Expects `url`, `title`, and a
/// space-joined `tags` binding (from `GROUP_CONCAT`).
pub fn parse_rows(bytes: &[u8]) -> Result<Vec<Row>, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let mut rows = Vec::new();
    if let Some(bindings) = v["results"]["bindings"].as_array() {
        for b in bindings {
            rows.push(Row {
                url: b["url"]["value"].as_str().unwrap_or("").to_string(),
                title: b["title"]["value"].as_str().unwrap_or("").to_string(),
                tags: b["tags"]["value"]
                    .as_str()
                    .unwrap_or("")
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
            });
        }
    }
    Ok(rows)
}

/// Render the rows as an htmx fragment: a `<section>` header + one `<article>` card
/// per resource. Semantic classes only — the stylesheet resource supplies the skin;
/// tag chips are htmx links that re-render the room for that tag.
pub fn cards_html(tag: &str, rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "<section class=\"cms-room\" data-view=\"{}\">",
        attr(tag)
    ));
    out.push_str(&format!(
        "<header class=\"cms-head\"><h2 class=\"cms-view-title\">#{}</h2>\
         <span class=\"cms-count\">{} resources</span></header>",
        esc(tag),
        rows.len()
    ));
    for row in rows {
        out.push_str("<article class=\"cms-card\">");
        out.push_str(&format!(
            "<a class=\"cms-title\" href=\"{}\">{}</a>",
            attr(&row.url),
            esc(&row.title)
        ));
        out.push_str(&format!(
            "<div class=\"cms-host\">{}</div>",
            esc(host_of(&row.url))
        ));
        if !row.tags.is_empty() {
            out.push_str("<div class=\"cms-tags\">");
            for t in &row.tags {
                out.push_str(&format!(
                    "<a class=\"cms-tag\" hx-get=\"urn:cms:view:{}\" hx-target=\"#room\">#{}</a>",
                    attr(t),
                    esc(t)
                ));
            }
            out.push_str("</div>");
        }
        out.push_str("</article>");
    }
    out.push_str("</section>");
    out
}

/// The host portion of a URL, for a compact card subtitle. Best-effort; falls back to
/// the whole string when there's no recognizable authority.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

/// Escape text for HTML element content.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a value for a double-quoted HTML attribute.
fn attr(s: &str) -> String {
    esc(s).replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sparql_results_json_into_rows() {
        let json = br#"{"head":{"vars":["url","title","tags"]},"results":{"bindings":[
            {"url":{"type":"literal","value":"https://quicwg.org"},
             "title":{"type":"literal","value":"QUIC WG"},
             "tags":{"type":"literal","value":"quic networking"}}]}}"#;
        let rows = parse_rows(json).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].url, "https://quicwg.org");
        assert_eq!(rows[0].tags, vec!["quic", "networking"]);
    }

    #[test]
    fn renders_cards_with_title_host_and_tag_links() {
        let rows = vec![Row {
            url: "https://blog.cloudflare.com/quiche".to_string(),
            title: "Enjoy a slice of QUIC".to_string(),
            tags: vec!["quic".to_string(), "rust".to_string()],
        }];
        let html = cards_html("quic", &rows);
        assert!(html.contains("data-view=\"quic\""), "{html}");
        assert!(html.contains(">#quic</h2>"), "{html}");
        assert!(
            html.contains("href=\"https://blog.cloudflare.com/quiche\""),
            "{html}"
        );
        assert!(html.contains(">blog.cloudflare.com</div>"), "{html}");
        assert!(
            html.contains("hx-get=\"urn:cms:view:rust\""),
            "tag chips link to their view: {html}"
        );
    }

    #[test]
    fn content_is_html_escaped() {
        let rows = vec![Row {
            url: "https://x/?a=1&b=2".to_string(),
            title: "A < B & \"C\"".to_string(),
            tags: vec![],
        }];
        let html = cards_html("t", &rows);
        assert!(html.contains("A &lt; B &amp; \"C\""), "{html}");
        assert!(html.contains("a=1&amp;b=2"), "{html}");
    }
}
