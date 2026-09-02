//! `?n=` / `?last=` parsing and the `Link` header (spec §Listing Tags).

use axum::http::HeaderValue;

use crate::config::ServerConfig;
use crate::error::{ApiError, ErrorCode};
use crate::query;

/// A validated page request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageParams {
    /// How many results to return, already clamped to
    /// [`ServerConfig::max_page_size`].
    pub limit: usize,
    /// Exclusive cursor: results begin strictly after this value.
    ///
    /// The spec is explicit that `last` "MUST NOT be a numerical index, but
    /// rather it MUST be a proper tag", which is what lets the cursor be a
    /// plain seek into a byte-ordered key range with no server-side state.
    pub last: Option<String>,
    /// `?n=0` was sent explicitly.
    ///
    /// Tracked separately from `limit == 0` because the spec singles it out:
    /// "this endpoint MUST return an empty list, and MUST NOT include a `Link`
    /// header". Without the flag a `Link` could leak out of the ordinary
    /// "there is more" path.
    pub explicit_zero: bool,
}

/// Parse `?n=` and `?last=`.
///
/// An unparseable or negative `n` is a `400`. An `n` above the configured
/// ceiling is **clamped**, not rejected - see [`ServerConfig::max_page_size`].
pub fn parse(pairs: &[(String, String)], config: &ServerConfig) -> Result<PageParams, ApiError> {
    let raw_n = query::first(pairs, "n");
    let (limit, explicit_zero) = match raw_n {
        None => (config.default_page_size, false),
        Some(value) => {
            let n: u64 = value.parse().map_err(|_| {
                ApiError::new(ErrorCode::PaginationNumberInvalid).with_detail(format!("n={value}"))
            })?;
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            (n.min(config.max_page_size), n == 0)
        }
    };

    let last = query::first(pairs, "last")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    Ok(PageParams {
        limit,
        last,
        explicit_zero,
    })
}

/// Build `Link: <path?last=…&n=…>; rel="next"`.
///
/// Path-only URL in angle brackets, query rebuilt from scratch with only
/// `last` and `n`, keys in alphabetical order (`last` before `n`), values
/// escaped Go-style. That is byte-for-byte what the reference implementation
/// emits, and it is what clients have been parsing for a decade.
///
/// Callers emit this **only when a further page genuinely exists**. The
/// reference implementation cannot tell - its `moreEntries` flag only clears
/// when the storage driver returns EOF, which a full page never does - so it
/// sends `Link` on the final page and every client pays for one wasted round
/// trip. An ordered key range can peek one past the limit, so summ does.
pub fn link_next(path: &str, last: &str, n: usize) -> Option<HeaderValue> {
    let value = format!(
        "<{}?last={}&n={}>; rel=\"next\"",
        path,
        query::query_escape(last),
        n
    );
    HeaderValue::from_str(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    fn config() -> ServerConfig {
        ServerConfig {
            default_page_size: 100,
            max_page_size: 500,
            ..ServerConfig::default()
        }
    }

    #[test]
    fn absent_n_uses_the_default() {
        let p = parse(&query::pairs(""), &config()).expect("no params is valid");
        assert_eq!(p.limit, 100);
        assert!(!p.explicit_zero);
        assert_eq!(p.last, None);
    }

    #[test]
    fn n_is_clamped_not_rejected() {
        let p = parse(&query::pairs("n=100000"), &config()).expect("oversized n is clamped");
        assert_eq!(p.limit, 500);
    }

    #[test]
    fn zero_is_recorded_explicitly() {
        let p = parse(&query::pairs("n=0"), &config()).expect("n=0 is valid");
        assert_eq!(p.limit, 0);
        assert!(p.explicit_zero);
    }

    #[test]
    fn malformed_n_is_a_pagination_error() {
        for q in ["n=-1", "n=abc", "n=", "n=1.5", "n= 1"] {
            let err = parse(&query::pairs(q), &config()).expect_err("should reject");
            assert_eq!(err.code(), ErrorCode::PaginationNumberInvalid);
            assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn last_is_carried_through_and_empty_means_absent() {
        let p = parse(&query::pairs("last=v1"), &config()).expect("valid");
        assert_eq!(p.last.as_deref(), Some("v1"));
        let p = parse(&query::pairs("last="), &config()).expect("valid");
        assert_eq!(p.last, None);
    }

    #[test]
    fn link_matches_the_reference_format() {
        let link = link_next("/v2/demo/app/tags/list", "v1", 1).expect("valid header");
        assert_eq!(link, "</v2/demo/app/tags/list?last=v1&n=1>; rel=\"next\"");

        let link = link_next("/v2/_catalog", "conformance/repo1", 1).expect("valid header");
        assert_eq!(
            link,
            "</v2/_catalog?last=conformance%2Frepo1&n=1>; rel=\"next\""
        );
    }
}
