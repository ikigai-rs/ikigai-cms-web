//! `urn:cms:graph:presentations` — Brian's lectern decks as `cms:Presentation` resources
//! on the shared CMS axis, so a talk is browsable in the reading room like a bookmark or
//! a book (type facet, tags, recency, search, pagination, sort — all for free).
//!
//! A **deck** is a directory containing `deck.toml`. Its title is the title slide's first
//! `# H1`; its tags are that slide's `<span class="tag …">` run **plus** the venue
//! segments of its path — so `conferences/nfjs/uberconf/2026/…` yields `#nfjs` and
//! `#uberconf` with no hand-tagging. Its link is the built `dist/index.html`.
//!
//! This is the *bootstrap* transreptor: it scrapes the presentation layer once to seed the
//! graph. The durable end state is the graph as source of truth with lectern rendering
//! *from* it (title/tags/provenance authored in the graph, not scraped back out of HTML).
//! Native-only (it walks the filesystem) and not golden-threaded, so a newly-authored deck
//! appears on the next restart.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ikigai_core::{Description, Endpoint, Invocation, ReprType, Representation, Result, Verb};

/// `urn:cms:graph:presentations` — the decks under `root` as typed `cms:Presentation`
/// Turtle. Empty when no presentations root is configured (so the union is unconditional).
pub(crate) struct PresentationsGraph {
    pub root: Option<PathBuf>,
}

#[async_trait]
impl Endpoint for PresentationsGraph {
    async fn invoke(&self, _inv: &Invocation<'_>) -> Result<Representation> {
        let turtle = match &self.root {
            Some(root) => presentations_turtle(root),
            None => String::new(),
        };
        Ok(Representation::new(
            ReprType::new("text/turtle").with_param("charset", "utf-8"),
            turtle.into_bytes(),
        )
        .cacheable())
    }

    fn name(&self) -> &str {
        "cms-graph"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:graph:presentations")
            .summary(
                "Lectern decks as cms:Presentation resources on the CMS axis: title from \
                 the title slide's H1, tags from its .tag spans + the venue path segments, \
                 link to the built deck. A view is a query.",
            )
            .verb(Verb::Source)
    }
}

/// The whole presentations graph: a `cms:Presentation` block per deck under `root`.
fn presentations_turtle(root: &Path) -> String {
    let mut out = String::from(
        "@prefix dc: <http://purl.org/dc/elements/1.1/> .\n\
         @prefix cms: <https://ikigai-rs.dev/ns/cms#> .\n",
    );
    for deck in decks(root) {
        if let Some(block) = deck_turtle(&deck, root) {
            out.push_str(&block);
        }
    }
    out
}

/// Every deck (a directory containing `deck.toml`) under `root`, recursively, sorted for a
/// stable graph. Build output (`dist`) and shared-resource dirs never hold decks.
fn decks(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_decks(root, &mut out);
    out.sort();
    out
}

fn collect_decks(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut is_deck = false;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !matches!(name, "dist" | "_assets" | "_themes" | "_partials" | ".git") {
                subdirs.push(path);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("deck.toml") {
            is_deck = true;
        }
    }
    // A deck holds no sub-decks; stop descending (its `slides/` has no deck.toml anyway).
    if is_deck {
        out.push(dir.to_path_buf());
        return;
    }
    for sub in subdirs {
        collect_decks(&sub, out);
    }
}

/// One deck → a `cms:Presentation` block, or `None` if it has no readable title slide.
fn deck_turtle(deck: &Path, root: &Path) -> Option<String> {
    let slide = title_slide(deck)?;
    let text = std::fs::read_to_string(&slide).ok()?;
    let region = title_region(&text);
    let title = first_h1(region).unwrap_or_else(|| pretty_slug(deck));

    let mut tags: Vec<String> = tag_spans(region);
    tags.extend(venue_tags(deck, root));
    tags.sort();
    tags.dedup();

    // Browsable-for-now: the built deck's path, shown on the card as a `file://` URL. A
    // served http URL swaps in here post-Uberconf and the card link just starts working.
    let built = deck.join("dist/index.html");
    let link = if built.is_file() {
        built
    } else {
        deck.to_path_buf()
    };

    let iri = deck_iri(deck, root);
    let mut ttl = format!(
        "<{iri}> a cms:Presentation ; dc:title {} ; dc:identifier {}",
        ttl_str(&title),
        ttl_str(&format!("file://{}", link.display())),
    );
    for tag in &tags {
        ttl.push_str(&format!(" ; dc:subject {}", ttl_str(tag)));
    }
    ttl.push_str(" .\n");
    Some(ttl)
}

/// A deck's title slide: `slides/00-title.md` if present, else the lexically-first
/// `slides/*.md` (single-file decks carry the title slide at the top of that file).
fn title_slide(deck: &Path) -> Option<PathBuf> {
    let slides = deck.join("slides");
    let explicit = slides.join("00-title.md");
    if explicit.is_file() {
        return Some(explicit);
    }
    let mut mds: Vec<PathBuf> = std::fs::read_dir(&slides)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    mds.sort();
    mds.into_iter().next()
}

/// The leading title-slide region of a slide file: everything before the first `---` slide
/// break (the whole file if there is none). Stopping at the break keeps a content slide's
/// `#` heading or a mid-deck `.tag` section marker out of the title/tags.
fn title_region(text: &str) -> &str {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim() == "---" {
            return &text[..offset];
        }
        offset += line.len();
    }
    text
}

/// The first `# ` (H1) heading text in the region, if any (never an `##` H2).
fn first_h1(region: &str) -> Option<String> {
    region.lines().find_map(|l| {
        l.trim_start()
            .strip_prefix("# ")
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
    })
}

/// The text of every `<span class="tag …">…</span>` in the region, lowercased. The
/// `cool`/`warm`/`ink` class is only a colour role, so we read the element text, not it.
fn tag_spans(region: &str) -> Vec<String> {
    let mut tags = Vec::new();
    let mut rest = region;
    while let Some(i) = rest.find("<span class=\"tag") {
        rest = &rest[i..];
        let Some(gt) = rest.find('>') else { break };
        let after = &rest[gt + 1..];
        let Some(end) = after.find("</span>") else {
            break;
        };
        let text = after[..end].trim();
        if !text.is_empty() {
            tags.push(text.to_lowercase());
        }
        rest = &after[end + "</span>".len()..];
    }
    tags
}

/// Venue tags from a deck's path relative to `root`: each path segment lowercased, dropping
/// the grouping dirs, four-digit years, and the deck's own directory — so
/// `conferences/nfjs/uberconf/2026/quant-bio` yields `nfjs`, `uberconf`.
fn venue_tags(deck: &Path, root: &Path) -> Vec<String> {
    let Ok(rel) = deck.strip_prefix(root) else {
        return Vec::new();
    };
    let comps: Vec<&str> = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let last = comps.len().saturating_sub(1);
    comps
        .iter()
        .enumerate()
        .filter(|(i, seg)| {
            *i != last
                && !matches!(**seg, "conferences" | "clients" | "talks")
                && !(seg.len() == 4 && seg.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|(_, seg)| seg.to_lowercase())
        .collect()
}

/// A stable IRI for a deck from its path relative to `root`.
fn deck_iri(deck: &Path, root: &Path) -> String {
    let rel = deck.strip_prefix(root).unwrap_or(deck);
    let slug: String = rel
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("urn:cms:presentation:{}", slug.trim_matches('-'))
}

/// A deck directory name as a human title fallback (`fs-crypto` → `fs crypto`).
fn pretty_slug(deck: &Path) -> String {
    deck.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Untitled")
        .replace(['-', '_'], " ")
}

/// Escape a value for a Turtle double-quoted string literal.
fn ttl_str(s: &str) -> String {
    let esc = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ");
    format!("\"{esc}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a deck (deck.toml + slides/00-title.md) under `dir`, return the deck dir.
    fn write_deck(dir: &Path, rel: &str, title_md: &str) -> PathBuf {
        let deck = dir.join(rel);
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        std::fs::write(deck.join("slides/00-title.md"), title_md).unwrap();
        deck
    }

    const TITLE_MD: &str = "<!-- .slide: class=\"slide inverse center middle\" -->\n\n\
        <span class=\"kicker\">NFJS · Virtual Workshop</span>\n\n\
        # Full Stack Engineering - Encryption\n\n\
        <span class=\"tag cool\">primitives</span> <span class=\"tag warm\">identity</span> \
        <span class=\"tag ink\">post-quantum</span>\n";

    #[test]
    fn a_deck_becomes_a_typed_presentation_with_title_tags_and_venue() {
        let tmp = tempfile::tempdir().unwrap();
        write_deck(tmp.path(), "conferences/nfjs/fs-crypto", TITLE_MD);
        let ttl = presentations_turtle(tmp.path());

        assert!(ttl.contains("a cms:Presentation"), "typed: {ttl}");
        assert!(
            ttl.contains("dc:title \"Full Stack Engineering - Encryption\""),
            "title from the H1, not deck.toml: {ttl}"
        );
        // Authored tags from the title slide's .tag spans.
        for t in ["primitives", "identity", "post-quantum"] {
            assert!(
                ttl.contains(&format!("dc:subject \"{t}\"")),
                "tag {t}: {ttl}"
            );
        }
        // Venue tag from the path (the Uberconf-style unlock) — nfjs here.
        assert!(ttl.contains("dc:subject \"nfjs\""), "venue tag: {ttl}");
        // A link the card can show.
        assert!(ttl.contains("dc:identifier \"file://"), "deck link: {ttl}");
    }

    #[test]
    fn the_venue_path_yields_free_tags_and_drops_years_and_the_deck_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("conferences/nfjs/uberconf/2026/quant-bio");
        let tags = venue_tags(&deck, tmp.path());
        assert!(tags.contains(&"nfjs".to_string()) && tags.contains(&"uberconf".to_string()));
        assert!(!tags.contains(&"2026".to_string()), "years dropped");
        assert!(
            !tags.contains(&"quant-bio".to_string()),
            "the deck's own dir dropped"
        );
        assert!(
            !tags.contains(&"conferences".to_string()),
            "grouping dir dropped"
        );
    }

    #[test]
    fn the_title_region_stops_at_the_first_slide_break() {
        // A single-file deck: the title slide is the region before the first `---`; a later
        // `# Agenda` content heading must not become the title.
        let single = "# Real Title\n\n<span class=\"tag ink\">topic</span>\n\n---\n# Agenda\n";
        assert_eq!(
            first_h1(title_region(single)).as_deref(),
            Some("Real Title")
        );
        assert_eq!(tag_spans(title_region(single)), vec!["topic".to_string()]);
    }

    #[test]
    fn a_dir_without_deck_toml_is_not_a_deck() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("_assets/images/security")).unwrap();
        write_deck(tmp.path(), "conferences/nfjs/real", TITLE_MD);
        let found = decks(tmp.path());
        assert_eq!(found.len(), 1, "only the real deck, not _assets: {found:?}");
    }
}
