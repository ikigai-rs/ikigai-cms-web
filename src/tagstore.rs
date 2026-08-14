//! Tag overlays — two persisted Turtle graphs merged into `urn:cms:graph`, so *approved* and
//! *suggested* tags ride on cards without ever touching the source files. This matters because the
//! sources are regenerated (Zotero `My Library.rdf` is re-exported; org files own their own tags):
//! a write-back would be clobbered, so promotions live in a dedicated RDF overlay instead.
//!
//! - `urn:cms:graph:tags-approved` — `<resource> dc:subject "tag"`. A promoted tag is a *real* tag:
//!   it merges onto the same `dc:subject` axis, so the resource shows up in that tag's view and the
//!   chip renders like any other, no special-casing.
//! - `urn:cms:graph:suggestions` — `<resource> cms:suggestedTag "tag"`. Provisional; a distinct
//!   predicate so the card can render it as a pending chip with a `+` (promote) and `x` (dismiss).
//!
//! Promote moves a triple suggestions→approved; dismiss drops it from suggestions. Both are
//! authorized mutations, mirroring the reviewed purge — never automatic.
//!
//! The on-disk format is one canonical triple per line with full IRIs (valid standalone Turtle,
//! and trivially line-parseable since we are the only writer):
//! `<iri> <predicate> "escaped tag" .`
//!
//! # What an overlay is keyed on
//!
//! The header above says overlays exist *because the sources are regenerated* — and for a long
//! time the key contradicted it. A book's graph IRI is `urn:cms:book:{SHA256(export subject)}`,
//! and for the 257 books in the export with no ISBN that subject is `#item_N`, a **per-export
//! ordinal that renumbers on every re-export**. The overlay key was itself regenerated: after a
//! re-export, a tag approved for the book that was `#item_500` landed on whatever book is
//! `#item_500` now — silently, and for approved, suggested and dismissed alike.
//!
//! So an overlay is keyed on the **durable** identity where one exists:
//! `urn:zotero:item:{KEY}` from the Zotero API, which [`crate::zotero`]'s pass writes into the link
//! overlay and which survives re-export. That key is storage-only; it is never what the graph sees.
//! Three rules keep the two apart:
//!
//! - **Write canonical** — every mutation maps the incoming graph IRI through the link overlay
//!   before it touches a file ([`TagPaths::canonical`]).
//! - **Read projected** — every served overlay maps the durable key back onto *today's*
//!   `urn:cms:book:{sha}` ([`Identities::project`]), so the graph joins as it always did. A durable
//!   key with no book in today's export emits nothing: `urn:cms:tags` counts `?s dc:subject ?tag`
//!   without requiring a title, so leaking a bare identity would inflate a tag's count.
//! - **Fall back to the current key** — a resource with no durable identity (a bookmark, a
//!   presentation, one of the 13 books the Zotero API has no unambiguous match for) keeps its
//!   existing IRI. Nothing is dropped for want of a match; [`TagPaths::migrate_to_durable_keys`]
//!   reports how many stayed behind, and re-running it after a better link sweep moves them.
//!
//! Bookmarks and presentations were never at risk — `urn:cms:bookmark:{sha}` hashes the URL, not
//! an ordinal — and pass through all three rules untouched.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DC_SUBJECT: &str = "http://purl.org/dc/elements/1.1/subject";
const CMS_SUGGESTED: &str = "https://ikigai-rs.dev/ns/cms#suggestedTag";
/// A tag the human dismissed for a resource — recorded so the tag-suggest pass never re-suggests it
/// (the negative half of the feedback loop). In `urn:cms:graph` so the pass's SPARQL can exclude a
/// fully-dismissed book from the candidate set; not rendered.
const CMS_DISMISSED: &str = "https://ikigai-rs.dev/ns/cms#dismissedTag";

/// The predicate the Zotero link overlay carries a book's durable identity on
/// (`<urn:cms:book:{sha}> cms:zoteroItem <urn:zotero:item:{KEY}>`). Read here, not only by
/// `urn:cms:graph:zotero-links`, because that overlay is where the durable key comes from.
const CMS_ZOTERO_ITEM: &str = "https://ikigai-rs.dev/ns/cms#zoteroItem";

/// The prefix of a durable overlay key. A subject carrying it is an *identity*, not a resource in
/// today's graph: it is projected onto the current book IRI on the way out, and dropped if the
/// export no longer holds that book.
const DURABLE_PREFIX: &str = "urn:zotero:item:";

/// The overlay files as explicit paths, threaded from the config into every consumer
/// (the graph endpoints, the tag-suggest pass) — no process-global state, so tests hand each
/// kernel its own tempdir store.
///
/// `zotero_links` is the odd one out: not a tag overlay but the same mechanism — a derived
/// Turtle sidecar the graph joins against, written by a pass, never by hand. It rides here so a
/// test's tempdir covers every overlay at once rather than half of them.
///
/// # Two views of an overlay, and which one is true
///
/// Since the durable-key change, an overlay file and the graph it serves do **not** hold the same
/// subject, and that difference is correct and permanent:
///
/// - [`entries`] is the **file**, verbatim. A book the Zotero API matched is stored under
///   `urn:zotero:item:{KEY}`, because that is the identity that survives a re-export.
/// - `*_turtle` is the **graph**, projected onto today's `urn:cms:book:{sha}` so joins work as
///   they always did.
///
/// **The file is authoritative; the projection is a view.** Storage is keyed on what is durable,
/// and `urn:cms:book:{sha}` is not — it hashes the export subject, which for the 257 no-ISBN books
/// is a `#item_N` ordinal that renumbers on every re-export. That is the whole reason for the
/// re-key. The projection exists only so nothing downstream of `urn:cms:graph` had to change.
///
/// So the two disagreeing is the design, not a bug to reconcile. Do not "fix" it by making
/// `entries` project, which would hand callers a view and lose the durable key on the next write;
/// and do not make the served graph emit raw keys, which would put identities that are not
/// resources into `urn:cms:graph` — `urn:cms:tags` counts `?s dc:subject ?tag` without requiring a
/// title, so a leaked bare identity silently inflates a tag's count.
///
/// When writing a test: assert on `entries` for what was *stored*, on `*_turtle` for what the room
/// *sees*. A fixture whose tempdir has no `cms-zotero-links.ttl` has nothing to canonicalize
/// against, so the two coincide there — which is why a test can pass while telling you nothing
/// about this distinction.
#[derive(Clone, Debug)]
pub struct TagPaths {
    pub approved: PathBuf,
    pub suggestions: PathBuf,
    pub dismissed: PathBuf,
    pub zotero_links: PathBuf,
}

impl TagPaths {
    /// The canonical file names under `dir` (any directory — tests use a tempdir).
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            approved: dir.join("cms-tags-approved.ttl"),
            suggestions: dir.join("cms-tag-suggestions.ttl"),
            dismissed: dir.join("cms-tag-dismissed.ttl"),
            zotero_links: dir.join("cms-zotero-links.ttl"),
        }
    }

    /// The default store: `{home}/.ikigai` — the ikigai-owned state dir (created if
    /// missing), kept out of synced content. `cms.toml` keys / flags override per file.
    pub fn in_state_dir(home: &Path) -> Self {
        let dir = home.join(".ikigai");
        let _ = std::fs::create_dir_all(&dir);
        Self::in_dir(&dir)
    }

    /// [`TagPaths::in_state_dir`] under `$HOME` — for the no-config kernel constructors.
    pub fn default_home() -> Self {
        Self::in_state_dir(
            &std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default(),
        )
    }
}

/// One overlay triple: a resource IRI and the tag literal it carries.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub iri: String,
    pub tag: String,
}

/// Parse an overlay file's entries (empty if absent/unreadable). Tolerant: skips any line that
/// isn't our canonical `<iri> <pred> "tag" .` shape.
pub fn entries(path: &Path) -> Vec<Entry> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let line = line.trim();
    let iri = line.strip_prefix('<')?;
    let (iri, rest) = iri.split_once('>')?;
    // rest = ` <pred> "tag" .`
    let rest = rest.trim_start();
    let after_pred = rest.strip_prefix('<')?.split_once('>')?.1.trim_start();
    let inner = after_pred.strip_prefix('"')?;
    let close = inner.rfind("\" .")?;
    Some(Entry {
        iri: iri.to_string(),
        tag: unescape(&inner[..close]),
    })
}

/// The two directions of the book↔identity map, read from the Zotero link overlay.
///
/// Built from the `cms:zoteroItem` triples alone — the same file `urn:cms:graph:zotero-links`
/// serves, read with `std::fs` exactly as that endpoint does. Empty when the pass has never run,
/// which is the honest answer: with no map, every rule below is the identity function and the
/// overlays behave as they did before.
#[derive(Clone, Debug, Default)]
pub struct Identities {
    /// `urn:cms:book:{sha}` → `urn:zotero:item:{KEY}`. At most one identity per book — the pass
    /// writes one `cms:zoteroItem` per matched book.
    to_durable: HashMap<String, String>,
    /// `urn:zotero:item:{KEY}` → every book IRI that IS that item. A Vec, not an Option: two
    /// duplicate entries in the export can carry the same ISBN and so match one API item, and a
    /// tag on that item belongs on *both* cards — collapsing to one would hide a card's tag.
    to_current: HashMap<String, Vec<String>>,
}

impl Identities {
    /// Read the link overlay (empty if absent/unreadable — never an error, mirroring
    /// `urn:cms:graph:zotero-links`, where a failure would take the book half of the room down).
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut ids = Self::default();
        for (book, item) in text.lines().filter_map(parse_link_line) {
            ids.to_durable.insert(book.clone(), item.clone());
            ids.to_current.entry(item).or_default().push(book);
        }
        for books in ids.to_current.values_mut() {
            books.sort();
            books.dedup();
        }
        ids
    }

    /// The key an overlay entry is *stored* under: the durable identity when the resource has one,
    /// otherwise the IRI unchanged.
    ///
    /// Idempotent, and that is structural rather than lucky — the map's keys are all
    /// `urn:cms:book:*`, so a durable IRI can never be canonicalized a second time.
    pub fn canonical<'a>(&'a self, iri: &'a str) -> &'a str {
        self.to_durable.get(iri).map(String::as_str).unwrap_or(iri)
    }

    /// The IRIs an overlay entry is *served* under, for joining into `urn:cms:graph`.
    ///
    /// A durable key becomes today's book IRI (or several, for duplicates), and **nothing at all**
    /// when no book in the current export is that item — the human decision stays in the file,
    /// waiting for the book to come back, rather than leaking a titleless subject into the graph.
    /// Anything else passes through: idempotent for the same reason `canonical` is, since a book
    /// IRI is never a key of `to_current`.
    pub fn project(&self, iri: &str) -> Vec<String> {
        if let Some(books) = self.to_current.get(iri) {
            return books.clone();
        }
        if iri.starts_with(DURABLE_PREFIX) {
            return Vec::new();
        }
        vec![iri.to_string()]
    }

    /// How many books have a durable identity — the coverage the migration report quotes.
    pub fn len(&self) -> usize {
        self.to_durable.len()
    }

    pub fn is_empty(&self) -> bool {
        self.to_durable.is_empty()
    }
}

/// Parse one `<book> <cms:zoteroItem> <item> .` line of the link overlay into its two IRIs.
/// Every other line (the `cms:readerUrl` literals, the attachment links) yields `None`.
fn parse_link_line(line: &str) -> Option<(String, String)> {
    let (subject, rest) = line.trim().strip_prefix('<')?.split_once("> <")?;
    let (predicate, rest) = rest.split_once("> <")?;
    if predicate != CMS_ZOTERO_ITEM {
        return None;
    }
    let (object, tail) = rest.split_once('>')?;
    (tail.trim() == ".").then(|| (subject.to_string(), object.to_string()))
}

fn serialize(entries: &[Entry], predicate: &str) -> String {
    let mut lines: Vec<String> = entries
        .iter()
        .map(|e| format!("<{}> <{predicate}> \"{}\" .", e.iri, escape(&e.tag)))
        .collect();
    lines.sort();
    lines.dedup();
    let mut s = lines.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn write_entries(path: &Path, entries: &[Entry], predicate: &str) {
    let _ = std::fs::write(path, serialize(entries, predicate));
}

/// Turtle string-literal escaping (the subset a tag can contain).
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other), // \\ and \" land here
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl TagPaths {
    /// The book↔identity map behind [`TagPaths::canonical`] and the served projections, read fresh
    /// from the link overlay.
    ///
    /// Fresh on every call, deliberately. The alternative is a memo, and a memo here would be a
    /// staleness bug: the link overlay is rewritten by a pass that runs *while the server serves*,
    /// and a book that just gained an identity must be keyed on it by the next write.
    ///
    /// The cost was measured rather than assumed, because a source added to a hot read is how the
    /// last performance regression here happened. Against the live 771 KB / 4,819-line overlay
    /// (1,607 identities), release build: **1.0 ms per load, 2.7 ms for all three overlays** —
    /// against a `urn:cms:graph` assembly that is uncacheable, reads that same file whole anyway,
    /// and takes ~1 s. Under 0.3%, and it buys a memo-free store with no staleness window.
    pub fn identities(&self) -> Identities {
        Identities::load(&self.zotero_links)
    }

    /// The key `iri`'s overlay entries are stored under — its durable Zotero identity if it has
    /// one, else `iri` itself. See the module header for why storage and graph keys differ.
    pub fn canonical(&self, iri: &str) -> String {
        self.identities().canonical(iri).to_string()
    }

    /// One overlay's entries as the graph should see them: every stored key projected back onto
    /// today's book IRIs, durable keys whose book has left the export dropped.
    fn projected(&self, path: &Path) -> Vec<Entry> {
        let ids = self.identities();
        entries(path)
            .into_iter()
            .flat_map(|e| {
                ids.project(&e.iri)
                    .into_iter()
                    .map(move |iri| Entry {
                        iri,
                        tag: e.tag.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Add a suggestion `<iri> cms:suggestedTag "tag"` (idempotent — no duplicate).
    pub fn add_suggestion(&self, iri: &str, tag: &str) {
        let mut es = entries(&self.suggestions);
        let e = Entry {
            iri: self.canonical(iri),
            tag: tag.to_string(),
        };
        if !es.contains(&e) {
            es.push(e);
            write_entries(&self.suggestions, &es, CMS_SUGGESTED);
        }
    }

    /// Promote a suggestion to a real tag: drop it from suggestions, add `<iri> dc:subject "tag"`
    /// to the approved overlay (idempotent). Returns false if the suggestion wasn't present.
    pub fn approve(&self, iri: &str, tag: &str) -> bool {
        let (target, raw) = self.targets(iri, tag);
        let mut sug = entries(&self.suggestions);
        let had = sug.contains(&target) || sug.contains(&raw);
        sug.retain(|e| *e != target && *e != raw);
        write_entries(&self.suggestions, &sug, CMS_SUGGESTED);

        let mut app = entries(&self.approved);
        if !app.contains(&target) {
            app.push(target);
            write_entries(&self.approved, &app, DC_SUBJECT);
        }
        had
    }

    /// Dismiss a suggestion: drop it from the suggestions overlay AND remember the dismissal, so
    /// the tag-suggest pass won't re-suggest it (and a fully-dismissed book drops out of the
    /// candidate set). Returns false if the suggestion wasn't present.
    pub fn reject(&self, iri: &str, tag: &str) -> bool {
        let (target, raw) = self.targets(iri, tag);
        let mut sug = entries(&self.suggestions);
        let had = sug.contains(&target) || sug.contains(&raw);
        sug.retain(|e| *e != target && *e != raw);
        write_entries(&self.suggestions, &sug, CMS_SUGGESTED);
        self.dismiss(iri, tag);
        had
    }

    /// Record that `tag` was dismissed for `iri` (idempotent) — the negative feedback signal.
    pub fn dismiss(&self, iri: &str, tag: &str) {
        let mut es = entries(&self.dismissed);
        let e = Entry {
            iri: self.canonical(iri),
            tag: tag.to_string(),
        };
        if !es.contains(&e) {
            es.push(e);
            write_entries(&self.dismissed, &es, CMS_DISMISSED);
        }
    }

    /// The entry a promote/dismiss writes, and the un-canonicalized one it also clears.
    ///
    /// The second is not redundant. A suggestion written before the migration is still keyed on the
    /// book IRI, and a `+` on it arrives with that same book IRI — which now canonicalizes to a
    /// durable key that the file has never heard of. Matching on the canonical form alone would
    /// leave the pending chip on the card forever. Clearing both makes the buttons work regardless
    /// of whether the store has been migrated yet.
    fn targets(&self, iri: &str, tag: &str) -> (Entry, Entry) {
        (
            Entry {
                iri: self.canonical(iri),
                tag: tag.to_string(),
            },
            Entry {
                iri: iri.to_string(),
                tag: tag.to_string(),
            },
        )
    }

    /// The distinct tags in the approved overlay — the human-curated vocabulary the tag-suggest
    /// pass always feeds the LLM as "strongly prefer these" (so an accepted tag steers future
    /// suggestions from its first acceptance, not only once it becomes frequent).
    pub fn approved_tags(&self) -> Vec<String> {
        let mut tags: Vec<String> = entries(&self.approved).into_iter().map(|e| e.tag).collect();
        tags.sort();
        tags.dedup();
        tags
    }

    /// The suggestions overlay as a Turtle string (for the `urn:cms:graph:suggestions` resource),
    /// projected onto today's book IRIs.
    pub fn suggestions_turtle(&self) -> String {
        serialize(&self.projected(&self.suggestions), CMS_SUGGESTED)
    }

    /// The approved overlay as a Turtle string (for the `urn:cms:graph:tags-approved` resource),
    /// projected onto today's book IRIs.
    pub fn approved_turtle(&self) -> String {
        serialize(&self.projected(&self.approved), DC_SUBJECT)
    }

    /// The dismissed overlay as a Turtle string (for the `urn:cms:graph:dismissed` resource),
    /// projected onto today's book IRIs.
    pub fn dismissed_turtle(&self) -> String {
        serialize(&self.projected(&self.dismissed), CMS_DISMISSED)
    }
}

// ---- the one-time rekey ---------------------------------------------------------------------

/// The suffix of the pre-migration copy of an overlay. Written once and never overwritten: it is
/// the last state that predates any rekeying, which is the copy worth keeping.
const BACKUP_SUFFIX: &str = ".pre-durable-keys.bak";

/// What [`TagPaths::migrate_to_durable_keys`] did, per run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrationReport {
    /// Entries rekeyed from a graph IRI onto a durable Zotero identity.
    pub moved: usize,
    /// Entries already keyed on a durable identity — a re-run's whole population.
    pub already: usize,
    /// Entries left on their current key for want of a durable identity: bookmarks and
    /// presentations (which never needed one) and the books the Zotero API had no unambiguous
    /// match for. **Not a failure** — see [`MigrationReport::at_risk`] for the ones that matter.
    pub unmatched: usize,
    /// The subset of `unmatched` that is a *book*, i.e. still keyed on an export ordinal or an
    /// ISBN-derived hash. These are what a re-run after a better link sweep would rescue, so this
    /// is the number worth printing.
    pub at_risk: usize,
    /// The overlay files actually rewritten (0 on a no-op re-run).
    pub rewritten: Vec<PathBuf>,
    /// Backups written this run. Empty when the pre-migration copies already exist.
    pub backups: Vec<PathBuf>,
}

impl MigrationReport {
    /// The one-line summary a bin prints. Shaped like the other passes' summaries.
    pub fn summary(&self) -> String {
        format!(
            "tag-overlay keys: {} moved onto urn:zotero:item:*, {} already durable, \
             {} left on the current key ({} of them books with no Zotero match); \
             {} file(s) rewritten, {} backed up",
            self.moved,
            self.already,
            self.unmatched,
            self.at_risk,
            self.rewritten.len(),
            self.backups.len(),
        )
    }
}

/// Why a rewrite was refused. Only ever built when the invariant below fails, which is to say never
/// in practice — it exists so that "never in practice" is enforced rather than asserted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationRefused {
    pub file: PathBuf,
    pub lost: Vec<Entry>,
}

impl std::fmt::Display for MigrationRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to rewrite {}: {} entr{} would stop reaching {} resource — left untouched",
            self.file.display(),
            self.lost.len(),
            if self.lost.len() == 1 { "y" } else { "ies" },
            if self.lost.len() == 1 { "its" } else { "their" },
        )
    }
}

impl TagPaths {
    /// Rekey the three overlays onto durable Zotero identities, in place, preserving every human
    /// decision. Safe to run on every startup: it is idempotent and writes nothing when there is
    /// nothing to move.
    ///
    /// The safety argument is one invariant, checked per file before anything is written:
    /// **every entry the old file held must still reach the same resource through the new file.**
    /// Projecting the rewritten entries and requiring that set to cover the old one proves it. It
    /// holds by construction — `project(canonical(x))` contains `x` whenever `x` is a book the link
    /// overlay knows, and `canonical` is the identity function on everything else — so a violation
    /// means the link overlay changed underfoot mid-run. In that case the file is left exactly as
    /// it was and the reason is returned; a partial rekey is the one outcome not worth having.
    ///
    /// Entry *count* is deliberately not the invariant. Two duplicate books that share one Zotero
    /// item collapse their identical tags into a single stored entry — the count drops, and the
    /// projection still puts the tag on both cards. Coverage is the honest test; count is not.
    pub fn migrate_to_durable_keys(
        &self,
    ) -> std::result::Result<MigrationReport, MigrationRefused> {
        let ids = self.identities();
        let mut report = MigrationReport::default();
        for (path, predicate) in [
            (&self.approved, DC_SUBJECT),
            (&self.suggestions, CMS_SUGGESTED),
            (&self.dismissed, CMS_DISMISSED),
        ] {
            let before = entries(path);
            let after: Vec<Entry> = before
                .iter()
                .map(|e| Entry {
                    iri: ids.canonical(&e.iri).to_string(),
                    tag: e.tag.clone(),
                })
                .collect();
            for (old, new) in before.iter().zip(&after) {
                if new.iri.starts_with(DURABLE_PREFIX) {
                    if old.iri == new.iri {
                        report.already += 1;
                    } else {
                        report.moved += 1;
                    }
                } else {
                    report.unmatched += 1;
                    if old.iri.starts_with("urn:cms:book:") {
                        report.at_risk += 1;
                    }
                }
            }
            if after == before {
                continue; // nothing to move in this file — no backup, no write, no mtime churn
            }
            // The invariant: does every old entry still reach its resource?
            let reachable: Vec<Entry> = after
                .iter()
                .flat_map(|e| {
                    ids.project(&e.iri)
                        .into_iter()
                        .map(move |iri| Entry {
                            iri,
                            tag: e.tag.clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let lost: Vec<Entry> = before
                .iter()
                .filter(|e| !reachable.contains(e))
                .cloned()
                .collect();
            if !lost.is_empty() {
                return Err(MigrationRefused {
                    file: path.clone(),
                    lost,
                });
            }
            // Back up the pre-migration file before the first rewrite, and only then: a later run
            // (after a link sweep matches more books) must not overwrite the original copy.
            let backup = path.with_extension(format!(
                "{}{BACKUP_SUFFIX}",
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default()
            ));
            if !backup.exists() && std::fs::copy(path, &backup).is_ok() {
                report.backups.push(backup);
            }
            write_entries(path, &after, predicate);
            report.rewritten.push(path.clone());
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrips_and_escapes() {
        let es = vec![Entry {
            iri: "urn:cms:book:abc".into(),
            tag: "quoted \"x\" & odd".into(),
        }];
        let ttl = serialize(&es, DC_SUBJECT);
        assert!(ttl.contains("<urn:cms:book:abc>"));
        assert!(ttl.contains("\\\"x\\\""), "quote escaped: {ttl}");
        // Re-parse gets the original tag back.
        let back: Vec<Entry> = ttl.lines().filter_map(parse_line).collect();
        assert_eq!(back, es);
    }

    #[test]
    fn approve_moves_suggestion_to_approved_and_reject_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let tags = TagPaths::in_dir(dir.path());

        tags.add_suggestion("urn:cms:book:1", "rust");
        tags.add_suggestion("urn:cms:book:1", "wasm");
        tags.add_suggestion("urn:cms:book:1", "rust"); // idempotent
        assert_eq!(entries(&tags.suggestions).len(), 2);

        // Promote one → it leaves suggestions and lands in approved as dc:subject + curated vocab.
        assert!(tags.approve("urn:cms:book:1", "rust"));
        assert!(!entries(&tags.suggestions).iter().any(|e| e.tag == "rust"));
        assert!(entries(&tags.approved)
            .iter()
            .any(|e| e.iri == "urn:cms:book:1" && e.tag == "rust"));
        assert_eq!(tags.approved_tags(), vec!["rust".to_string()]);

        // Dismiss the other → gone from suggestions, never in approved, and REMEMBERED as dismissed
        // (so the pass won't re-suggest it — the negative feedback).
        assert!(tags.reject("urn:cms:book:1", "wasm"));
        assert!(entries(&tags.suggestions).is_empty());
        assert!(!entries(&tags.approved).iter().any(|e| e.tag == "wasm"));
        assert!(entries(&tags.dismissed)
            .iter()
            .any(|e| e.iri == "urn:cms:book:1" && e.tag == "wasm"));
        // Re-reject is a no-op that reports "wasn't there".
        assert!(!tags.reject("urn:cms:book:1", "wasm"));
    }

    /// A store whose link overlay says `urn:cms:book:{sha}` IS `urn:zotero:item:{key}`.
    fn linked(pairs: &[(&str, &str)]) -> (tempfile::TempDir, TagPaths) {
        let dir = tempfile::tempdir().unwrap();
        let tags = TagPaths::in_dir(dir.path());
        let ttl: String = pairs
            .iter()
            .map(|(book, item)| format!("<{book}> <{CMS_ZOTERO_ITEM}> <{item}> .\n"))
            .collect();
        std::fs::write(&tags.zotero_links, ttl).unwrap();
        (dir, tags)
    }

    #[test]
    fn the_link_overlays_reader_urls_are_not_identities() {
        // The file the map is read from is mostly NOT identity triples: two thirds of it is reader
        // URLs and attachment links, and a sloppy parse would turn a literal into a book key.
        let ids = {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("links.ttl");
            std::fs::write(
                &p,
                "<urn:cms:book:a> <https://ikigai-rs.dev/ns/cms#zoteroItem> <urn:zotero:item:K1> .\n\
                 <urn:cms:book:a> <https://ikigai-rs.dev/ns/cms#zoteroAttachment> <urn:zotero:item:A1> .\n\
                 <urn:cms:book:a> <https://ikigai-rs.dev/ns/cms#readerUrl> \"https://www.zotero.org/u/items/A1\" .\n",
            )
            .unwrap();
            Identities::load(&p)
        };
        assert_eq!(ids.len(), 1, "only the zoteroItem triple is an identity");
        assert_eq!(ids.canonical("urn:cms:book:a"), "urn:zotero:item:K1");
        // The attachment is not a book's identity, so it projects to nothing rather than to a card.
        assert!(ids.project("urn:zotero:item:A1").is_empty());
    }

    #[test]
    fn a_decision_survives_the_export_renumbering_its_items() {
        // Before the re-export: `#item_500` hashes to `…:aaa`, and Brian approves a tag on it.
        let (_d, tags) = linked(&[("urn:cms:book:aaa", "urn:zotero:item:KEY1")]);
        tags.add_suggestion("urn:cms:book:aaa", "rust");
        assert!(tags.approve("urn:cms:book:aaa", "rust"));
        // Stored on the identity, not on the ordinal-derived hash — that is the whole fix.
        assert_eq!(entries(&tags.approved)[0].iri, "urn:zotero:item:KEY1");
        assert!(tags.approved_turtle().contains("<urn:cms:book:aaa>"));

        // Re-export: the same book is now `#item_733` → `…:bbb`, and `…:aaa` is some other book.
        // The link pass rewrites the overlay against the new graph.
        std::fs::write(
            &tags.zotero_links,
            format!("<urn:cms:book:bbb> <{CMS_ZOTERO_ITEM}> <urn:zotero:item:KEY1> .\n"),
        )
        .unwrap();

        let ttl = tags.approved_turtle();
        assert!(
            ttl.contains("<urn:cms:book:bbb>"),
            "follows the book: {ttl}"
        );
        assert!(
            !ttl.contains("<urn:cms:book:aaa>"),
            "the tag must NOT land on whatever is at the old ordinal now: {ttl}"
        );
    }

    #[test]
    fn a_book_the_export_no_longer_holds_emits_nothing_but_keeps_its_tags() {
        let (_d, tags) = linked(&[("urn:cms:book:aaa", "urn:zotero:item:KEY1")]);
        tags.approve("urn:cms:book:aaa", "rust");
        // The book leaves the library: the link overlay no longer maps its identity to any card.
        std::fs::write(&tags.zotero_links, "").unwrap();
        assert_eq!(
            tags.approved_turtle(),
            "",
            "a bare identity in the graph would inflate urn:cms:tags' counts"
        );
        // The decision itself is untouched, ready for the book's return.
        assert_eq!(entries(&tags.approved).len(), 1);
    }

    #[test]
    fn duplicate_books_sharing_one_zotero_item_both_carry_the_tag() {
        let (_d, tags) = linked(&[
            ("urn:cms:book:aaa", "urn:zotero:item:KEY1"),
            ("urn:cms:book:bbb", "urn:zotero:item:KEY1"),
        ]);
        tags.approve("urn:cms:book:aaa", "rust");
        let ttl = tags.approved_turtle();
        assert!(ttl.contains("<urn:cms:book:aaa>"), "{ttl}");
        assert!(
            ttl.contains("<urn:cms:book:bbb>"),
            "the duplicate card is the same book — it shows the tag too: {ttl}"
        );
        assert_eq!(entries(&tags.approved).len(), 1, "stored once");
    }

    #[test]
    fn resources_with_no_durable_identity_are_untouched() {
        // A bookmark (never at risk — its hash is of the URL) and a book the API had no match for.
        let (_d, tags) = linked(&[("urn:cms:book:matched", "urn:zotero:item:KEY1")]);
        tags.approve("urn:cms:bookmark:xyz", "reading");
        tags.approve("urn:cms:book:unmatched", "reading");
        for iri in ["urn:cms:bookmark:xyz", "urn:cms:book:unmatched"] {
            assert!(entries(&tags.approved).iter().any(|e| e.iri == iri));
            assert!(tags.approved_turtle().contains(&format!("<{iri}>")));
        }
    }

    #[test]
    fn migrate_moves_old_keys_preserves_every_decision_and_re_runs_clean() {
        let (_d, tags) = linked(&[
            ("urn:cms:book:aaa", "urn:zotero:item:KEY1"),
            ("urn:cms:book:bbb", "urn:zotero:item:KEY2"),
        ]);
        // A store as it exists today: everything keyed on the graph IRI, including a bookmark and
        // an unmatched book that must survive the rekey untouched.
        for (path, pred) in [
            (&tags.approved, DC_SUBJECT),
            (&tags.suggestions, CMS_SUGGESTED),
            (&tags.dismissed, CMS_DISMISSED),
        ] {
            let es: Vec<Entry> = [
                ("urn:cms:book:aaa", "rust"),
                ("urn:cms:book:bbb", "wasm"),
                ("urn:cms:book:orphan", "systems"),
                ("urn:cms:bookmark:xyz", "reading"),
            ]
            .iter()
            .map(|(iri, tag)| Entry {
                iri: iri.to_string(),
                tag: tag.to_string(),
            })
            .collect();
            write_entries(path, &es, pred);
        }
        let served_before: Vec<String> = [
            tags.approved_turtle(),
            tags.suggestions_turtle(),
            tags.dismissed_turtle(),
        ]
        .to_vec();

        let report = tags.migrate_to_durable_keys().expect("nothing is lost");
        assert_eq!(report.moved, 6, "two books × three files: {report:?}");
        assert_eq!(report.already, 0);
        assert_eq!(report.unmatched, 6, "the orphan + the bookmark, ×3");
        assert_eq!(report.at_risk, 3, "only the orphan BOOK is still fragile");
        assert_eq!(report.rewritten.len(), 3);
        assert_eq!(report.backups.len(), 3);

        // The decisions are intact: what the graph is served is byte-identical to before.
        assert_eq!(
            served_before,
            [
                tags.approved_turtle(),
                tags.suggestions_turtle(),
                tags.dismissed_turtle()
            ]
            .to_vec(),
            "the migration is invisible to the room"
        );
        // ...and each backup holds the pre-migration file.
        for b in &report.backups {
            assert!(std::fs::read_to_string(b)
                .unwrap()
                .contains("urn:cms:book:aaa"));
        }
        // The approved count — the number Brian cares about — is unchanged.
        assert_eq!(entries(&tags.approved).len(), 4);

        // Idempotent: a second run moves nothing, rewrites nothing, and clobbers no backup.
        let again = tags.migrate_to_durable_keys().expect("still nothing lost");
        assert_eq!(again.moved, 0);
        assert_eq!(again.already, 6);
        assert_eq!(again.rewritten, Vec::<PathBuf>::new());
        assert_eq!(again.backups, Vec::<PathBuf>::new());
    }

    #[test]
    fn migrate_with_no_link_overlay_is_a_no_op() {
        // A machine that has never run the Zotero pass: no map, so nothing is durable and nothing
        // is touched — the overlays keep working exactly as before.
        let dir = tempfile::tempdir().unwrap();
        let tags = TagPaths::in_dir(dir.path());
        tags.approve("urn:cms:book:aaa", "rust");
        let report = tags.migrate_to_durable_keys().unwrap();
        assert_eq!(report.moved, 0);
        assert_eq!(report.at_risk, 1);
        assert!(report.rewritten.is_empty() && report.backups.is_empty());
        assert!(tags.approved_turtle().contains("<urn:cms:book:aaa>"));
    }

    #[test]
    fn a_promote_still_works_on_a_suggestion_written_before_the_migration() {
        // The hazard window: suggestions on disk from the old code, a `+` clicked before anything
        // rekeys them. The pending chip must clear, not linger forever.
        let (_d, tags) = linked(&[("urn:cms:book:aaa", "urn:zotero:item:KEY1")]);
        write_entries(
            &tags.suggestions,
            &[Entry {
                iri: "urn:cms:book:aaa".into(),
                tag: "rust".into(),
            }],
            CMS_SUGGESTED,
        );
        assert!(
            tags.approve("urn:cms:book:aaa", "rust"),
            "the stale-keyed suggestion is found"
        );
        assert!(entries(&tags.suggestions).is_empty(), "the chip is gone");
        assert_eq!(entries(&tags.approved)[0].iri, "urn:zotero:item:KEY1");
    }
}
