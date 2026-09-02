//! The spec error model.
//!
//! A `4XX` body MAY be any format, but if it is JSON the Distribution Spec
//! fixes its shape exactly (spec §Error Codes):
//!
//! ```json
//! {"errors":[{"code":"NAME_UNKNOWN","message":"…","detail":"…"}]}
//! ```
//!
//! `code` MUST contain only uppercase alphabetic characters and underscores,
//! which is why [`ErrorCode`] is an enum with a hand-written `as_str` rather
//! than a serde rename: the constraint is on the wire form, so the wire form is
//! written out literally and checked by a test.

use std::borrow::Cow;

use axum::http::{header, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// The spec's fourteen codes, plus one deviation.
///
/// The spec says "The `code` field MUST be one of the following", which taken
/// literally closes the set. summ stays inside it with one exception:
/// [`ErrorCode::PaginationNumberInvalid`]. See its own doc comment.
///
/// Two codes the reference implementation invents are deliberately *not* here:
///
/// - `RANGE_INVALID` for a `416` on an out-of-order upload chunk. summ uses
///   [`ErrorCode::BlobUploadInvalid`], which is in the spec's set and says the
///   same thing.
/// - `MANIFEST_UNVERIFIED` / `TAG_INVALID`, both covered by
///   [`ErrorCode::ManifestInvalid`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BlobUnknown,
    BlobUploadInvalid,
    BlobUploadUnknown,
    DigestInvalid,
    ManifestBlobUnknown,
    ManifestInvalid,
    ManifestUnknown,
    NameInvalid,
    NameUnknown,
    SizeInvalid,
    Unauthorized,
    Denied,
    Unsupported,
    /// Present for completeness of the taxonomy and **never emitted by summ**.
    ///
    /// containerd's `retryRequest` treats `429` as retryable and retries
    /// immediately, five times, without honouring `Retry-After`
    /// (`research/R5` §A9). A `429` therefore multiplies the load it was meant
    /// to shed. Escaping provider rate limits is half the reason this project
    /// exists; if summ ever needs to shed load it must do so at the connection
    /// or accept level, not with a status code.
    TooManyRequests,
    /// **Deviation from the spec's closed set**, and the only one.
    ///
    /// A malformed `?n=` has no home among the fourteen - `UNSUPPORTED` is a
    /// `405`, `MANIFEST_INVALID` is about manifest content - and every client
    /// in existence already sees this exact code from the reference
    /// implementation for this exact condition. Interoperability wins over
    /// literalism here; the conformance suite tests neither, so nothing is at
    /// risk but purity.
    PaginationNumberInvalid,
}

impl ErrorCode {
    /// The wire form. Uppercase alphabetic and underscores only, per spec.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BlobUnknown => "BLOB_UNKNOWN",
            ErrorCode::BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            ErrorCode::BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            ErrorCode::DigestInvalid => "DIGEST_INVALID",
            ErrorCode::ManifestBlobUnknown => "MANIFEST_BLOB_UNKNOWN",
            ErrorCode::ManifestInvalid => "MANIFEST_INVALID",
            ErrorCode::ManifestUnknown => "MANIFEST_UNKNOWN",
            ErrorCode::NameInvalid => "NAME_INVALID",
            ErrorCode::NameUnknown => "NAME_UNKNOWN",
            ErrorCode::SizeInvalid => "SIZE_INVALID",
            ErrorCode::Unauthorized => "UNAUTHORIZED",
            ErrorCode::Denied => "DENIED",
            ErrorCode::Unsupported => "UNSUPPORTED",
            ErrorCode::TooManyRequests => "TOOMANYREQUESTS",
            ErrorCode::PaginationNumberInvalid => "PAGINATION_NUMBER_INVALID",
        }
    }

    /// The status this code carries unless a handler overrides it.
    ///
    /// Overrides are rare and always spec-driven: `BLOB_UPLOAD_INVALID` is a
    /// `400` in general but a `416` for an out-of-order chunk, and
    /// `MANIFEST_INVALID` becomes `413` when the body exceeds the manifest
    /// limit.
    pub fn status(self) -> StatusCode {
        match self {
            ErrorCode::BlobUnknown
            | ErrorCode::BlobUploadUnknown
            | ErrorCode::ManifestUnknown
            | ErrorCode::NameUnknown => StatusCode::NOT_FOUND,
            ErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            ErrorCode::Denied => StatusCode::FORBIDDEN,
            ErrorCode::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::BAD_REQUEST,
        }
    }

    /// The message the reference implementation sends, where it has one.
    /// Clients do not parse these, but matching them keeps diffs against a
    /// `distribution` transcript readable.
    pub fn default_message(self) -> &'static str {
        match self {
            ErrorCode::BlobUnknown => "blob unknown to registry",
            ErrorCode::BlobUploadInvalid => "blob upload invalid",
            ErrorCode::BlobUploadUnknown => "blob upload unknown to registry",
            ErrorCode::DigestInvalid => "provided digest did not match uploaded content",
            ErrorCode::ManifestBlobUnknown => {
                "manifest references a manifest or blob unknown to registry"
            }
            ErrorCode::ManifestInvalid => "manifest invalid",
            ErrorCode::ManifestUnknown => "manifest unknown",
            ErrorCode::NameInvalid => "invalid repository name",
            ErrorCode::NameUnknown => "repository name not known to registry",
            ErrorCode::SizeInvalid => "provided length did not match content length",
            ErrorCode::Unauthorized => "authentication required",
            ErrorCode::Denied => "requested access to the resource is denied",
            ErrorCode::Unsupported => "the operation is unsupported",
            ErrorCode::TooManyRequests => "too many requests",
            ErrorCode::PaginationNumberInvalid => "invalid number of results requested",
        }
    }
}

#[derive(Serialize)]
struct WireError<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
struct WireBody<'a> {
    errors: [WireError<'a>; 1],
}

/// One error, ready to become a response.
///
/// `extra` exists for the handful of failures the spec attaches headers to -
/// notably a `416` on a blob `Range`, which MUST carry
/// `Content-Range: bytes */<len>`.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    message: Cow<'static, str>,
    detail: Option<serde_json::Value>,
    extra: Vec<(HeaderName, HeaderValue)>,
    /// Send the status with no JSON error body at all.
    ///
    /// Two cases need it. A `5XX` has no code in the spec's set and the spec
    /// constrains only `4XX` bodies, so inventing one (as the reference
    /// implementation does with `UNKNOWN`) would be a deviation bought for
    /// nothing. And a `416` on a blob `Range` is a pure RFC 9110 condition
    /// that none of the fourteen describes; it carries `Content-Range: bytes
    /// */<len>` and needs no body.
    bare: bool,
}

impl ApiError {
    pub fn new(code: ErrorCode) -> Self {
        ApiError {
            status: code.status(),
            code,
            message: Cow::Borrowed(code.default_message()),
            detail: None,
            extra: Vec::new(),
            bare: false,
        }
    }

    /// A `500`, logged here and reported without a body. See [`ApiError::bare`].
    pub fn internal(detail: impl AsRef<str>) -> Self {
        tracing::error!(detail = detail.as_ref(), "internal error");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: ErrorCode::Unsupported,
            message: Cow::Borrowed("internal error"),
            detail: None,
            extra: Vec::new(),
            bare: true,
        }
    }

    /// A status and headers with no body and no error code.
    ///
    /// For conditions the spec's fourteen codes do not describe and a client
    /// cannot act on differently for knowing - notably `416` on a blob `Range`,
    /// where `Content-Range: bytes */<len>` already says everything there is to
    /// say. The stored code is never serialised.
    pub fn status_only(status: StatusCode) -> Self {
        ApiError {
            status,
            code: ErrorCode::Unsupported,
            message: Cow::Borrowed(""),
            detail: None,
            extra: Vec::new(),
            bare: true,
        }
    }

    /// Override the status. Used only where the spec names a status that is not
    /// the code's usual one; see [`ErrorCode::status`].
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub fn with_message(mut self, message: impl Into<Cow<'static, str>>) -> Self {
        self.message = message.into();
        self
    }

    /// `detail` may be arbitrary JSON per the spec, but `specs-go`'s own struct
    /// types it as a string, so naive clients fail to unmarshal an object.
    /// Prefer a string; take a `Value` so a caller that has structure can send
    /// it deliberately.
    pub fn with_detail(mut self, detail: impl Into<serde_json::Value>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.extra.push((name, value));
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.bare {
            let mut response = Response::builder()
                .status(self.status)
                .header(header::CONTENT_LENGTH, 0)
                .body(axum::body::Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            for (name, value) in self.extra {
                response.headers_mut().insert(name, value);
            }
            return response;
        }

        let body = WireBody {
            errors: [WireError {
                code: self.code.as_str(),
                message: &self.message,
                detail: self.detail.as_ref(),
            }],
        };
        // Serialising a fixed struct of strings cannot fail; the fallback keeps
        // the no-unwrap rule without pretending the branch is reachable.
        let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
            br#"{"errors":[{"code":"UNSUPPORTED","message":"error encoding failed"}]}"#.to_vec()
        });

        let mut response = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(axum::body::Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());

        for (name, value) in self.extra {
            response.headers_mut().insert(name, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ErrorCode; 15] = [
        ErrorCode::BlobUnknown,
        ErrorCode::BlobUploadInvalid,
        ErrorCode::BlobUploadUnknown,
        ErrorCode::DigestInvalid,
        ErrorCode::ManifestBlobUnknown,
        ErrorCode::ManifestInvalid,
        ErrorCode::ManifestUnknown,
        ErrorCode::NameInvalid,
        ErrorCode::NameUnknown,
        ErrorCode::SizeInvalid,
        ErrorCode::Unauthorized,
        ErrorCode::Denied,
        ErrorCode::Unsupported,
        ErrorCode::TooManyRequests,
        ErrorCode::PaginationNumberInvalid,
    ];

    #[test]
    fn codes_are_uppercase_alphabetic_and_underscores_only() {
        for code in ALL {
            let s = code.as_str();
            assert!(!s.is_empty());
            assert!(
                s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_'),
                "{s} violates the spec's character set for `code`"
            );
        }
    }

    #[test]
    fn codes_are_unique() {
        let mut seen: Vec<&str> = ALL.iter().map(|c| c.as_str()).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate error code string");
    }

    #[test]
    fn statuses_match_the_spec_table() {
        assert_eq!(ErrorCode::BlobUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            ErrorCode::BlobUploadInvalid.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(ErrorCode::BlobUploadUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::DigestInvalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            ErrorCode::ManifestBlobUnknown.status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(ErrorCode::ManifestInvalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::ManifestUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::NameInvalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::NameUnknown.status(), StatusCode::NOT_FOUND);
        assert_eq!(ErrorCode::SizeInvalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(ErrorCode::Unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(ErrorCode::Denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            ErrorCode::Unsupported.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            ErrorCode::TooManyRequests.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorCode::PaginationNumberInvalid.status(),
            StatusCode::BAD_REQUEST
        );
    }
}
