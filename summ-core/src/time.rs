//! A single instant, carried from the request that started an operation.
//!
//! The ops layer never reads a clock: a `WriteBatch` carrying an apply-time
//! timestamp would mean something different on a replica than it did on the
//! node that built it, and the batch is the future WAL. So the clock is read
//! once per request and the value travels with the operation.
//!
//! It is a newtype rather than a bare `u64` because two resolutions are in use
//! and they differ by a factor of 1000, which is exactly the kind of mistake
//! that stores a plausible-looking wrong number:
//!
//! - **Stored records hold seconds.** `TagRecord.tagged_at`,
//!   `ManifestRecord.pushed_at`, `RepoBlobRecord.added_at` and
//!   `UploadSession.started_at` are all a second's resolution and have been
//!   since the first commit.
//! - **Tag-history keys hold milliseconds.** `H` and `J` order events by time,
//!   and a second is not fine enough to order them: a create and a delete of
//!   the same tag at the same digest inside one second encode to *the same
//!   key*, so the later write silently replaces the earlier, and two distinct
//!   events in one second come back ordered by digest rather than by what
//!   happened. Deleting and re-pushing a tag from a script does this.
//!
//! Callers therefore have to say which they want, and the compiler asks.
//!
//! The two encodings are self-distinguishing, which is what made the change
//! free on a populated store: a seconds value now is about 1.7e9 and a
//! milliseconds value about 1.7e12, and they cannot overlap until the year
//! 5138. Events written before this are read back correctly by magnitude, and
//! because milliseconds are numerically larger every new event still sorts as
//! newer than every old one.

/// Unix milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis)
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000))
    }

    /// For a history key, where ordering is the point.
    pub const fn millis(self) -> u64 {
        self.0
    }

    /// For a stored record, every one of which is a second's resolution.
    pub const fn secs(self) -> u64 {
        self.0 / 1_000
    }
}

#[cfg(test)]
mod tests {
    use super::Timestamp;

    #[test]
    fn seconds_and_milliseconds_convert_both_ways() {
        let t = Timestamp::from_secs(1_700_000_000);
        assert_eq!(t.millis(), 1_700_000_000_000);
        assert_eq!(t.secs(), 1_700_000_000);
        assert_eq!(
            Timestamp::from_millis(1_700_000_000_499).secs(),
            1_700_000_000
        );
    }

    /// The property the migration rests on: the two encodings cannot be
    /// confused for one another within any lifetime this registry has.
    #[test]
    fn a_legacy_seconds_key_sorts_older_than_every_millisecond_key() {
        let legacy_seconds = 1_700_000_000_u64;
        let now_millis = Timestamp::from_secs(1_700_000_000).millis();
        assert!(now_millis > legacy_seconds);
        // Complemented, so "greater" means "sorts earlier", i.e. newer.
        assert!(!now_millis < !legacy_seconds);
    }

    #[test]
    fn ordering_is_by_instant() {
        assert!(Timestamp::from_millis(1_001) > Timestamp::from_millis(1_000));
    }
}
