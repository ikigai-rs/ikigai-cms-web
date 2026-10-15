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
//! Native-only. The graph reads nothing from disk itself (ledger #286). Which decks exist is
//! the resource `urn:cms:decks` (the listing: the one place the deck tree is walked), and each
//! deck's **contents** are read through the deck jail (`urn:cms:deck:*`). Both are cacheable,
//! each under its own golden thread, so the graph hangs from all of them. What cuts those
//! threads is the [`DeckWatch`] a serving host runs over the decks root: a new deck directory
//! cuts the listing and a `lectern build` that rewrites a deck cuts its files, and the reading
//! room recomputes on its next read without a restart. A kernel with no watch serves what it
//! read first until something cuts. The durable end state is the graph as source of truth with
//! lectern rendering *from* it.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ikigai_core::{
    Description, Endpoint, Invocation, Iri, ReprType, Representation, Request, Result, Verb,
};

/// The deck listing, and the golden thread it hangs from: a [`DeckWatch`] cuts this name
/// whenever anything under the decks root changes, so a deck that appears, moves or goes away
/// reaches the graph on its next read.
pub(crate) const DECKS_IRI: &str = "urn:cms:decks";

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
        crate::native::serves(inv, &self.describe())?;
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
            .output(crate::native::TURTLE)
            // Deck files are read through `urn:cms:deck:*`, an fs jail like the sources.
            .requires(crate::native::FS_READ)
    }
}

/// The whole presentations graph: a `cms:Presentation` block per deck the listing names, each
/// linked at `base_url` (or `file://` when none is configured). Nothing here touches the disk:
/// which decks exist comes from `urn:cms:decks` ([`DeckListing`]) and every deck file is read
/// through the deck jail ([`deck_source`]), so the graph hangs from their threads and
/// recomputes when a [`DeckWatch`] cuts one.
async fn presentations_turtle(inv: &Invocation<'_>, root: &Path, base_url: Option<&str>) -> String {
    let mut out = String::from(
        "@prefix dc: <http://purl.org/dc/elements/1.1/> .\n\
         @prefix cms: <https://ikigai-rs.dev/ns/cms#> .\n",
    );
    let listing = match inv.source(&Iri::parse(DECKS_IRI).expect("valid IRI")).await {
        Ok(repr) => String::from_utf8_lossy(&repr.bytes).into_owned(),
        // A refused listing (no read of the decks root) is no decks, as a refused deck read
        // always was. An error is never cached, so the graph is not cached either.
        Err(_) => return out,
    };
    for (deck, title_slide) in parse_listing(&listing, root) {
        if let Some(block) = deck_turtle(inv, &deck, title_slide.as_deref(), root, base_url).await {
            out.push_str(&block);
        }
    }
    out
}

/// The `urn:cms:deck:{rel}` name of a file at `abs` under `root`, each path segment
/// percent-encoded where an IRI cannot carry it raw. `None` outside `root`, or for a name the
/// IRI grammar still refuses (said once, on stderr).
fn deck_file_iri(root: &Path, abs: &Path) -> Option<Iri> {
    let rel = abs.strip_prefix(root).ok()?;
    // A deck under `talks/my talk` made an IRI with a space, `Iri::parse` refused it, and the
    // deck vanished from the room with nothing said. The deck jail decodes the binding back.
    match Iri::parse(format!("urn:cms:deck:{}", rel_iri(rel))) {
        Ok(iri) => Some(iri),
        Err(e) => {
            eprintln!("presentations: skipping {}: {e}", abs.display());
            None
        }
    }
}

/// A relative path as IRI text: its segments, each through [`iri_segment`], joined by `/`.
fn rel_iri(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| c.as_os_str().to_str())
        .map(iri_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Read a deck file (given its absolute path under `root`) THROUGH the kernel as
/// `urn:cms:deck:{rel}`, so the read is golden-threaded (and capability-gated) rather than a
/// raw `std::fs` read. `None` if the file is absent/unreadable or outside `root`.
async fn deck_source(inv: &Invocation<'_>, root: &Path, abs: &Path) -> Option<String> {
    let iri = deck_file_iri(root, abs)?;
    let repr = inv.source(&iri).await.ok()?;
    String::from_utf8(repr.bytes).ok()
}

/// Whether the deck jail answers that the file at `abs` exists, under the caller's capability:
/// `false` for a file that is absent, outside `root`, or one the caller may not read.
async fn deck_exists(inv: &Invocation<'_>, root: &Path, abs: &Path) -> bool {
    let Some(iri) = deck_file_iri(root, abs) else {
        return false;
    };
    matches!(inv.issue(Request::new(Verb::Exists, iri)).await, Ok(r) if r.bytes == b"true")
}

/// `urn:cms:decks` — which decks exist under the presentations root, and each one's title
/// slide. The only place the deck tree is walked: the directory tree is this resource's state,
/// and the graph composes it rather than reading the disk.
///
/// `text/plain`, one deck per line, as paths relative to the root in IRI text (each segment
/// through [`iri_segment`], so a raw space never appears): `{deck}` alone, or `{deck} {slide}`
/// when the deck has a title slide, the slide relative to the deck. A deck is listed only when
/// the caller may read its `deck.toml` through the deck jail, and a slide only when the caller
/// may read it, so the listing names nothing the caller's grants do not reach. Cacheable under
/// its own name, which a [`DeckWatch`] cuts.
pub(crate) struct DeckListing {
    pub root: PathBuf,
}

#[async_trait]
impl Endpoint for DeckListing {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        crate::native::serves(inv, &self.describe())?;
        let mut out = String::new();
        for deck in decks(&self.root) {
            if !deck_exists(inv, &self.root, &deck.join("deck.toml")).await {
                continue;
            }
            let Ok(rel) = deck.strip_prefix(&self.root) else {
                continue;
            };
            out.push_str(&rel_iri(rel));
            if let Some(slide) = title_slide(inv, &self.root, &deck).await {
                if let Ok(slide) = slide.strip_prefix(&deck) {
                    out.push(' ');
                    out.push_str(&rel_iri(slide));
                }
            }
            out.push('\n');
        }
        Ok(Representation::new(
            ReprType::new("text/plain").with_param("charset", "utf-8"),
            out.into_bytes(),
        )
        .cacheable()
        .depends_on(DECKS_IRI))
    }

    fn name(&self) -> &str {
        "cms-decks"
    }

    fn describe(&self) -> Description {
        Description::new(DECKS_IRI)
            .summary(
                "The lectern decks under the presentations root, one per line as root-relative \
                 IRI path text, each followed by its title slide when it has one. Lists only \
                 what the caller may read through the deck jail.",
            )
            .verb(Verb::Source)
            .output("text/plain")
            .requires(crate::native::FS_READ)
    }
}

/// The listing's lines as `(deck dir, title slide)` absolute paths under `root`. A line that
/// is empty is skipped; anything else is taken as the listing wrote it.
fn parse_listing(listing: &str, root: &Path) -> Vec<(PathBuf, Option<PathBuf>)> {
    listing
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let (deck, slide) = match line.split_once(' ') {
                Some((deck, slide)) => (deck, Some(slide)),
                None => (line, None),
            };
            let deck = root.join(crate::native::pct_decode(deck));
            let slide = slide.map(|s| deck.join(crate::native::pct_decode(s)));
            (deck, slide)
        })
        .collect()
}

/// One path segment as IRI text: every byte an IRI path segment cannot carry raw — whitespace,
/// controls, `%`, and the delimiters `<>"{}|\^`, `?` and `#` — percent-encoded; the rest (non-ASCII
/// included, which an IRI allows) kept.
fn iri_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for c in segment.chars() {
        if c.is_whitespace()
            || c.is_control()
            || matches!(
                c,
                '%' | '<' | '>' | '"' | '{' | '}' | '|' | '\\' | '^' | '`' | '?' | '#'
            )
        {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
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

/// One deck → a `cms:Presentation` block, or `None` if it carries no metadata at all (no
/// built JSON-LD and no readable title slide).
async fn deck_turtle(
    inv: &Invocation<'_>,
    deck: &Path,
    title_slide: Option<&Path>,
    root: &Path,
    base_url: Option<&str>,
) -> Option<String> {
    let (title, mut tags) = deck_metadata(inv, deck, title_slide, root).await?;
    // The venue tags come from the path, not the deck — always added, whatever the source.
    tags.extend(venue_tags(deck, root));
    tags.sort();
    tags.dedup();
    let title = title.unwrap_or_else(|| pretty_slug(deck));

    let iri = deck_iri(deck, root);
    // Only a `file://` link asks whether the deck is built; a served link names the built deck
    // either way.
    let built = base_url.is_none() && deck_exists(inv, root, &deck.join("dist/index.html")).await;
    let mut ttl = format!(
        "<{iri}> a cms:Presentation ; dc:title {} ; dc:identifier {}",
        ttl_str(&title),
        ttl_str(&deck_link(deck, root, base_url, built)),
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
    title_slide: Option<&Path>,
    root: &Path,
) -> Option<(Option<String>, Vec<String>)> {
    match jsonld_metadata(inv, deck, root).await {
        Some(m) => Some(m),
        None => scraped_metadata(inv, title_slide?, root).await,
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
    slide: &Path,
    root: &Path,
) -> Option<(Option<String>, Vec<String>)> {
    let text = deck_source(inv, root, slide).await?;
    Some((first_h1(title_region(&text)), Vec::new()))
}

/// The card link for a deck: its built `dist/index.html` served under `base_url` (so a
/// click opens the deck to present), or a `file://` path when no server base is configured
/// (locatable but not clickable from an https page). The served URL is emitted whether or
/// not the deck is built yet — it resolves once `lectern build` has run. `built` says whether
/// the deck's `dist/index.html` exists, which only the `file://` form uses: the caller asks
/// the deck jail, so this reads nothing from disk.
fn deck_link(deck: &Path, root: &Path, base_url: Option<&str>, built: bool) -> String {
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
            let target = if built {
                deck.join("dist/index.html")
            } else {
                deck.to_path_buf()
            };
            format!("file://{}", target.display())
        }
    }
}

/// A deck's title slide: `slides/00-title.md` if present, else the lexically-first
/// `slides/*.md` (single-file decks carry the title slide at the top of that file). Part of the
/// listing's walk ([`DeckListing`]); each candidate must also exist through the deck jail under
/// the caller's capability, so a slide the caller may not read is never named.
async fn title_slide(inv: &Invocation<'_>, root: &Path, deck: &Path) -> Option<PathBuf> {
    let slides = deck.join("slides");
    let mut candidates = vec![slides.join("00-title.md")];
    let mut mds: Vec<PathBuf> = std::fs::read_dir(&slides)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    mds.sort();
    candidates.extend(mds);
    for candidate in candidates {
        if deck_exists(inv, root, &candidate).await {
            return Some(candidate);
        }
    }
    None
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

/// Escape a value for a Turtle double-quoted string literal (`STRING_LITERAL_QUOTE`), which
/// forbids exactly four characters raw: `"`, `\`, LF and CR. Every one is escaped; missing CR
/// was enough to make the whole `urn:cms:graph` unparseable, and every view with it, from one deck
/// title. Line breaks were flattened to a space before; they are escaped now, which renders the
/// same in a card (HTML collapses the whitespace) and keeps the value intact.
fn ttl_str(s: &str) -> String {
    let esc = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{esc}\"")
}

/// The threads a set of changed paths names, for a decks root spelled any of `roots` ways:
/// every path under the root names its own deck-jail thread (`urn:cms:deck:{rel}`, spelled
/// exactly as [`deck_file_iri`] spells it), and any path under the root at all names the
/// listing's thread ([`DECKS_IRI`]), first. A path outside every spelling names nothing.
///
/// Every change cuts the listing, not only a create or a remove: the platforms disagree about
/// which kind they report for an editor's atomic save or a rename, and a re-walk is cheap
/// beside a deck that never appears. Pure: the decision, separated from the platform feeding it.
#[cfg(feature = "server")]
fn deck_threads_for<'a>(
    roots: &[PathBuf],
    paths: impl IntoIterator<Item = &'a Path>,
) -> Vec<String> {
    let mut cut: Vec<String> = Vec::new();
    for path in paths {
        let Some(rel) = roots.iter().find_map(|root| path.strip_prefix(root).ok()) else {
            continue;
        };
        if cut.is_empty() {
            cut.push(DECKS_IRI.to_string());
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let thread = format!("urn:cms:deck:{}", rel_iri(rel));
        if !cut.contains(&thread) {
            cut.push(thread);
        }
    }
    cut
}

/// The watch that keeps `urn:cms:graph:presentations` fresh in a serving process (ledger
/// #286): the platform watcher (FSEvents / inotify) over the decks root, recursively, cutting
/// the listing's thread and each changed file's deck-jail thread **by name**. The kernel is
/// neither restarted nor rebuilt; the graph recomputes on its next read.
///
/// Declaring a thread is a promise that something cuts it, and before this nothing did: the
/// deck files were cached under their names in a process that ran no watcher, so an edited
/// deck was served stale until a restart, and a new deck (found by a walk outside the kernel)
/// was invisible until one. Same shape as ikigai-browse's `ConfigWatch`: the watch is split
/// from the thread that drives it, so a test drives the notification path
/// ([`apply_next`](Self::apply_next)) rather than sleeping.
///
/// The platform reports canonical paths (macOS maps `/var` to `/private/var`) while the
/// threads are named after the root the kernel was GIVEN, so a changed path is placed against
/// both spellings.
#[cfg(feature = "server")]
pub struct DeckWatch {
    kernel: std::sync::Arc<ikigai_core::Kernel>,
    /// The decks root as given and, when it differs, as the platform will report it.
    roots: Vec<PathBuf>,
    events: std::sync::mpsc::Receiver<notify::Result<notify::Event>>,
    /// Held for the watch's lifetime: dropping it ends the platform watch and closes
    /// `events`, which ends [`run`](Self::run).
    _watcher: notify::RecommendedWatcher,
}

#[cfg(feature = "server")]
impl DeckWatch {
    /// Start watching `root` on `kernel`'s behalf, and hand the watch back to be driven.
    /// Fails, rather than silently watching nothing, when the platform watcher cannot be
    /// created or pointed at the root (a root that does not exist is the usual cause).
    pub fn start(
        kernel: std::sync::Arc<ikigai_core::Kernel>,
        root: &Path,
    ) -> std::result::Result<Self, String> {
        use notify::Watcher;
        let mut roots = vec![root.to_path_buf()];
        if let Ok(canonical) = root.canonicalize() {
            if canonical != root {
                roots.push(canonical);
            }
        }
        let (tx, events) = std::sync::mpsc::channel();
        let platform = |e: notify::Error| format!("cannot watch {}: {e}", root.display());
        let mut watcher = notify::recommended_watcher(move |res| {
            // A closed receiver means the watch was dropped; nothing to report to.
            let _ = tx.send(res);
        })
        .map_err(platform)?;
        watcher
            .watch(root, notify::RecursiveMode::Recursive)
            .map_err(platform)?;
        Ok(DeckWatch {
            kernel,
            roots,
            events,
            _watcher: watcher,
        })
    }

    /// [`start`](Self::start), then drive the watch on a detached thread for the process's
    /// lifetime: the serving host's one line.
    pub fn spawn(
        kernel: std::sync::Arc<ikigai_core::Kernel>,
        root: &Path,
    ) -> std::result::Result<(), String> {
        let watch = Self::start(kernel, root)?;
        std::thread::spawn(move || watch.run());
        Ok(())
    }

    /// Cut every thread a change names. Returns the threads cut: empty for an access (a read
    /// changes no content), a path outside the root, or a watcher error (which names no path).
    fn apply(&self, event: notify::Result<notify::Event>) -> Vec<String> {
        let Ok(event) = event else {
            return Vec::new();
        };
        if event.kind.is_access() {
            return Vec::new();
        }
        let cut = deck_threads_for(&self.roots, event.paths.iter().map(PathBuf::as_path));
        for thread in &cut {
            self.kernel.cut(thread.as_str());
        }
        cut
    }

    /// Wait up to `timeout` for the watcher's next notification and apply it: `None` when
    /// nothing arrived in time or the watch has ended, else the threads it cut. The deadline
    /// is an upper bound on platform latency, not a guess at it.
    pub fn apply_next(&self, timeout: std::time::Duration) -> Option<Vec<String>> {
        self.events
            .recv_timeout(timeout)
            .ok()
            .map(|event| self.apply(event))
    }

    /// Drive the watch until its channel closes: in practice, for the process's lifetime.
    pub fn run(self) {
        for event in self.events.iter() {
            self.apply(event);
        }
    }
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
            None,
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
            deck_link(&deck, root, Some("http://localhost:8000/"), false),
            "http://localhost:8000/conferences/nfjs/fs-crypto/dist/index.html"
        );
        assert_eq!(
            deck_link(&deck, root, None, false),
            "file:///decks/conferences/nfjs/fs-crypto"
        );
        assert_eq!(
            deck_link(&deck, root, None, true),
            "file:///decks/conferences/nfjs/fs-crypto/dist/index.html"
        );
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
