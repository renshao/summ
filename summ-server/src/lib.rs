//! The HTTP layer: the `/v2/` Distribution Spec surface, the spec error model,
//! and the request validation that has to happen before any storage is touched.
//!
//! Everything below the wire is reached through [`seam::Registry`], a single
//! narrow trait. That is deliberate: the spec's sharp edges - the two
//! `Content-Range` grammars, `Docker-Content-Digest`, out-of-order chunk
//! rejection, pagination `Link` headers, the name grammar - are all decided
//! here, above storage, so they can be tested without one. [`backend`] is the
//! real implementation of that trait - `summ-registry` over `summ-meta`, with
//! `summ-storage` holding the bytes - and [`memory`] is a second one, kept
//! because a trait with one implementation is not a seam.
//!
//! Two things the middleware stack deliberately does *not* have, both from
//! `research/R5`:
//!
//! - **No compression layer.** A blob's digest is over its plaintext bytes;
//!   any transform of the body breaks it. There is no path on which a
//!   `CompressionLayer` would be safe, so there is none anywhere.
//! - **No rate limiter returning `429`.** containerd retries `429` immediately,
//!   five times, ignoring `Retry-After`, so it amplifies load rather than
//!   shedding it. Throttling, if it is ever needed, belongs at the connection
//!   or accept level.

pub mod app;
pub mod backend;
pub mod config;
pub mod error;
pub mod handlers;
pub mod memory;
pub mod pagination;
pub mod query;
pub mod range;
pub mod reference;
pub mod seam;
pub mod ui;

pub use app::{router, AppState};
pub use config::ServerConfig;
pub use error::{ApiError, ErrorCode};
