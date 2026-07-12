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

/// `$HOME/.ikigai/{file}` — the ikigai-owned state dir (created if missing), kept out of synced
/// content. `CMS_TAG_APPROVED` / `CMS_TAG_SUGGESTIONS` override the two files.
fn ikigai_path(env: &str, file: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env) {
        return PathBuf::from(p);
    }
    let dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".ikigai");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(file)
}

pub fn approved_path() -> PathBuf {
    ikigai_path("CMS_TAG_APPROVED", "cms-tags-approved.ttl")
}

pub fn suggestions_path() -> PathBuf {
    ikigai_path("CMS_TAG_SUGGESTIONS", "cms-tag-suggestions.ttl")
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

/// Add a suggestion `<iri> cms:suggestedTag "tag"` (idempotent — no duplicate).
pub fn add_suggestion(iri: &str, tag: &str) {
    let path = suggestions_path();
    let mut es = entries(&path);
    let e = Entry {
        iri: iri.to_string(),
        tag: tag.to_string(),
    };
    if !es.contains(&e) {
        es.push(e);
        write_entries(&path, &es, CMS_SUGGESTED);
    }
}

/// Promote a suggestion to a real tag: drop it from suggestions, add `<iri> dc:subject "tag"` to
/// the approved overlay (idempotent). Returns false if the suggestion wasn't present.
pub fn approve(iri: &str, tag: &str) -> bool {
    let target = Entry {
        iri: iri.to_string(),
        tag: tag.to_string(),
    };
    let sug_path = suggestions_path();
    let mut sug = entries(&sug_path);
    let had = sug.contains(&target);
    sug.retain(|e| *e != target);
    write_entries(&sug_path, &sug, CMS_SUGGESTED);

    let app_path = approved_path();
    let mut app = entries(&app_path);
    if !app.contains(&target) {
        app.push(target);
        write_entries(&app_path, &app, DC_SUBJECT);
    }
    had
}

/// Dismiss a suggestion: drop it from the suggestions overlay. Returns false if it wasn't present.
pub fn reject(iri: &str, tag: &str) -> bool {
    let target = Entry {
        iri: iri.to_string(),
        tag: tag.to_string(),
    };
    let path = suggestions_path();
    let mut sug = entries(&path);
    let had = sug.contains(&target);
    sug.retain(|e| *e != target);
    write_entries(&path, &sug, CMS_SUGGESTED);
    had
}

/// The suggestions overlay as a Turtle string (for the `urn:cms:graph:suggestions` resource).
pub fn suggestions_turtle() -> String {
    serialize(&entries(&suggestions_path()), CMS_SUGGESTED)
}

/// The approved overlay as a Turtle string (for the `urn:cms:graph:tags-approved` resource).
pub fn approved_turtle() -> String {
    serialize(&entries(&approved_path()), DC_SUBJECT)
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
        std::env::set_var("CMS_TAG_SUGGESTIONS", dir.path().join("s.ttl"));
        std::env::set_var("CMS_TAG_APPROVED", dir.path().join("a.ttl"));

        add_suggestion("urn:cms:book:1", "rust");
        add_suggestion("urn:cms:book:1", "wasm");
        add_suggestion("urn:cms:book:1", "rust"); // idempotent
        assert_eq!(entries(&suggestions_path()).len(), 2);

        // Promote one → it leaves suggestions and lands in approved as dc:subject.
        assert!(approve("urn:cms:book:1", "rust"));
        assert!(!entries(&suggestions_path()).iter().any(|e| e.tag == "rust"));
        assert!(
            approved_turtle().contains("#/subject> \"rust\"")
                || approved_turtle().contains("subject> \"rust\"")
        );
        assert!(entries(&approved_path())
            .iter()
            .any(|e| e.iri == "urn:cms:book:1" && e.tag == "rust"));

        // Dismiss the other → gone from suggestions, never in approved.
        assert!(reject("urn:cms:book:1", "wasm"));
        assert!(entries(&suggestions_path()).is_empty());
        assert!(!entries(&approved_path()).iter().any(|e| e.tag == "wasm"));
        // Re-reject is a no-op that reports "wasn't there".
        assert!(!reject("urn:cms:book:1", "wasm"));

        std::env::remove_var("CMS_TAG_SUGGESTIONS");
        std::env::remove_var("CMS_TAG_APPROVED");
    }
}
