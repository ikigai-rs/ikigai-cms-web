//! `urn:cms:zotero-links` — give a book card a link to the *readable copy*.
//!
//! The books in the room come from a Zotero RDF export (`My Library.rdf`), which carries titles,
//! authors, tags and ISBNs but **no way to reach the file**: its attachment records hold only a
//! filename and a MIME type, and its subjects are either `urn:isbn:…` or `#item_N` — a per-export
//! ordinal that renumbers every time the library is re-exported. Nothing in the export is a stable,
//! linkable identity.
//!
//! The Zotero *API* has both. This pass sweeps it and writes an **overlay** — the same idea as the
//! tag overlays in [`crate::tagstore`]: a small Turtle file the books graph joins against, so the
//! regenerated source is never written to. Two triples per matched book:
//!
//! ```text
//! <urn:cms:book:{sha}> cms:zoteroItem <urn:zotero:item:{KEY}> .        # durable identity
//! <urn:cms:book:{sha}> cms:readerUrl  "https://www.zotero.org/…" .     # the readable copy
//! ```
//!
//! The identity is the bigger prize. `urn:zotero:item:{KEY}` survives re-export, so a books graph
//! keyed on it is diffable and joinable; `#item_N` is not. The reading link is a byproduct.
//!
//! Nothing here downloads or proxies a byte of the library: the overlay links to Zotero's own web
//! reader and the browser's existing session does the authenticating.

use async_trait::async_trait;
use ikigai_core::{
    ArgRef, ArgSpec, Description, Endpoint, Error, Invocation, Iri, ReprType, Representation,
    Request, Result, Verb,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The Zotero Web API root. Read-only, and every call is capability-gated by `ikigai-http`'s net
/// ACL like any other outbound request.
const API: &str = "https://api.zotero.org";

/// The API's hard ceiling on `limit`. Asking for more is silently clamped, so paging must assume
/// it.
const PAGE: usize = 100;

/// A stop on the paging loop: 2,125 attachments is ~22 pages, so 200 is two orders of headroom
/// while still bounding a paging bug to a finite number of requests.
const MAX_PAGES: usize = 200;

/// How long a swept page stays fresh. A day, not the link-checker's week: the library is edited
/// by hand and a new book should be reachable the next day. Within a day, a re-run of the pass
/// costs nothing — which is the point, since re-running after a failed match is the normal way to
/// work on it.
const DAY_SECS: u64 = 86_400;

const CMS_ZOTERO_ITEM: &str = "https://ikigai-rs.dev/ns/cms#zoteroItem";
const CMS_ZOTERO_ATTACHMENT: &str = "https://ikigai-rs.dev/ns/cms#zoteroAttachment";
const CMS_READER_URL: &str = "https://ikigai-rs.dev/ns/cms#readerUrl";

// ---- the pass ----------------------------------------------------------------------------------

/// `urn:cms:zotero-links` — sweep the Zotero API and rewrite the link overlay. Sourcing it returns
/// a one-line coverage summary; the overlay file is the real output.
pub struct ZoteroLinkPass {
    /// Where the overlay is written — the same file `urn:cms:graph:zotero-links` serves.
    pub path: PathBuf,
}

#[async_trait]
impl Endpoint for ZoteroLinkPass {
    async fn invoke(&self, inv: &Invocation<'_>) -> Result<Representation> {
        let max_age = inv
            .inline_str("max_age")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DAY_SECS);
        let key = api_key(inv).await?;

        // The key knows which library it opens, so the user id is never configured or hardcoded:
        // whoever's key is in the keystore is whose library gets linked.
        let (user_id, username) = identity(inv, &key).await?;
        eprintln!("[zotero-links] library: users/{user_id} ({username})");

        let api_books = sweep(
            inv,
            &key,
            max_age,
            &format!("{API}/users/{user_id}/items/top?itemType=book"),
        )
        .await?;
        let api_attachments = sweep(
            inv,
            &key,
            max_age,
            &format!("{API}/users/{user_id}/items?itemType=attachment"),
        )
        .await?;
        eprintln!(
            "[zotero-links] API: {} books, {} attachments",
            api_books.len(),
            api_attachments.len()
        );

        let books: Vec<ApiBook> = api_books.iter().filter_map(ApiBook::parse).collect();
        let readable = readable_by_parent(&api_attachments);
        let graph_books = list_graph_books(inv).await?;
        eprintln!("[zotero-links] graph: {} books to match", graph_books.len());

        let matches = match_books(&graph_books, &books);
        let mut rows: Vec<Row> = Vec::new();
        let (mut by_isbn, mut by_title, mut unmatched, mut no_readable) = (0, 0, 0, 0);
        for gb in &graph_books {
            let Some((api, how)) = matches.get(gb.id.as_str()) else {
                unmatched += 1;
                continue;
            };
            match how {
                How::Isbn => by_isbn += 1,
                How::Title => by_title += 1,
            }
            let att = readable.get(api.key.as_str());
            if att.is_none() {
                no_readable += 1;
            }
            rows.push(Row {
                book: gb.id.clone(),
                item: api.key.clone(),
                attachment: att.map(|a| a.key.clone()),
                reader_url: att.map(|a| a.alternate.clone()),
            });
        }

        write_overlay(&self.path, &rows);
        let line = format!(
            "zotero-links: {} of {} books matched ({by_isbn} by ISBN, {by_title} by title+creator), \
             {unmatched} unmatched; {} readable, {no_readable} matched with no readable attachment",
            by_isbn + by_title,
            graph_books.len(),
            rows.iter().filter(|r| r.reader_url.is_some()).count(),
        );
        Ok(Representation::new(
            ReprType::new("text/plain"),
            line.into_bytes(),
        ))
    }

    fn name(&self) -> &str {
        "cms-zotero-links"
    }

    fn describe(&self) -> Description {
        Description::new("urn:cms:zotero-links")
            .summary(
                "Sweep the Zotero API and rewrite the book link overlay: a durable \
                 urn:zotero:item:{KEY} identity per matched book, plus a web-reader URL for the \
                 books whose library copy is readable.",
            )
            .verb(Verb::Source)
            .input(
                ArgSpec::new("max_age")
                    .summary("seconds a swept API page stays fresh")
                    .optional()
                    .class("http://www.w3.org/2001/XMLSchema#integer")
                    .default_value(DAY_SECS.to_string()),
            )
    }
}

/// One overlay row: a graph book, the Zotero item it is, and the readable attachment if it has one.
struct Row {
    book: String,
    item: String,
    attachment: Option<String>,
    reader_url: Option<String>,
}

// ---- the API -----------------------------------------------------------------------------------

/// The API key, read as a resource (`urn:secret:zotero-api-key`) so `ikigai-secret`'s capability
/// gate actually runs. Reading it off a held `Arc<dyn Backend>` would skip that check entirely —
/// the gate lives in the endpoint, not the backend.
///
/// The value never appears in an error, a log line, or the summary: the only place it goes is the
/// `Zotero-API-Key` request header.
async fn api_key(inv: &Invocation<'_>) -> Result<String> {
    let repr = inv
        .issue(Request::new(
            Verb::Source,
            Iri::parse("urn:secret:zotero-api-key").expect("valid IRI"),
        ))
        .await
        .map_err(|e| {
            Error::Endpoint(format!(
                "no Zotero API key (urn:secret:zotero-api-key): {e}"
            ))
        })?;
    let key = String::from_utf8_lossy(&repr.bytes).trim().to_string();
    if key.is_empty() {
        return Err(Error::Endpoint(
            "urn:secret:zotero-api-key is empty".to_string(),
        ));
    }
    Ok(key)
}

/// Whose library the key opens: `/keys/current` reports the userID and username the key is bound
/// to. Doing it this way means the library id is never configuration that can drift out of sync
/// with the credential.
async fn identity(inv: &Invocation<'_>, key: &str) -> Result<(String, String)> {
    // Never cached: it is one call, and a stale answer here would point the whole sweep at the
    // wrong library.
    let json = get_json(inv, key, &format!("{API}/keys/current"), 0).await?;
    let user_id = json["userID"]
        .as_u64()
        .ok_or_else(|| Error::Endpoint("Zotero /keys/current returned no userID".to_string()))?;
    let username = json["username"].as_str().unwrap_or("").to_string();
    if !json["access"]["user"]["library"].as_bool().unwrap_or(false) {
        return Err(Error::Denied(
            "the Zotero API key has no personal-library access (needs `Allow library access`)"
                .to_string(),
        ));
    }
    Ok((user_id.to_string(), username))
}

/// GET a JSON resource from the Zotero API with the key in a header.
///
/// The key rides in `headers=`, never in the URL: `ikigai-http` puts the URL in its errors and its
/// golden thread, so a `?key=` query parameter would leak the credential into both.
async fn get_json(
    inv: &Invocation<'_>,
    key: &str,
    url: &str,
    max_age: u64,
) -> Result<serde_json::Value> {
    let mut request = Request::new(Verb::Source, Iri::parse("urn:httpGet").expect("valid IRI"))
        .with_arg("url", ArgRef::Inline(url.as_bytes().to_vec()))
        .with_arg(
            "headers",
            ArgRef::Inline(format!("Zotero-API-Key: {key}\nZotero-API-Version: 3").into_bytes()),
        );
    if max_age > 0 {
        request = request.with_arg("max_age", ArgRef::Inline(max_age.to_string().into_bytes()));
    }
    let repr = inv.issue(request).await?;
    serde_json::from_slice(&repr.bytes)
        .map_err(|e| Error::Endpoint(format!("Zotero API returned non-JSON for `{url}`: {e}")))
}

/// Page through a Zotero collection endpoint and return every item.
///
/// **Offsets, not the `Link: rel="next"` header** — not by preference. `urn:httpGet` returns a
/// representation (bytes + media type) and drops the response headers, so `Link`, `Total-Results`
/// and `Last-Modified-Version` are all unreachable from inside the kernel. `start`/`limit` is what
/// `rel="next"` encodes anyway, and the stop rule is self-evident: a page shorter than `limit` is
/// the last page.
///
/// Sorted by `dateAdded` ascending so the offsets are stable *during* a sweep: an item's dateAdded
/// never changes, so anything added mid-sweep lands at the end and cannot shift a page boundary
/// under us. (Sorting by title — the API default — would let one new book shift every later page
/// and silently drop a book from the sweep.)
async fn sweep(
    inv: &Invocation<'_>,
    key: &str,
    max_age: u64,
    base: &str,
) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    for page in 0..MAX_PAGES {
        let url = format!(
            "{base}&format=json&sort=dateAdded&direction=asc&limit={PAGE}&start={}",
            page * PAGE
        );
        let json = get_json(inv, key, &url, max_age).await?;
        let Some(items) = json.as_array() else {
            return Err(Error::Endpoint(format!(
                "Zotero API returned a non-array page for `{base}`"
            )));
        };
        let n = items.len();
        out.extend(items.iter().cloned());
        if n < PAGE {
            return Ok(out);
        }
    }
    Err(Error::Endpoint(format!(
        "Zotero paging exceeded {MAX_PAGES} pages for `{base}` — refusing to sweep further"
    )))
}

/// A book as the API reports it.
struct ApiBook {
    key: String,
    title_key: String,
    surname: String,
    isbns: Vec<String>,
}

impl ApiBook {
    fn parse(v: &serde_json::Value) -> Option<Self> {
        let data = &v["data"];
        let title = data["title"].as_str().unwrap_or("");
        if title.is_empty() {
            return None;
        }
        Some(Self {
            key: v["key"].as_str()?.to_string(),
            title_key: norm_title(title),
            surname: data["creators"]
                .as_array()
                .and_then(|cs| cs.iter().find_map(creator_surname))
                .unwrap_or_default(),
            isbns: isbn_keys(data["ISBN"].as_str().unwrap_or("")),
        })
    }
}

/// A creator's surname: Zotero stores either a two-field name or a single `name` string.
fn creator_surname(c: &serde_json::Value) -> Option<String> {
    if let Some(last) = c["lastName"].as_str() {
        if !last.trim().is_empty() {
            return Some(norm_word(last));
        }
    }
    let name = c["name"].as_str()?.trim();
    (!name.is_empty()).then(|| norm_word(name.rsplit(' ').next().unwrap_or(name)))
}

/// A readable attachment: the file the reader link points at.
struct Readable {
    key: String,
    alternate: String,
}

/// Group attachments by parent item, keeping only the *readable* one per book.
///
/// Two filters, and both are load-bearing:
///
/// - **`linkMode` must be an imported mode.** `imported_file` and `imported_url` have bytes in
///   Zotero storage; `linked_url` is a bookmark and `linked_file` is a path on some other machine.
///   Filtering on `contentType` alone is not enough and not theoretical — the API serves
///   `linked_url` attachments carrying `contentType: application/pdf`, and every one of those would
///   mint a reader link to a file that does not exist. Do not "simplify" this away.
/// - **EPUB before PDF.** Evidence, not taste: the library holds 1,364 `application/epub+zip`
///   against 753 `application/pdf`, so EPUB is the format this collection is actually in. Anything
///   else (html, webarchive, mp4) is not a readable copy of the book and gets no link at all.
///
/// Ties inside a rank break on the lowest key, so the overlay is byte-stable across runs and a
/// diff of it means something changed.
fn readable_by_parent(items: &[serde_json::Value]) -> HashMap<String, Readable> {
    let mut best: HashMap<String, (u8, Readable)> = HashMap::new();
    for v in items {
        let data = &v["data"];
        let Some(parent) = data["parentItem"].as_str() else {
            continue;
        };
        let link_mode = data["linkMode"].as_str().unwrap_or("");
        if link_mode != "imported_file" && link_mode != "imported_url" {
            continue;
        }
        let Some(rank) = readable_rank(data["contentType"].as_str().unwrap_or("")) else {
            continue;
        };
        let (Some(key), Some(alternate)) =
            (v["key"].as_str(), v["links"]["alternate"]["href"].as_str())
        else {
            continue;
        };
        let candidate = Readable {
            key: key.to_string(),
            alternate: alternate.to_string(),
        };
        match best.get(parent) {
            Some((r, existing)) if (*r, existing.key.as_str()) <= (rank, key) => {}
            _ => {
                best.insert(parent.to_string(), (rank, candidate));
            }
        }
    }
    best.into_iter().map(|(k, (_, r))| (k, r)).collect()
}

fn readable_rank(content_type: &str) -> Option<u8> {
    match content_type {
        "application/epub+zip" => Some(0),
        "application/pdf" => Some(1),
        _ => None,
    }
}

// ---- matching ----------------------------------------------------------------------------------

/// A book as the graph has it (from the RDF export, normalized by `BOOK_CONSTRUCT`).
pub struct GraphBook {
    pub id: String,
    pub title: String,
    pub isbn: String,
    pub author: String,
}

/// How a graph book was matched to an API item — reported, because the two have very different
/// confidence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum How {
    Isbn,
    Title,
}

/// Match graph books to API books: ISBN first, then title+creator surname.
///
/// The title path is deliberately timid. A wrong link goes *into Brian's own library* and opens
/// the wrong book, which is worse than no link at all, so a title match is only taken when the
/// normalized key is unique on **both** sides — one graph book, one API book. Anything ambiguous
/// stays unmatched and shows up in the report as such.
fn match_books<'a>(graph: &[GraphBook], api: &'a [ApiBook]) -> HashMap<String, (&'a ApiBook, How)> {
    let mut by_isbn: HashMap<&str, Vec<&ApiBook>> = HashMap::new();
    let mut by_pair: HashMap<(&str, &str), Vec<&ApiBook>> = HashMap::new();
    let mut by_title: HashMap<&str, Vec<&ApiBook>> = HashMap::new();
    for b in api {
        for i in &b.isbns {
            by_isbn.entry(i.as_str()).or_default().push(b);
        }
        by_pair
            .entry((b.title_key.as_str(), b.surname.as_str()))
            .or_default()
            .push(b);
        by_title.entry(b.title_key.as_str()).or_default().push(b);
    }
    // Graph-side title frequency: the uniqueness test has to hold on both sides, or two copies of
    // the same title in the export would both claim the one API item.
    let mut graph_titles: HashMap<String, usize> = HashMap::new();
    for g in graph {
        *graph_titles.entry(norm_title(&g.title)).or_default() += 1;
    }

    let mut out = HashMap::new();
    for g in graph {
        let title_key = norm_title(&g.title);
        let surname = graph_surname(&g.author);
        // An ISBN is an identifier: if it matches, that IS the book, even if the titles differ
        // (editions retitle). Multiple ISBNs on one export subject are all tried.
        let hit = isbn_keys(&g.isbn)
            .iter()
            .find_map(|i| match by_isbn.get(i.as_str()) {
                Some(v) if v.len() == 1 => Some((v[0], How::Isbn)),
                _ => None,
            })
            .or_else(
                || match by_pair.get(&(title_key.as_str(), surname.as_str())) {
                    Some(v) if v.len() == 1 && !surname.is_empty() => Some((v[0], How::Title)),
                    _ => None,
                },
            )
            .or_else(|| match by_title.get(title_key.as_str()) {
                Some(v) if v.len() == 1 && graph_titles.get(&title_key) == Some(&1) => {
                    Some((v[0], How::Title))
                }
                _ => None,
            });
        if let Some(hit) = hit {
            out.insert(g.id.clone(), hit);
        }
    }
    out
}

/// The surname out of the graph's `"Surname, Given"` creator literal.
fn graph_surname(author: &str) -> String {
    norm_word(author.split(',').next().unwrap_or(author))
}

/// A comparison key for a title: lowercase, punctuation to spaces, whitespace collapsed. Deliberately
/// not clever — no subtitle stripping, no article dropping — because every extra normalization step
/// makes two *different* books collide, and a collision here mints a wrong link.
fn norm_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !space {
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// A comparison key for a single word (a surname): letters and digits, lowercased.
fn norm_word(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Every ISBN in a raw field, as canonical ISBN-13 keys.
///
/// Both sides need this and for different reasons. The API's `ISBN` field is free text and often
/// holds several. The export is worse: its subject IRIs are hyphenated (`urn:isbn:978-3-319-23093-1`)
/// and **463 of them carry two to five ISBNs joined by percent-encoded spaces**, because the whole
/// field was pasted into the IRI. Comparing those raw strings matches almost nothing.
///
/// ISBN-10s are converted to their ISBN-13 form so an export listing the 10 and an API record
/// listing the 13 still meet.
fn isbn_keys(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.replace("%20", " ").split([' ', ',', ';', '\t', '\n']) {
        let digits: String = part
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let key = match digits.len() {
            13 if digits.chars().all(|c| c.is_ascii_digit()) => Some(digits),
            10 => isbn10_to_13(&digits),
            _ => None,
        };
        if let Some(k) = key {
            if !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

/// ISBN-10 → ISBN-13: prefix `978` and recompute the check digit. `None` if it isn't a plausible
/// ISBN-10 (nine digits plus a digit-or-X check).
fn isbn10_to_13(s: &str) -> Option<String> {
    let body: &str = s.get(..9)?;
    if !body.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let check = s.chars().nth(9)?;
    if !check.is_ascii_digit() && check != 'X' {
        return None;
    }
    let twelve = format!("978{body}");
    let sum: u32 = twelve
        .chars()
        .enumerate()
        .map(|(i, c)| c.to_digit(10).unwrap_or(0) * if i % 2 == 0 { 1 } else { 3 })
        .sum();
    Some(format!("{twelve}{}", (10 - sum % 10) % 10))
}

// ---- the graph side ----------------------------------------------------------------------------

/// Every book in the graph, with what matching needs. Ordered so a run is reproducible.
async fn list_graph_books(inv: &Invocation<'_>) -> Result<Vec<GraphBook>> {
    let query = "PREFIX dc: <http://purl.org/dc/elements/1.1/> \
         PREFIX cms: <https://ikigai-rs.dev/ns/cms#> \
         SELECT ?id ?title ?isbn ?author WHERE { \
           ?id a cms:Book ; dc:title ?title . \
           OPTIONAL { ?id cms:isbn ?isbn } OPTIONAL { ?id dc:creator ?author } } ORDER BY ?id";
    let request = Request::new(
        Verb::Source,
        Iri::parse("urn:sparql:select").expect("valid IRI"),
    )
    .with_arg("query", ArgRef::Inline(query.as_bytes().to_vec()))
    .with_arg("graph", ArgRef::Inline(b"urn:cms:graph".to_vec()));
    let repr = inv.issue(request).await?;
    let json: serde_json::Value =
        serde_json::from_slice(&repr.bytes).unwrap_or(serde_json::Value::Null);
    let field = |r: &serde_json::Value, k: &str| r[k]["value"].as_str().unwrap_or("").to_string();
    let mut out: Vec<GraphBook> = Vec::new();
    if let Some(rows) = json["results"]["bindings"].as_array() {
        for r in rows {
            let (Some(id), Some(title)) = (r["id"]["value"].as_str(), r["title"]["value"].as_str())
            else {
                continue;
            };
            // A book with several authors comes back as one row per author; the first is the one
            // matching uses, and the rest add nothing.
            if out.last().is_some_and(|p| p.id == id) {
                continue;
            }
            out.push(GraphBook {
                id: id.to_string(),
                title: title.to_string(),
                isbn: field(r, "isbn"),
                author: field(r, "author"),
            });
        }
    }
    Ok(out)
}

/// Serialize the overlay: one canonical triple per line, full IRIs, sorted — the same
/// line-parseable Turtle discipline the tag overlays use, so a diff between runs is readable.
fn overlay_turtle(rows: &[Row]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for r in rows {
        lines.push(format!(
            "<{}> <{CMS_ZOTERO_ITEM}> <urn:zotero:item:{}> .",
            r.book, r.item
        ));
        if let Some(att) = &r.attachment {
            lines.push(format!(
                "<{}> <{CMS_ZOTERO_ATTACHMENT}> <urn:zotero:item:{att}> .",
                r.book
            ));
        }
        if let Some(url) = &r.reader_url {
            lines.push(format!(
                "<{}> <{CMS_READER_URL}> \"{}\" .",
                r.book,
                escape(url)
            ));
        }
    }
    lines.sort();
    lines.dedup();
    let mut s = lines.join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

fn write_overlay(path: &Path, rows: &[Row]) {
    let _ = std::fs::write(path, overlay_turtle(rows));
}

/// Turtle string-literal escaping (a URL can legally contain a quote or a backslash).
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isbns_survive_hyphens_multiples_and_the_isbn10_form() {
        // The export's shape: hyphenated, and several ISBNs jammed into one IRI with %20.
        let keys = isbn_keys("978-1-119-00120-1%20978-1-119-00119-5%20978-1-119-00121-8");
        assert_eq!(
            keys,
            vec![
                "9781119001201".to_string(),
                "9781119001195".to_string(),
                "9781119001218".to_string()
            ]
        );
        // An ISBN-10 (including the X check digit) canonicalizes to its 13 form, so an export
        // listing the 10 meets an API record listing the 13.
        assert_eq!(
            isbn_keys("1-934356-00-X"),
            vec!["9781934356005".to_string()]
        );
        assert_eq!(isbn_keys("0596007124"), isbn_keys("978-0-596-00712-6"));
        // Junk yields nothing rather than a bogus key.
        assert!(isbn_keys("n/a").is_empty());
        assert!(isbn_keys("").is_empty());
    }

    #[test]
    fn a_linked_url_pdf_is_not_readable() {
        // The trap this filter exists for: a bookmark that advertises a PDF content type. Linking
        // it would open a reader on a file Zotero does not have.
        let items = vec![
            serde_json::json!({
                "key": "AAAAAAAA",
                "links": {"alternate": {"href": "https://www.zotero.org/u/items/AAAAAAAA"}},
                "data": {"parentItem": "BOOK1", "linkMode": "linked_url",
                         "contentType": "application/pdf"}
            }),
            serde_json::json!({
                "key": "BBBBBBBB",
                "links": {"alternate": {"href": "https://www.zotero.org/u/items/BBBBBBBB"}},
                "data": {"parentItem": "BOOK2", "linkMode": "imported_url",
                         "contentType": "text/html"}
            }),
        ];
        assert!(readable_by_parent(&items).is_empty());
    }

    #[test]
    fn epub_wins_over_pdf_and_ties_are_stable() {
        let att = |key: &str, ct: &str| {
            serde_json::json!({
                "key": key,
                "links": {"alternate": {"href": format!("https://www.zotero.org/u/items/{key}")}},
                "data": {"parentItem": "BOOK1", "linkMode": "imported_file", "contentType": ct}
            })
        };
        // PDF first in input order, EPUB second — the EPUB still wins.
        let best = readable_by_parent(&[
            att("PPPPPPPP", "application/pdf"),
            att("EEEEEEEE", "application/epub+zip"),
        ]);
        assert_eq!(best["BOOK1"].key, "EEEEEEEE");
        // Two EPUBs: the lowest key wins whichever order they arrive in, so the overlay is stable.
        let a = readable_by_parent(&[
            att("ZZZZZZZZ", "application/epub+zip"),
            att("AAAAAAAA", "application/epub+zip"),
        ]);
        let b = readable_by_parent(&[
            att("AAAAAAAA", "application/epub+zip"),
            att("ZZZZZZZZ", "application/epub+zip"),
        ]);
        assert_eq!(a["BOOK1"].key, "AAAAAAAA");
        assert_eq!(b["BOOK1"].key, "AAAAAAAA");
    }

    fn api_book(key: &str, title: &str, surname: &str, isbn: &str) -> ApiBook {
        ApiBook {
            key: key.to_string(),
            title_key: norm_title(title),
            surname: norm_word(surname),
            isbns: isbn_keys(isbn),
        }
    }

    fn graph_book(id: &str, title: &str, author: &str, isbn: &str) -> GraphBook {
        GraphBook {
            id: id.to_string(),
            title: title.to_string(),
            isbn: isbn.to_string(),
            author: author.to_string(),
        }
    }

    #[test]
    fn isbn_beats_a_retitled_edition() {
        let api = vec![api_book(
            "KEY1",
            "Programming Rust, 2nd Edition",
            "Blandy",
            "978-1-4920-5259-3",
        )];
        // Same book, different title text and an ISBN-10 in the export — the ISBN still carries it.
        let graph = vec![graph_book(
            "urn:cms:book:1",
            "Programming Rust",
            "Blandy, Jim",
            "1492052590",
        )];
        let m = match_books(&graph, &api);
        assert_eq!(m["urn:cms:book:1"].0.key, "KEY1");
        assert_eq!(m["urn:cms:book:1"].1, How::Isbn);
    }

    #[test]
    fn title_and_creator_match_when_there_is_no_isbn() {
        let api = vec![api_book(
            "KEY1",
            "The Timeless Way of Building",
            "Alexander",
            "",
        )];
        let graph = vec![graph_book(
            "urn:cms:book:1",
            "The Timeless Way of Building",
            "Alexander, Christopher",
            "",
        )];
        let m = match_books(&graph, &api);
        assert_eq!(m["urn:cms:book:1"].1, How::Title);
    }

    #[test]
    fn an_ambiguous_title_is_left_unmatched() {
        // Two API books share a title and neither has an ISBN or a distinguishing surname.
        let api = vec![
            api_book("KEY1", "Foundation", "", ""),
            api_book("KEY2", "Foundation", "", ""),
        ];
        let graph = vec![graph_book("urn:cms:book:1", "Foundation", "", "")];
        assert!(
            match_books(&graph, &api).is_empty(),
            "a guess into a personal library is worse than no link"
        );
        // Same title twice in the GRAPH is equally ambiguous, even against a unique API book.
        let api = vec![api_book("KEY1", "Foundation", "", "")];
        let graph = vec![
            graph_book("urn:cms:book:1", "Foundation", "", ""),
            graph_book("urn:cms:book:2", "Foundation", "", ""),
        ];
        assert!(match_books(&graph, &api).is_empty());
    }

    #[test]
    fn overlay_is_sorted_canonical_turtle() {
        let rows = vec![
            Row {
                book: "urn:cms:book:b".into(),
                item: "ITEM2".into(),
                attachment: None,
                reader_url: None,
            },
            Row {
                book: "urn:cms:book:a".into(),
                item: "ITEM1".into(),
                attachment: Some("ATT1".into()),
                reader_url: Some("https://www.zotero.org/u/items/ATT1".into()),
            },
        ];
        let ttl = overlay_turtle(&rows);
        // Identity for both; a reader URL only for the one with a readable attachment.
        assert!(ttl.contains(
            "<urn:cms:book:b> <https://ikigai-rs.dev/ns/cms#zoteroItem> <urn:zotero:item:ITEM2> ."
        ));
        assert!(ttl.contains("<urn:cms:book:a> <https://ikigai-rs.dev/ns/cms#readerUrl> \"https://www.zotero.org/u/items/ATT1\" ."));
        assert_eq!(ttl.matches("readerUrl").count(), 1);
        // Sorted → a diff between runs means something actually changed.
        let lines: Vec<&str> = ttl.lines().collect();
        let mut sorted = lines.clone();
        sorted.sort_unstable();
        assert_eq!(lines, sorted);
    }
}
