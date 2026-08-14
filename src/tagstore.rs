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

use std::path::{Path, PathBuf};

const DC_SUBJECT: &str = "http://purl.org/dc/elements/1.1/subject";
const CMS_SUGGESTED: &str = "https://ikigai-rs.dev/ns/cms#suggestedTag";
/// A tag the human dismissed for a resource — recorded so the tag-suggest pass never re-suggests it
/// (the negative half of the feedback loop). In `urn:cms:graph` so the pass's SPARQL can exclude a
/// fully-dismissed book from the candidate set; not rendered.
const CMS_DISMISSED: &str = "https://ikigai-rs.dev/ns/cms#dismissedTag";

/// The overlay files as explicit paths, threaded from the config into every consumer
/// (the graph endpoints, the tag-suggest pass) — no process-global state, so tests hand each
/// kernel its own tempdir store.
///
/// `zotero_links` is the odd one out: not a tag overlay but the same mechanism — a derived
/// Turtle sidecar the graph joins against, written by a pass, never by hand. It rides here so a
/// test's tempdir covers every overlay at once rather than half of them.
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
    /// Add a suggestion `<iri> cms:suggestedTag "tag"` (idempotent — no duplicate).
    pub fn add_suggestion(&self, iri: &str, tag: &str) {
        let mut es = entries(&self.suggestions);
        let e = Entry {
            iri: iri.to_string(),
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
        let target = Entry {
            iri: iri.to_string(),
            tag: tag.to_string(),
        };
        let mut sug = entries(&self.suggestions);
        let had = sug.contains(&target);
        sug.retain(|e| *e != target);
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
        let target = Entry {
            iri: iri.to_string(),
            tag: tag.to_string(),
        };
        let mut sug = entries(&self.suggestions);
        let had = sug.contains(&target);
        sug.retain(|e| *e != target);
        write_entries(&self.suggestions, &sug, CMS_SUGGESTED);
        self.dismiss(iri, tag);
        had
    }

    /// Record that `tag` was dismissed for `iri` (idempotent) — the negative feedback signal.
    pub fn dismiss(&self, iri: &str, tag: &str) {
        let mut es = entries(&self.dismissed);
        let e = Entry {
            iri: iri.to_string(),
            tag: tag.to_string(),
        };
        if !es.contains(&e) {
            es.push(e);
            write_entries(&self.dismissed, &es, CMS_DISMISSED);
        }
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

    /// The suggestions overlay as a Turtle string (for the `urn:cms:graph:suggestions` resource).
    pub fn suggestions_turtle(&self) -> String {
        serialize(&entries(&self.suggestions), CMS_SUGGESTED)
    }

    /// The approved overlay as a Turtle string (for the `urn:cms:graph:tags-approved` resource).
    pub fn approved_turtle(&self) -> String {
        serialize(&entries(&self.approved), DC_SUBJECT)
    }

    /// The dismissed overlay as a Turtle string (for the `urn:cms:graph:dismissed` resource).
    pub fn dismissed_turtle(&self) -> String {
        serialize(&entries(&self.dismissed), CMS_DISMISSED)
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
}
