//! The two range grammars, kept in one file so the contrast is impossible to
//! miss. Mixing them up is the classic Distribution Spec implementation bug.
//!
//! | Where | Header | Grammar | Example |
//! |---|---|---|---|
//! | Blob **download** | `Range` / `Content-Range` | RFC 9110 §14 | `bytes 500-1499/2048` |
//! | Blob **upload**, chunked | `Content-Range` | spec §Pushing a blob in chunks | `0-1023` |
//!
//! The upload form has **no `bytes ` prefix and no `/total` suffix**, is
//! inclusive on both ends, and is anchored to `^[0-9]+-[0-9]+$`. The `202`
//! answering a chunk echoes a bare `Range: 0-<end>` in the same dialect.
//!
//! Because the two are so nearly identical, each parser explicitly rejects the
//! other's syntax rather than tolerating it. Accepting `bytes 0-1023/2048` on a
//! `PATCH` would let a confused client push chunks that silently disagree with
//! our offset accounting.

/// A resolved window into a blob, inclusive at both ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    /// Bytes covered. Named `size` rather than `len` because the range is
    /// inclusive at both ends and can never be empty, so the usual
    /// `len`/`is_empty` pairing would be misleading.
    pub fn size(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// What a `Range` header on a blob `GET` resolves to against a known length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOutcome {
    /// No usable range: serve the whole blob with `200`. RFC 9110 permits a
    /// server to ignore a `Range` it does not understand, which is what we do
    /// for multi-range requests until `multipart/byteranges` is implemented.
    Whole,
    /// Serve `206` with this window.
    Partial(ByteRange),
    /// Serve `416` with `Content-Range: bytes */<len>`.
    Unsatisfiable,
}

/// Parse an RFC 9110 `Range` header for a blob of `len` bytes.
///
/// The six cases the conformance suite exercises against a 2048-byte blob, all
/// covered by tests below:
///
/// | Request | Result |
/// |---|---|
/// | `bytes=500-1499` | `206`, 1000 bytes |
/// | `bytes=500-` | `206`, to EOF |
/// | `bytes=-500` | `206`, last 500 bytes |
/// | `bytes=2000-5000` | `206`, end clamped to EOF |
/// | `bytes=500-0` | `416` |
/// | `bytes=5000-10000` | `416` |
///
/// `bytes=500-0` is the one that does not follow from RFC 9110 alone: a
/// first-byte-pos greater than last-byte-pos makes the range *spec* invalid,
/// which strictly means the whole header should be ignored and a `200`
/// returned. The suite requires `416`, and the suite is the gate.
pub fn parse_range(header: &str, len: u64) -> RangeOutcome {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Whole;
    };
    let spec = spec.trim();
    // Multi-range is legal to ignore, and ignoring it is strictly better than
    // answering the first range only: containerd discards a mismatched body
    // and falls back to a single stream, whereas a wrong 206 is a corrupt pull.
    if spec.contains(',') {
        return RangeOutcome::Whole;
    }

    let Some((first, last)) = spec.split_once('-') else {
        return RangeOutcome::Whole;
    };
    let (first, last) = (first.trim(), last.trim());

    // A zero-length blob can satisfy no range at all.
    if len == 0 {
        return RangeOutcome::Unsatisfiable;
    }

    match (first.is_empty(), last.is_empty()) {
        // `-N`: the final N bytes.
        (true, false) => match last.parse::<u64>() {
            Ok(0) => RangeOutcome::Unsatisfiable,
            Ok(suffix) => RangeOutcome::Partial(ByteRange {
                start: len.saturating_sub(suffix),
                end: len - 1,
            }),
            Err(_) => RangeOutcome::Whole,
        },
        // `N-`: from N to EOF.
        (false, true) => match first.parse::<u64>() {
            Ok(start) if start >= len => RangeOutcome::Unsatisfiable,
            Ok(start) => RangeOutcome::Partial(ByteRange {
                start,
                end: len - 1,
            }),
            Err(_) => RangeOutcome::Whole,
        },
        // `A-B`: end clamped to EOF, start beyond EOF is unsatisfiable.
        (false, false) => match (first.parse::<u64>(), last.parse::<u64>()) {
            (Ok(start), Ok(end)) => {
                if start >= len || start > end {
                    RangeOutcome::Unsatisfiable
                } else {
                    RangeOutcome::Partial(ByteRange {
                        start,
                        end: end.min(len - 1),
                    })
                }
            }
            _ => RangeOutcome::Whole,
        },
        (true, true) => RangeOutcome::Whole,
    }
}

/// Why a chunked-upload `Content-Range` was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkRangeError {
    /// Not `^[0-9]+-[0-9]+$`. Includes the common mistake of sending the
    /// RFC 9110 form.
    Malformed(String),
    /// `start > end`.
    Inverted,
}

/// Parse the chunked-upload `Content-Range`: `^[0-9]+-[0-9]+$`, inclusive.
///
/// Explicitly rejects `bytes ` prefixes and `/total` suffixes so that a client
/// sending the download grammar gets a clear `400` rather than having its
/// chunk written at an offset nobody agreed on.
pub fn parse_chunk_range(value: &str) -> Result<ByteRange, ChunkRangeError> {
    let raw = value.trim();
    if raw.is_empty() {
        return Err(ChunkRangeError::Malformed(value.to_owned()));
    }
    if raw.contains('/') || raw.as_bytes().iter().any(|c| c.is_ascii_alphabetic()) {
        return Err(ChunkRangeError::Malformed(format!(
            "chunked upload Content-Range is `<start>-<end>`, not RFC 9110 form: {raw}"
        )));
    }
    let Some((first, last)) = raw.split_once('-') else {
        return Err(ChunkRangeError::Malformed(raw.to_owned()));
    };
    if first.is_empty()
        || last.is_empty()
        || !first.bytes().all(|c| c.is_ascii_digit())
        || !last.bytes().all(|c| c.is_ascii_digit())
    {
        return Err(ChunkRangeError::Malformed(raw.to_owned()));
    }
    let (Ok(start), Ok(end)) = (first.parse::<u64>(), last.parse::<u64>()) else {
        return Err(ChunkRangeError::Malformed(raw.to_owned()));
    };
    if start > end {
        return Err(ChunkRangeError::Inverted);
    }
    Ok(ByteRange { start, end })
}

/// The bare `Range` an upload response carries: `0-<last byte written>`.
///
/// After 1024 bytes this is `0-1023`, not `0-1024` - the value is the position
/// of the last uploaded byte, and an off-by-one here breaks resumable uploads
/// silently. A session with nothing written yet reports `0-0`, which is what
/// the reference implementation does; it is a wart of a grammar with no way to
/// express an empty range, and clients read the offset from it only after a
/// successful chunk.
pub fn upload_range_header(offset: u64) -> String {
    format!("0-{}", offset.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: u64 = 2048;

    fn partial(start: u64, end: u64) -> RangeOutcome {
        RangeOutcome::Partial(ByteRange { start, end })
    }

    #[test]
    fn the_six_conformance_range_cases() {
        assert_eq!(parse_range("bytes=500-1499", LEN), partial(500, 1499));
        assert_eq!(parse_range("bytes=500-", LEN), partial(500, 2047));
        assert_eq!(parse_range("bytes=-500", LEN), partial(1548, 2047));
        assert_eq!(parse_range("bytes=2000-5000", LEN), partial(2000, 2047));
        assert_eq!(parse_range("bytes=500-0", LEN), RangeOutcome::Unsatisfiable);
        assert_eq!(
            parse_range("bytes=5000-10000", LEN),
            RangeOutcome::Unsatisfiable
        );
    }

    #[test]
    fn range_lengths_are_inclusive() {
        assert_eq!(
            ByteRange {
                start: 500,
                end: 1499
            }
            .size(),
            1000
        );
        assert_eq!(
            ByteRange {
                start: 500,
                end: 2047
            }
            .size(),
            1548
        );
        assert_eq!(
            ByteRange {
                start: 1548,
                end: 2047
            }
            .size(),
            500
        );
        assert_eq!(
            ByteRange {
                start: 2000,
                end: 2047
            }
            .size(),
            48
        );
    }

    #[test]
    fn unparseable_or_multi_ranges_fall_back_to_the_whole_blob() {
        assert_eq!(parse_range("items=0-1", LEN), RangeOutcome::Whole);
        assert_eq!(parse_range("bytes=abc-def", LEN), RangeOutcome::Whole);
        assert_eq!(parse_range("bytes=0-99,200-299", LEN), RangeOutcome::Whole);
        assert_eq!(parse_range("bytes=-", LEN), RangeOutcome::Whole);
    }

    #[test]
    fn an_empty_blob_satisfies_no_range() {
        assert_eq!(parse_range("bytes=0-0", 0), RangeOutcome::Unsatisfiable);
    }

    #[test]
    fn chunk_range_accepts_only_the_bare_grammar() {
        assert_eq!(
            parse_chunk_range("0-1023"),
            Ok(ByteRange {
                start: 0,
                end: 1023
            })
        );
        assert_eq!(
            parse_chunk_range("1024-2047"),
            Ok(ByteRange {
                start: 1024,
                end: 2047
            })
        );
    }

    #[test]
    fn chunk_range_rejects_the_download_grammar() {
        // The whole point of the file: these are the two forms that get mixed
        // up, and the upload parser must not accept the download one.
        assert!(matches!(
            parse_chunk_range("bytes 0-1023/2048"),
            Err(ChunkRangeError::Malformed(_))
        ));
        assert!(matches!(
            parse_chunk_range("bytes=0-1023"),
            Err(ChunkRangeError::Malformed(_))
        ));
        assert!(matches!(
            parse_chunk_range("0-1023/2048"),
            Err(ChunkRangeError::Malformed(_))
        ));
    }

    #[test]
    fn chunk_range_rejects_junk_and_inversion() {
        assert!(matches!(
            parse_chunk_range("0"),
            Err(ChunkRangeError::Malformed(_))
        ));
        assert!(matches!(
            parse_chunk_range("-5"),
            Err(ChunkRangeError::Malformed(_))
        ));
        assert!(matches!(
            parse_chunk_range("5-"),
            Err(ChunkRangeError::Malformed(_))
        ));
        assert_eq!(parse_chunk_range("10-5"), Err(ChunkRangeError::Inverted));
    }

    #[test]
    fn upload_range_reports_the_last_written_byte() {
        assert_eq!(upload_range_header(1024), "0-1023");
        assert_eq!(upload_range_header(1), "0-0");
        assert_eq!(upload_range_header(0), "0-0");
    }
}
