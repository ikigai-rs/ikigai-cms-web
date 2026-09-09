//! ISBNs as the Zotero export actually hands them over.
//!
//! Two passes need this and neither can trust its input. The Zotero *API*'s `ISBN` field is free
//! text and often holds several. The export is worse: a book's subject IRI is
//! `urn:isbn:978-3-319-23093-1` — hyphenated — and **454 of the 1,362 ISBN-bearing books carry two
//! to five ISBNs joined by percent-encoded spaces**, because the whole field was pasted into the
//! IRI. `cms:isbn` is that field verbatim (see `BOOK_CONSTRUCT` in `native.rs`).
//!
//! [`keys`] is the normalizer: split the field, drop the punctuation, keep only what is actually an
//! ISBN, and canonicalize to ISBN-13 so an export listing the 10 and a record listing the 13 still
//! meet. Both consumers want the same thing for different reasons — `crate::zotero` compares
//! these keys against the API's, and [`crate::maintenance`] sends them to OpenLibrary as `bibkeys`.

/// Every ISBN in a raw field, as canonical ISBN-13 keys. Deduped, in field order.
///
/// Anything that isn't a plausible ISBN-10 or ISBN-13 is dropped, so `"n/a"` yields nothing rather
/// than a key that can only ever miss.
pub(crate) fn keys(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.replace("%20", " ").split([' ', ',', ';', '\t', '\n']) {
        let digits: String = part
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let key = match digits.len() {
            13 if digits.chars().all(|c| c.is_ascii_digit()) => Some(digits),
            10 => to_13(&digits),
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
///
/// The check digit is recomputed rather than carried, so a wrong one in the export is corrected on
/// the way through instead of being forwarded to whoever we ask.
fn to_13(s: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_survive_hyphens_multiples_and_the_isbn10_form() {
        // The export's shape: hyphenated, and several ISBNs jammed into one IRI with %20. Splitting
        // this field is the whole fix — as one string it is not an ISBN and matches nothing.
        let k = keys("978-1-119-00120-1%20978-1-119-00119-5%20978-1-119-00121-8");
        assert_eq!(
            k,
            vec!["9781119001201", "9781119001195", "9781119001218"],
            "every ISBN in the field, unhyphenated"
        );

        // An ISBN-10 (including the X check digit) canonicalizes to its 13 form, so an export
        // listing the 10 and a record listing the 13 meet.
        assert_eq!(keys("1-934356-00-X"), vec!["9781934356005"]);
        assert_eq!(keys("0596007124"), keys("978-0-596-00712-6"));

        // A field that holds no ISBN yields no key — better than a key that can only miss.
        assert!(keys("n/a").is_empty());
        assert!(keys("").is_empty());
    }

    #[test]
    fn a_bad_check_digit_is_corrected_not_forwarded() {
        // 0-596-00712-9 carries the wrong check digit (4 is correct). We recompute from the body,
        // so both spellings land on the one real ISBN-13 rather than propagating the typo.
        assert_eq!(keys("0-596-00712-9"), keys("0-596-00712-4"));
        assert_eq!(keys("0-596-00712-9"), vec!["9780596007126"]);
    }

    #[test]
    fn a_979_isbn_survives_untouched() {
        // 979 was allocated after ISBN-10 ran out, so it has no 10 form and nothing to convert.
        // It is already a 13 and passes straight through.
        assert_eq!(keys("979-8-88650-000-1"), vec!["9798886500001"]);
    }
}
