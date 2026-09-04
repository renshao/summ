//! API-key authentication.
//!
//! Two keys, two levels: a read key admits `GET` and `HEAD`, a write key admits
//! everything and reads as well. Off by default - `summ serve` with no
//! arguments is an anonymous read-write registry, which is what makes it usable
//! in one command - and turned on wholesale with `--auth apikey`.
//!
//! # Why `Basic`, when the spec's example is `Bearer`
//!
//! The spec's authentication flow (§Authentication) is a `401` carrying
//! `WWW-Authenticate`, and it is deliberately open about the scheme. `Bearer`
//! is what the hosted registries advertise, but it means something specific to
//! a client: the challenge names a `realm` that is a *token server*, and docker
//! and containerd will go and `GET` it, exchange credentials for a scoped
//! bearer token, and only then retry. Advertising `Bearer` without standing up
//! that endpoint produces a client that fails in the token exchange rather than
//! one that authenticates.
//!
//! `Basic` is the challenge those same clients answer by sending the
//! credentials they already hold, which is exactly the model here: the key *is*
//! the credential, and there is nothing to exchange it for. So `docker login`,
//! `oras login` and `podman login` work with the key as the password, a browser
//! opening the UI gets its native prompt, and no token endpoint has to exist.
//!
//! `Bearer <key>` is *accepted* anyway, because it is what a human with `curl`
//! reaches for and costs one match arm. It is never advertised.
//!
//! # What is protected
//!
//! Everything: `/v2/`, `/api/v1/` and the UI, through one middleware in
//! [`crate::app::router`]. There is no exemption list, deliberately - an
//! exemption is a hole that has to be re-argued every time a route is added,
//! and the two candidates both fail on inspection. `GET /v2/` is the endpoint
//! whose `401` is how a client *discovers* it needs credentials, so exempting
//! it would break the flow it exists for. And the UI's assets are worth no less
//! than the API they read: serving the shell to an anonymous browser only moves
//! the prompt to the first `fetch`, where a native dialog on an XHR is a worse
//! experience than one on the document.

use std::fmt;

use axum::http::{header, HeaderMap, HeaderValue, Method};
use uuid::Uuid;

use crate::error::{ApiError, ErrorCode};

/// The challenge sent with every `401`.
///
/// With `Basic` the `realm` is a label rather than an address - the client
/// sends the credentials it already holds instead of going anywhere - so it
/// only has to identify the thing asking, and a browser shows it in the
/// password dialog. `charset` is defined for `Basic` alone (RFC 7617) and tells
/// a client to encode a non-ASCII credential as UTF-8 rather than guessing;
/// keys are hex, so it is for whatever a human types into that dialog.
const CHALLENGE: HeaderValue = HeaderValue::from_static("Basic realm=\"summ\", charset=\"UTF-8\"");

/// What a request needs in order to be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// `GET`, `HEAD`, `OPTIONS`.
    Read,
    /// Everything else - `PUT`, `POST`, `PATCH`, `DELETE`.
    Write,
}

impl Access {
    /// The level a method requires.
    ///
    /// Derived from the method alone, and that is a decision rather than a
    /// shortcut: every `/v2/` and `/api/v1/` operation that mutates the
    /// registry is a `PUT`, `POST`, `PATCH` or `DELETE`, and every one that
    /// does not is a `GET` or a `HEAD`. Deciding it from the parsed endpoint
    /// instead would put a second, longer table in the way of that fact and
    /// give a future endpoint somewhere to be forgotten.
    ///
    /// An unknown method lands on [`Access::Write`]: the safe side of a rule
    /// that decides whether a stranger may change the registry.
    pub fn of(method: &Method) -> Self {
        match *method {
            Method::GET | Method::HEAD | Method::OPTIONS => Access::Read,
            _ => Access::Write,
        }
    }
}

/// One API key.
///
/// The wrapper exists for its two `impl`s and nothing else: [`ApiKey::matches`]
/// so no caller can compare a key with `==`, and a [`fmt::Debug`] that redacts
/// so a key cannot reach a log through a `{:?}` on [`crate::ServerConfig`].
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        ApiKey(key.into())
    }

    /// A freshly generated key: 32 random bytes, lowercase hex.
    ///
    /// The randomness is two v4 UUIDs, which is the operating system's CSPRNG
    /// by way of `getrandom` - v4 fixes six bits for the version and variant,
    /// so this is 244 bits of entropy rather than 256. That is not a shortcut
    /// taken to avoid a dependency on quality; it avoids a *second* dependency
    /// on the same source, since `uuid` is already here for upload ids.
    ///
    /// Hex rather than base64 because a key is pasted into shells, `docker
    /// login -p`, YAML and URLs, and none of `+`, `/` or `=` survives all four
    /// without someone having to think about quoting.
    pub fn generate() -> Self {
        let mut hex = String::with_capacity(64);
        for _ in 0..2 {
            for byte in Uuid::new_v4().as_bytes() {
                use fmt::Write as _;
                // Writing to a `String` cannot fail; the `let _` keeps the
                // no-unwrap rule without pretending the branch is reachable.
                let _ = write!(hex, "{byte:02x}");
            }
        }
        ApiKey(hex)
    }

    /// The key itself. Only the startup banner should call this, and only for a
    /// key it just generated - see [`AuthPolicy`]'s doc on why a supplied key
    /// is never echoed.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Compare in time independent of how much of the key is right.
    ///
    /// A naive `==` on a `String` stops at the first differing byte, which
    /// makes the number of correct leading bytes measurable and a key
    /// recoverable one byte at a time. The fold below always reads every byte
    /// of the longer of the two and mixes the length difference in, so neither
    /// the position of the first error nor the length of the key is in the
    /// timing.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let got = presented.as_bytes();
        // A key is never empty - `AuthPolicy::new` rejects one - but an empty
        // *presented* value must not be allowed to match by exhausting the
        // loop without a difference.
        if expected.is_empty() || got.is_empty() {
            return false;
        }
        let mut diff = (expected.len() ^ got.len()) as u8;
        for i in 0..expected.len().max(got.len()) {
            // Index modulo, rather than zip, so the loop count does not depend
            // on the shorter input.
            diff |= expected[i % expected.len()] ^ got[i % got.len()];
        }
        diff == 0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// How the registry decides who may do what.
///
/// A supplied key is never printed back - not in the banner, not through
/// [`fmt::Debug`]. An operator who passed it already has it, and the only
/// thing echoing it achieves is putting a live credential in a log file, a
/// terminal scrollback and a CI transcript. A *generated* key has nowhere else
/// to be, so it is printed once and only then.
#[derive(Debug, Clone)]
pub enum AuthPolicy {
    /// No credentials required, for reading or for writing. The default.
    Anonymous,
    /// A read key and a write key. The write key also reads.
    ApiKey { read: ApiKey, write: ApiKey },
}

/// Why a request was not authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization` header at all. The client has to be told how to
    /// authenticate, so this is the challenge.
    Missing,
    /// An `Authorization` header that is not a scheme and a credential this
    /// registry understands, or a `Basic` value that is not base64 of
    /// `user:pass`.
    Malformed,
    /// A credential that matched no key.
    Invalid,
    /// The read key, on a request that writes. The credential is genuine, so
    /// this is a `403` and carries no challenge - re-sending the same key is
    /// not going to help, and a client told to try again will.
    Insufficient,
}

impl AuthPolicy {
    /// Build the policy for a mode and a pair of optional keys.
    ///
    /// Returns the policy and whether each key was generated, because the
    /// banner prints a generated key and must never print a supplied one.
    pub fn new(read: Option<String>, write: Option<String>) -> Result<(Self, Generated), String> {
        // An empty key would be indistinguishable from an absent one to
        // `matches`, which is a registry advertising authentication and
        // accepting `Authorization: Bearer `.
        for (label, key) in [("read", &read), ("write", &write)] {
            if key.as_ref().is_some_and(|k| k.trim().is_empty()) {
                return Err(format!("the {label} API key is empty"));
            }
        }
        let generated = Generated {
            read: read.is_none(),
            write: write.is_none(),
        };
        let policy = AuthPolicy::ApiKey {
            read: read.map(ApiKey::new).unwrap_or_else(ApiKey::generate),
            write: write.map(ApiKey::new).unwrap_or_else(ApiKey::generate),
        };
        Ok((policy, generated))
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, AuthPolicy::Anonymous)
    }

    /// Decide one request.
    pub fn authorize(&self, method: &Method, headers: &HeaderMap) -> Result<(), AuthError> {
        let AuthPolicy::ApiKey { read, write } = self else {
            return Ok(());
        };
        let Some(raw) = headers.get(header::AUTHORIZATION) else {
            return Err(AuthError::Missing);
        };
        let presented = raw
            .to_str()
            .ok()
            .and_then(credential)
            .ok_or(AuthError::Malformed)?;

        // Write is tried first, so a deployment that sets both keys to the same
        // value is a single key that can do everything rather than one that has
        // silently lost its write access.
        if write.matches(&presented) {
            return Ok(());
        }
        if read.matches(&presented) {
            return match Access::of(method) {
                Access::Read => Ok(()),
                Access::Write => Err(AuthError::Insufficient),
            };
        }
        Err(AuthError::Invalid)
    }
}

/// Which keys [`AuthPolicy::new`] had to invent.
#[derive(Debug, Clone, Copy)]
pub struct Generated {
    pub read: bool,
    pub write: bool,
}

impl AuthError {
    /// The response, with the challenge on the two cases a client can act on.
    pub fn into_api_error(self) -> ApiError {
        match self {
            AuthError::Missing => ApiError::new(ErrorCode::Unauthorized)
                .with_detail("send the API key as the password of an HTTP Basic credential")
                .with_header(header::WWW_AUTHENTICATE, CHALLENGE),
            AuthError::Malformed => ApiError::new(ErrorCode::Unauthorized)
                .with_message("malformed Authorization header")
                .with_detail("expected `Basic <base64>` or `Bearer <key>`")
                .with_header(header::WWW_AUTHENTICATE, CHALLENGE),
            AuthError::Invalid => ApiError::new(ErrorCode::Unauthorized)
                .with_message("invalid API key")
                .with_header(header::WWW_AUTHENTICATE, CHALLENGE),
            // No challenge: the credential was real, and a client that is
            // re-challenged retries with the same one.
            AuthError::Insufficient => ApiError::new(ErrorCode::Denied)
                .with_message("the read API key does not permit writes")
                .with_detail("use the write API key for push and delete"),
        }
    }
}

/// Pull the key out of an `Authorization` value.
///
/// `Basic` takes the **password**, ignoring the username: the key is the whole
/// credential, so `docker login -u anyone -p <key>` works and nobody has to
/// discover what username a registry wanted. A `Basic` value with an empty
/// password falls back to the username, which is what `curl -u <key>:` sends
/// and what a person pasting a key into one field produces.
///
/// The scheme is matched case-insensitively, per RFC 9110: `Bearer`, `bearer`
/// and `BEARER` are the same token.
fn credential(value: &str) -> Option<String> {
    let (scheme, rest) = value.split_once(' ')?;
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return (!rest.is_empty()).then(|| rest.to_owned());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64_decode(rest)?;
        let decoded = String::from_utf8(decoded).ok()?;
        // Split at the *first* colon: RFC 7617 forbids one in the username and
        // says nothing about the password, so a key containing `:` survives.
        let (user, password) = decoded.split_once(':')?;
        let key = if password.is_empty() { user } else { password };
        return (!key.is_empty()).then(|| key.to_owned());
    }
    None
}

/// Standard base64 (RFC 4648 §4), padding optional.
///
/// Hand-written rather than a dependency: this is the only base64 in the
/// binary, it decodes at most a few dozen bytes of header, and a crate would
/// have to earn its place in something that deliberately ships as one static
/// binary. Padding is optional because a client that omits it is still
/// unambiguous, and `Basic` is old enough to have all of them.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => u32::from(byte - b'A'),
            b'a'..=b'z' => u32::from(byte - b'a') + 26,
            b'0'..=b'9' => u32::from(byte - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }

    let input = input.as_bytes();
    let input = input
        .strip_suffix(b"==")
        .or(input.strip_suffix(b"="))
        .unwrap_or(input);
    // One leftover byte cannot be the tail of any encoding: a group of `n`
    // input bytes is 2, 3 or 4 characters, never 1.
    if input.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    for chunk in input.chunks(4) {
        let mut acc = 0u32;
        for &byte in chunk {
            acc = (acc << 6) | sextet(byte)?;
        }
        // A short final chunk holds 6 or 12 significant bits below the byte
        // boundary; shift them up so the same extraction works on every chunk.
        acc <<= 6 * (4 - chunk.len());
        for i in 0..chunk.len() - 1 {
            out.push(((acc >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(user: &str, password: &str) -> String {
        // The encoder exists only here, to build the input the decoder is
        // tested on. Nothing in the server encodes base64.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let raw = format!("{user}:{password}").into_bytes();
        let mut out = String::new();
        for chunk in raw.chunks(3) {
            let mut acc = 0u32;
            for (i, &byte) in chunk.iter().enumerate() {
                acc |= u32::from(byte) << (16 - 8 * i);
            }
            for i in 0..chunk.len() + 1 {
                out.push(ALPHABET[((acc >> (18 - 6 * i)) & 0x3f) as usize] as char);
            }
            for _ in 0..3 - chunk.len() {
                out.push('=');
            }
        }
        format!("Basic {out}")
    }

    fn headers(authorization: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        if !authorization.is_empty() {
            match HeaderValue::from_str(authorization) {
                Ok(value) => {
                    map.insert(header::AUTHORIZATION, value);
                }
                Err(e) => panic!("test header is not a valid header value: {e}"),
            }
        }
        map
    }

    fn policy() -> AuthPolicy {
        AuthPolicy::ApiKey {
            read: ApiKey::new("read-key"),
            write: ApiKey::new("write-key"),
        }
    }

    #[test]
    fn base64_round_trips_every_tail_length() {
        for password in ["", "a", "ab", "abc", "abcd", "abcde", "hunter2", "a:b:c"] {
            let value = basic("user", password);
            let encoded = value.trim_start_matches("Basic ");
            let decoded = base64_decode(encoded).expect("decodes");
            assert_eq!(decoded, format!("user:{password}").as_bytes());
        }
    }

    #[test]
    fn base64_decodes_without_padding() {
        // `dXNlcjprZXk=` is `user:key`; the same value with its padding
        // stripped must decode identically.
        assert_eq!(
            base64_decode("dXNlcjprZXk=").as_deref(),
            Some(&b"user:key"[..])
        );
        assert_eq!(
            base64_decode("dXNlcjprZXk").as_deref(),
            Some(&b"user:key"[..])
        );
    }

    #[test]
    fn base64_rejects_junk() {
        assert_eq!(base64_decode("not base64!"), None);
        assert_eq!(
            base64_decode("a"),
            None,
            "one leftover character is no encoding's tail"
        );
        assert_eq!(base64_decode("dXNlcjpr*Xk="), None);
    }

    #[test]
    fn the_basic_password_is_the_key_and_the_username_is_ignored() {
        assert_eq!(credential(&basic("anyone", "k")).as_deref(), Some("k"));
        assert_eq!(credential(&basic("", "k")).as_deref(), Some("k"));
        assert_eq!(
            credential(&basic("other", "k")).as_deref(),
            Some("k"),
            "no registry should require a particular username for a key"
        );
    }

    #[test]
    fn a_key_containing_a_colon_survives_the_split() {
        assert_eq!(credential(&basic("u", "a:b:c")).as_deref(), Some("a:b:c"));
    }

    #[test]
    fn an_empty_password_falls_back_to_the_username() {
        // What `curl -u <key>:` sends.
        assert_eq!(credential(&basic("k", "")).as_deref(), Some("k"));
        assert_eq!(credential(&basic("", "")), None);
    }

    #[test]
    fn bearer_is_accepted_and_the_scheme_is_case_insensitive() {
        assert_eq!(credential("Bearer k").as_deref(), Some("k"));
        assert_eq!(credential("bearer k").as_deref(), Some("k"));
        assert_eq!(credential("BASIC dXNlcjprZXk=").as_deref(), Some("key"));
        assert_eq!(credential("Bearer "), None);
        assert_eq!(credential("Digest abc"), None);
        assert_eq!(credential("nonsense"), None);
    }

    #[test]
    fn anonymous_admits_everything() {
        let anon = AuthPolicy::Anonymous;
        assert_eq!(anon.authorize(&Method::GET, &HeaderMap::new()), Ok(()));
        assert_eq!(anon.authorize(&Method::PUT, &HeaderMap::new()), Ok(()));
        assert!(!anon.is_enabled());
    }

    #[test]
    fn the_read_key_reads_but_does_not_write() {
        let policy = policy();
        let headers = headers("Bearer read-key");
        assert_eq!(policy.authorize(&Method::GET, &headers), Ok(()));
        assert_eq!(policy.authorize(&Method::HEAD, &headers), Ok(()));
        assert_eq!(
            policy.authorize(&Method::PUT, &headers),
            Err(AuthError::Insufficient)
        );
        assert_eq!(
            policy.authorize(&Method::DELETE, &headers),
            Err(AuthError::Insufficient)
        );
    }

    #[test]
    fn the_write_key_also_reads() {
        let policy = policy();
        let headers = headers("Bearer write-key");
        for method in [
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
        ] {
            assert_eq!(policy.authorize(&method, &headers), Ok(()));
        }
    }

    #[test]
    fn one_key_used_for_both_keeps_its_write_access() {
        let policy = AuthPolicy::ApiKey {
            read: ApiKey::new("same"),
            write: ApiKey::new("same"),
        };
        assert_eq!(
            policy.authorize(&Method::PUT, &headers("Bearer same")),
            Ok(())
        );
    }

    #[test]
    fn a_missing_or_wrong_credential_is_distinguished() {
        let policy = policy();
        assert_eq!(
            policy.authorize(&Method::GET, &HeaderMap::new()),
            Err(AuthError::Missing)
        );
        assert_eq!(
            policy.authorize(&Method::GET, &headers("Bearer nope")),
            Err(AuthError::Invalid)
        );
        assert_eq!(
            policy.authorize(&Method::GET, &headers("Digest nope")),
            Err(AuthError::Malformed)
        );
        assert_eq!(
            policy.authorize(&Method::GET, &headers("Basic %%%")),
            Err(AuthError::Malformed)
        );
    }

    #[test]
    fn an_unknown_method_needs_write() {
        // The safe side: a method nobody has classified must not be readable
        // by a read key on the assumption that it is harmless.
        let unknown = Method::from_bytes(b"FROBNICATE").expect("valid token");
        assert_eq!(Access::of(&unknown), Access::Write);
    }

    #[test]
    fn matching_rejects_the_empty_string_and_prefixes() {
        let key = ApiKey::new("abcdef");
        assert!(key.matches("abcdef"));
        assert!(!key.matches(""));
        assert!(!key.matches("abcde"));
        assert!(!key.matches("abcdefg"));
        assert!(
            !key.matches("abcdefabcdef"),
            "a repeat must not fold to equal"
        );
        assert!(!ApiKey::new("").matches(""));
    }

    #[test]
    fn a_generated_key_is_64_hex_characters_and_not_the_last_one() {
        let key = ApiKey::generate();
        assert_eq!(key.expose().len(), 64);
        assert!(key.expose().bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(key.expose(), ApiKey::generate().expose());
    }

    #[test]
    fn a_key_never_reaches_a_log_through_debug() {
        let policy = policy();
        let rendered = format!("{policy:?}");
        assert!(!rendered.contains("read-key"), "{rendered}");
        assert!(!rendered.contains("write-key"), "{rendered}");
    }

    #[test]
    fn an_empty_supplied_key_is_a_startup_error() {
        assert!(AuthPolicy::new(Some("  ".to_owned()), None).is_err());
        assert!(AuthPolicy::new(None, Some(String::new())).is_err());
    }

    #[test]
    fn absent_keys_are_generated_and_reported_as_such() {
        let (policy, generated) = AuthPolicy::new(Some("mine".to_owned()), None).expect("valid");
        assert!(!generated.read);
        assert!(generated.write, "the write key had to be invented");
        assert_eq!(
            policy.authorize(&Method::GET, &headers("Bearer mine")),
            Ok(())
        );
    }

    #[test]
    fn the_challenge_is_sent_on_a_401_and_withheld_on_a_403() {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;

        for err in [AuthError::Missing, AuthError::Malformed, AuthError::Invalid] {
            let response = err.into_api_error().into_response();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let challenge = response
                .headers()
                .get(header::WWW_AUTHENTICATE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            assert!(challenge.starts_with("Basic "), "{challenge}");
            assert!(challenge.contains("realm=\"summ\""), "{challenge}");
        }

        let response = AuthError::Insufficient.into_api_error().into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(response.headers().get(header::WWW_AUTHENTICATE).is_none());
    }
}
