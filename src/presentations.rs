//! `urn:cms:graph:presentations` — Brian's lectern decks as `cms:Presentation` resources
//! on the shared CMS axis, so a talk is browsable in the reading room like a bookmark or
//! a book (type facet, tags, recency, search, pagination, sort — all for free).
//!
//! A **deck** is a directory containing `deck.toml`. Title and tags come from the deck's
//! authored **JSON-LD** — a `<script type="application/ld+json">` lectern emits into the
//! built `dist/index.html` using the shared `dc:`/`cms:` vocab (the agreed interface). For
//! un-migrated or un-built decks the title **falls back** to the title slide's first `# H1`,
//! but tags never do: the visual `<span class="tag …">` chips are presentation, not
//! semantics, so they are never read. Tags come only from authored JSON-LD plus the **venue
//! segments of the path** — so `conferences/nfjs/uberconf/2026/…` yields `#nfjs`/`#uberconf`
//! with no hand-tagging, and an un-migrated deck carries just its title and venue tag. The
//! link is the built `dist/index.html`, served under the configured base.
//!
//! Native-only. Deck **contents** are read through the kernel (`urn:cms:deck:*`), so the
//! graph is **golden-threaded** to them — a `lectern build` that rewrites a deck cuts the
//! thread and the reading room refreshes. The directory **walk** (which decks exist) is still
//! `std::fs`, so a brand-new deck *directory* appears on the next restart. The durable end
//! state is the graph as source of truth with lectern rendering *from* it.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ikigai_core::{Description, Endpoint, Invocation, Iri, ReprType, Representation, Result, Verb};

/// Configuration for the presentations source: the decks root, and the base URL a static
/// server exposes that tree at — so a card link opens the built deck (`{base}/{deck}/dist/
/// index.html`). `base_url = None` falls back to `file://` paths: locatable on the card but
/// not clickable from an https page.
pub struct Presentations {
    pub root: PathBuf,
    pub base_url: Option<String>,
}

/// `urn:cms:graph:presentations` — the decks under the configured root as typed
/// `cms:Presentation` Turtle. Empty when unconfigured (so the union is unconditional).
pub(crate) struct PresentationsGraph {
    pub config: Option<Presentations>,
}

#[async_trait]
impl Endpoint for PresentationsGraph {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let turtle = match &self.config {
            Some(cfg) => presentations_turtle(inv, &cfg.root, cfg.base_url.as_deref()).await,
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
                "Lectern decks as cms:Presentation resources on the CMS axis: title + tags \
                 from the deck's authored JSON-LD (scraped title slide as fallback), plus \
                 venue path segments, linked to the built deck. A view is a query.",
            )
            .verb(Verb::Source)
    }
}

/// The whole presentations graph: a `cms:Presentation` block per deck under `root`, each
/// linked at `base_url` (or `file://` when none is configured). Deck *files* are read
/// through the kernel (`urn:cms:deck:*`, see [`deck_source`]) so the graph is golden-threaded
/// to them — a `lectern build` that rewrites a deck cuts the thread and this recomputes.
async fn presentations_turtle(inv: &Invocation<'_>, root: &Path, base_url: Option<&str>) -> String {
    let mut out = String::from(
        "@prefix dc: <http://purl.org/dc/elements/1.1/> .\n\
         @prefix cms: <https://ikigai-rs.dev/ns/cms#> .\n",
    );
    // The directory *walk* (which decks exist) stays on std::fs — a brand-new deck dir still
    // needs a restart — but each deck's contents ride a golden thread.
    for deck in decks(root) {
        if let Some(block) = deck_turtle(inv, &deck, root, base_url).await {
            out.push_str(&block);
        }
    }
    out
}

/// Read a deck file (given its absolute path under `root`) THROUGH the kernel as
/// `urn:cms:deck:{rel}`, so the read is golden-threaded (and capability-gated) rather than a
/// raw `std::fs` read. `None` if the file is absent/unreadable or outside `root`.
async fn deck_source(inv: &Invocation<'_>, root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let rel_url = rel
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/");
    let iri = Iri::parse(format!("urn:cms:deck:{rel_url}")).ok()?;
    let repr = inv.source(&iri).await.ok()?;
    String::from_utf8(repr.bytes).ok()
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

/// One deck → a `cms:Presentation` block, or `None` if it carries no metadata at all (no
/// built JSON-LD and no readable title slide).
async fn deck_turtle(
    inv: &Invocation<'_>,
    deck: &Path,
    root: &Path,
    base_url: Option<&str>,
) -> Option<String> {
    let (title, mut tags) = deck_metadata(inv, deck, root).await?;
    // The venue tags come from the path, not the deck — always added, whatever the source.
    tags.extend(venue_tags(deck, root));
    tags.sort();
    tags.dedup();
    let title = title.unwrap_or_else(|| pretty_slug(deck));

    let iri = deck_iri(deck, root);
    let mut ttl = format!(
        "<{iri}> a cms:Presentation ; dc:title {} ; dc:identifier {}",
        ttl_str(&title),
        ttl_str(&deck_link(deck, root, base_url)),
    );
    for tag in &tags {
        ttl.push_str(&format!(" ; dc:subject {}", ttl_str(tag)));
    }
    ttl.push_str(" .\n");
    Some(ttl)
}

/// A deck's authored `(title, tags)`: its built **JSON-LD** if present (the agreed
/// interface), else a scrape of the title slide for un-migrated/un-built decks. `None` when
/// the deck has neither. Venue tags are added by the caller, not here.
async fn deck_metadata(
    inv: &Invocation<'_>,
    deck: &Path,
    root: &Path,
) -> Option<(Option<String>, Vec<String>)> {
    match jsonld_metadata(inv, deck, root).await {
        Some(m) => Some(m),
        None => scraped_metadata(inv, deck, root).await,
    }
}

/// Authored metadata from the deck's built JSON-LD (`<script type="application/ld+json">` in
/// `dist/index.html`). The context maps `title`→`dc:title` and `tags`→`dc:subject`, so we
/// read those compact keys. `None` when the deck isn't built or carries no JSON-LD.
async fn jsonld_metadata(
    inv: &Invocation<'_>,
    deck: &Path,
    root: &Path,
) -> Option<(Option<String>, Vec<String>)> {
    let html = deck_source(inv, root, &deck.join("dist/index.html")).await?;
    let block = extract_ld_json(&html)?;
    let json: serde_json::Value = serde_json::from_str(&block).ok()?;
    let title = json["title"]
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty());
    // `tags` is a `@set` (an array), but tolerate a lone string too.
    let tags = match &json["tags"] {
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_lowercase())
            .collect(),
        serde_json::Value::String(s) => vec![s.to_lowercase()],
        _ => Vec::new(),
    };
    Some((title, tags))
}

/// The body of the first `<script type="application/ld+json">…</script>` in an HTML string.
fn extract_ld_json(html: &str) -> Option<String> {
    let open = "<script type=\"application/ld+json\">";
    let i = html.find(open)?;
    let after = &html[i + open.len()..];
    let end = after.find("</script>")?;
    Some(after[..end].trim().to_string())
}

/// Fallback metadata for a deck with no JSON-LD: only the title slide's first `# H1` for the
/// title — **no tags**. The visual `<span class="tag …">` chips are presentation, not
/// semantics, so they never become CMS tags (only authored JSON-LD tags + venue tags do).
/// So an un-migrated deck shows up with its title and venue tag, and gets real tags once its
/// `[metadata]` is authored and built. `None` when the deck has no readable title slide.
async fn scraped_metadata(
    inv: &Invocation<'_>,
    deck: &Path,
    root: &Path,
) -> Option<(Option<String>, Vec<String>)> {
    let slide = title_slide(deck)?;
    let text = deck_source(inv, root, &slide).await?;
    Some((first_h1(title_region(&text)), Vec::new()))
}

/// The card link for a deck: its built `dist/index.html` served under `base_url` (so a
/// click opens the deck to present), or a `file://` path when no server base is configured
/// (locatable but not clickable from an https page). The served URL is emitted whether or
/// not the deck is built yet — it resolves once `lectern build` has run.
fn deck_link(deck: &Path, root: &Path, base_url: Option<&str>) -> String {
    match base_url {
        Some(base) => {
            let rel = deck.strip_prefix(root).unwrap_or(deck);
            let rel_url = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
            format!("{}/{}/dist/index.html", base.trim_end_matches('/'), rel_url)
        }
        None => {
            let built = deck.join("dist/index.html");
            let target = if built.is_file() {
                built
            } else {
                deck.to_path_buf()
            };
            format!("file://{}", target.display())
        }
    }
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
    use ikigai_core::Request;
    use ikigai_resolve::Resolver;

    /// Write a deck (deck.toml + slides/00-title.md) under `dir`, return the deck dir.
    fn write_deck(dir: &Path, rel: &str, title_md: &str) -> PathBuf {
        let deck = dir.join(rel);
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        std::fs::write(deck.join("slides/00-title.md"), title_md).unwrap();
        deck
    }

    /// Resolve `urn:cms:graph:presentations` over a real kernel rooted at `root` — the
    /// threaded path (deck files read through `urn:cms:deck:*`), returning the Turtle.
    fn presentations_ttl(root: &Path, base_url: Option<&str>) -> String {
        let src = tempfile::tempdir().unwrap();
        let kernel = crate::build_cms_kernel_with(
            src.path().to_path_buf(),
            None,
            Some(Presentations {
                root: root.to_path_buf(),
                base_url: base_url.map(str::to_string),
            }),
            None,
            crate::tagstore::TagPaths::in_dir(src.path()),
        );
        let (repr, _) = Resolver::issue(
            &kernel,
            Request::new(
                Verb::Source,
                Iri::parse("urn:cms:graph:presentations").unwrap(),
            ),
        )
        .expect("presentations graph resolves");
        String::from_utf8(repr.bytes).unwrap()
    }

    const TITLE_MD: &str = "<!-- .slide: class=\"slide inverse center middle\" -->\n\n\
        <span class=\"kicker\">NFJS · Virtual Workshop</span>\n\n\
        # Full Stack Engineering - Encryption\n\n\
        <span class=\"tag cool\">primitives</span> <span class=\"tag warm\">identity</span> \
        <span class=\"tag ink\">post-quantum</span>\n";

    #[test]
    fn an_unmigrated_deck_gets_its_title_and_venue_but_no_span_tags() {
        let tmp = tempfile::tempdir().unwrap();
        // No dist/ → the scrape fallback: title from the H1, tags from JSON-LD only (none).
        write_deck(tmp.path(), "conferences/nfjs/fs-crypto", TITLE_MD);
        let ttl = presentations_ttl(tmp.path(), None);

        assert!(ttl.contains("a cms:Presentation"), "typed: {ttl}");
        assert!(
            ttl.contains("dc:title \"Full Stack Engineering - Encryption\""),
            "title from the H1: {ttl}"
        );
        // The visual .tag spans are NEVER read as CMS tags — presentation, not semantics.
        for t in ["primitives", "identity", "post-quantum"] {
            assert!(
                !ttl.contains(&format!("dc:subject \"{t}\"")),
                "span `{t}` must not become a tag: {ttl}"
            );
        }
        // The only tag is the venue, from the path.
        assert!(ttl.contains("dc:subject \"nfjs\""), "venue tag: {ttl}");
        assert!(ttl.contains("dc:identifier \"file://"), "deck link: {ttl}");
    }

    #[test]
    fn built_json_ld_wins_over_the_scraped_title_slide() {
        let tmp = tempfile::tempdir().unwrap();
        let deck = tmp.path().join("conferences/nfjs/fs-crypto");
        std::fs::create_dir_all(deck.join("slides")).unwrap();
        std::fs::create_dir_all(deck.join("dist")).unwrap();
        std::fs::write(deck.join("deck.toml"), "title = \"x\"\n").unwrap();
        // A title slide that says "Encryption" with a "primitives" span — both must be
        // OVERRIDDEN by the authored JSON-LD (the real fs-crypto case: H1 and metadata diverge).
        std::fs::write(
            deck.join("slides/00-title.md"),
            "# Full Stack Engineering - Encryption\n\n<span class=\"tag ink\">primitives</span>\n",
        )
        .unwrap();
        std::fs::write(
            deck.join("dist/index.html"),
            "<head>\n<script type=\"application/ld+json\">\n{\
             \"@context\":{\"dc\":\"http://purl.org/dc/elements/1.1/\",\
             \"cms\":\"https://ikigai-rs.dev/ns/cms#\",\"title\":\"dc:title\",\
             \"tags\":{\"@id\":\"dc:subject\",\"@container\":\"@set\"}},\
             \"@type\":\"cms:Presentation\",\
             \"title\":\"Full Stack Engineering - Cryptography\",\
             \"tags\":[\"cryptography\",\"security\"]}\n</script>\n</head>",
        )
        .unwrap();

        let ttl = presentations_ttl(tmp.path(), None);
        // Authored title wins over the stale H1.
        assert!(
            ttl.contains("dc:title \"Full Stack Engineering - Cryptography\""),
            "authored title from JSON-LD: {ttl}"
        );
        assert!(!ttl.contains("Encryption"), "stale H1 not used: {ttl}");
        // Authored tags, not the scraped span.
        assert!(
            ttl.contains("dc:subject \"cryptography\"") && ttl.contains("dc:subject \"security\""),
            "authored tags: {ttl}"
        );
        assert!(
            !ttl.contains("dc:subject \"primitives\""),
            "the scraped span is ignored when JSON-LD is present: {ttl}"
        );
        // The venue tag is still merged from the path.
        assert!(
            ttl.contains("dc:subject \"nfjs\""),
            "venue tag still added: {ttl}"
        );
    }

    #[test]
    fn a_base_url_makes_the_link_a_clickable_served_deck() {
        // deck_link is pure — a base URL yields the served deck URL (trailing slash trimmed);
        // no base yields a file:// path.
        let root = Path::new("/decks");
        let deck = root.join("conferences/nfjs/fs-crypto");
        assert_eq!(
            deck_link(&deck, root, Some("http://localhost:8000/")),
            "http://localhost:8000/conferences/nfjs/fs-crypto/dist/index.html"
        );
        assert!(deck_link(&deck, root, None).starts_with("file://"));
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
        // A single-file deck: the title H1 comes from the region before the first `---`; a
        // later `# Agenda` content heading must not become the title.
        let single = "# Real Title\n\n---\n# Agenda\n";
        assert_eq!(
            first_h1(title_region(single)).as_deref(),
            Some("Real Title")
        );
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
