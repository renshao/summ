//! Query-string and percent-encoding helpers.
//!
//! Written here rather than taken from `serde_urlencoded` or
//! `percent-encoding` for two reasons. First, several `/v2/` parameters repeat
//! (`?tag=a&tag=b` on end-7b), which a `Deserialize`-into-struct API loses.
//! Second, the `Link` header's query has to be byte-compatible with what Go's
//! `url.Values.Encode` produces, because that is what every client has been
//! parsing from the reference implementation for a decade - and the crates'
//! defaults differ from it in the reserved set.

/// Split a raw query string into pairs, preserving order and repeats.
///
/// A key with no `=` yields an empty value, matching Go's `url.ParseQuery`.
/// Undecodable percent escapes are left literal rather than failing the whole
/// request: the value will then simply not match anything valid.
pub fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(part), String::new()),
        })
        .collect()
}

/// First value for `key`, or `None`.
pub fn first<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Every value for `key`, in order.
pub fn all<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .collect()
}

/// Percent-decoding for a query value: `+` means space.
pub fn form_decode(s: &str) -> String {
    percent_decode(s, true)
}

/// Percent-decoding for a path segment: `+` is literal.
///
/// Splitting the path *before* decoding, and decoding each segment
/// separately, is what stops an encoded `%2F` inside a tag from being mistaken
/// for a path separator.
pub fn path_decode(s: &str) -> String {
    percent_decode(s, false)
}

fn percent_decode(s: &str, plus_is_space: bool) -> String {
    if !(s.contains('%') || (plus_is_space && s.contains('+'))) {
        return s.to_owned();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match hex_pair(b[i + 1], b[i + 2]) {
                Some(byte) => {
                    out.push(byte);
                    i += 3;
                }
                None => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(hi: u8, lo: u8) -> Option<u8> {
    Some(hex_digit(hi)? << 4 | hex_digit(lo)?)
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Escape a query *value* the way Go's `url.QueryEscape` does: unreserved
/// characters pass through, a space becomes `+`, everything else becomes
/// `%XX` with uppercase hex.
///
/// This is what makes a paginated repository name come back as
/// `last=conformance%2Frepo1`, byte-identical to the reference implementation.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_preserve_order_and_repeats() {
        let p = pairs("tag=a&tag=b&n=3");
        assert_eq!(all(&p, "tag"), vec!["a", "b"]);
        assert_eq!(first(&p, "n"), Some("3"));
        assert_eq!(first(&p, "missing"), None);
    }

    #[test]
    fn a_bare_key_has_an_empty_value() {
        let p = pairs("digest&n=1");
        assert_eq!(first(&p, "digest"), Some(""));
    }

    #[test]
    fn decoding_distinguishes_path_from_query() {
        assert_eq!(form_decode("a+b"), "a b");
        assert_eq!(path_decode("a+b"), "a+b");
        assert_eq!(path_decode("conformance%2Frepo1"), "conformance/repo1");
    }

    #[test]
    fn escaping_matches_go_query_escape() {
        assert_eq!(query_escape("conformance/repo1"), "conformance%2Frepo1");
        assert_eq!(query_escape("v1.0-rc_1~x"), "v1.0-rc_1~x");
        assert_eq!(query_escape("a b"), "a+b");
    }
}
