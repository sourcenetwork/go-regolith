//! Engine options, laid out for C.
//!
//! regolith's [`Options`] has dozens of fields, several of them carrying
//! `Arc<dyn Trait>` hooks that have no C representation at all. This
//! module exposes a deliberately small subset as a flat `#[repr(C)]`
//! struct, and resolves it onto an otherwise-default [`Options`].
//!
//! # Unset is not zero
//!
//! Three of the exposed fields treat `0` as a meaningful setting rather
//! than as "no opinion":
//!
//! * `max_background_compactions = 0` runs compaction on the calling
//!   thread instead of on a worker,
//! * `block_cache_size = 0` disables the block cache entirely,
//! * `transaction_keys_inline = 0` never indexes a transaction's buffer.
//!
//! So a zero-means-default scheme would make those three unreachable. A
//! single `present` bitmask carries which fields the caller actually set
//! instead: a field is read only when its bit is set, and a zeroed
//! struct therefore means "engine defaults, every field". One word to
//! check, no per-field flag to forget, and adding a field later is a new
//! bit constant plus one line in [`options_from`].
//!
//! # Not every field is an engine option
//!
//! `isolation` is not a field of regolith's [`Options`] at all: the
//! level lives on the transaction database, which is what hands it to
//! each transaction it begins. [`options_from`] therefore resolves into
//! a [`ResolvedOptions`] pair - the engine options, plus the things the
//! store handle has to carry itself - rather than into an [`Options`].
//!
//! # Validation
//!
//! Only the things regolith cannot see are checked here: an unknown enum
//! discriminant, and a `u64` that does not fit a `usize` on a 32-bit
//! host. Value ranges are regolith's own business - [`Options::validate`]
//! runs at open and its error names the offending field - so they are
//! mapped through rather than duplicated.

use regolith::{CompressionType, DurabilityMode, IsolationLevel, Options};

use crate::Failure;

// ---------------------------------------------------------------------
// Presence bits. Kept numerically identical to the header.
// ---------------------------------------------------------------------

/// `write_buffer_size` is set.
pub const OPT_WRITE_BUFFER_SIZE: u64 = 1 << 0;
/// `block_cache_size` is set.
pub const OPT_BLOCK_CACHE_SIZE: u64 = 1 << 1;
/// `max_background_compactions` is set.
pub const OPT_MAX_BACKGROUND_COMPACTIONS: u64 = 1 << 2;
/// `transaction_keys_inline` is set.
pub const OPT_TRANSACTION_KEYS_INLINE: u64 = 1 << 3;
/// `compression` is set.
pub const OPT_COMPRESSION: u64 = 1 << 4;
/// `durability` is set.
pub const OPT_DURABILITY: u64 = 1 << 5;
/// `isolation` is set.
pub const OPT_ISOLATION: u64 = 1 << 6;

/// Every bit this version understands. An unknown bit is rejected, so a
/// caller built against a newer header cannot silently get a default.
const OPT_KNOWN: u64 = OPT_WRITE_BUFFER_SIZE
    | OPT_BLOCK_CACHE_SIZE
    | OPT_MAX_BACKGROUND_COMPACTIONS
    | OPT_TRANSACTION_KEYS_INLINE
    | OPT_COMPRESSION
    | OPT_DURABILITY
    | OPT_ISOLATION;

// ---------------------------------------------------------------------
// Enum discriminants. Kept numerically identical to the header.
// ---------------------------------------------------------------------

/// No block compression.
pub const COMPRESSION_NONE: u32 = 0;
/// Snappy block compression.
pub const COMPRESSION_SNAPPY: u32 = 1;
/// LZ4 block compression (regolith's default).
pub const COMPRESSION_LZ4: u32 = 2;

/// Flush to disk on every write.
pub const DURABILITY_IMMEDIATE: u32 = 0;
/// Let the OS flush eventually (regolith's default).
pub const DURABILITY_EVENTUAL: u32 = 1;

/// Validate only what the transaction wrote.
pub const ISOLATION_READ_COMMITTED: u32 = 0;
/// Validate writes and keys read for update (regolith's default).
pub const ISOLATION_SNAPSHOT: u32 = 1;
/// Validate the transaction's entire read set.
pub const ISOLATION_SERIALIZABLE: u32 = 2;
/// Validate every key a point read returned; record a scan per stretch,
/// not per key.
pub const ISOLATION_REPEATABLE_READ: u32 = 3;

/// Engine options, laid out for C.
///
/// Contains no pointers, which is why the Go side can pass a plain
/// struct across without copying anything into C memory. A field is read
/// only when its bit is set in `present`; see the module docs.
#[repr(C)]
pub struct RegolithOptions {
    /// Bitwise OR of the `OPT_*` presence bits for the fields set below.
    pub present: u64,
    /// Memtable bytes before a flush. Engine default: 64 MB.
    pub write_buffer_size: u64,
    /// Block cache bytes; `0` disables it. Engine default: 512 MB.
    pub block_cache_size: u64,
    /// Background compaction threads; `0` compacts on the calling
    /// thread. Engine default: 1.
    pub max_background_compactions: u64,
    /// Keys a transaction buffers before indexing them; `0` never
    /// indexes. Engine default: 32.
    pub transaction_keys_inline: u64,
    /// One of the `COMPRESSION_*` constants. Engine default: LZ4.
    pub compression: u32,
    /// One of the `DURABILITY_*` constants. Engine default: Eventual.
    pub durability: u32,
    /// One of the `ISOLATION_*` constants, used by every transaction the
    /// store begins. Engine default: snapshot isolation.
    pub isolation: u32,
}

/// What a caller's options resolve into: the engine's own [`Options`],
/// plus the settings the store handle has to carry because regolith does
/// not keep them on [`Options`].
#[derive(Debug, Default)]
pub(crate) struct ResolvedOptions {
    /// Passed to `OptimisticTransactionDb::open`.
    pub engine: Options,
    /// Applied with `OptimisticTransactionDb::with_isolation`, and used
    /// by every transaction that store begins.
    pub isolation: IsolationLevel,
}

/// Narrow a C `u64` to a `usize`, naming the field if it does not fit.
///
/// Only reachable on a 32-bit host; there it is the difference between a
/// clear rejection and a silently truncated budget.
fn as_usize(name: &str, value: u64) -> Result<usize, Failure> {
    usize::try_from(value).map_err(|_| {
        Failure::invalid_arg(format!(
            "invalid option `{name}`: {value} does not fit in a usize on this platform"
        ))
    })
}

/// Resolve a caller-supplied options struct onto the engine defaults. A
/// null `opts`, or one with `present == 0`, yields exactly
/// `ResolvedOptions::default()`.
///
/// # Safety
/// `opts` must be null or point at a valid [`RegolithOptions`] for the
/// duration of the call. Nothing is retained past it.
pub(crate) unsafe fn options_from(
    opts: *const RegolithOptions,
) -> Result<ResolvedOptions, Failure> {
    let mut resolved = ResolvedOptions::default();
    let Some(opts) = (unsafe { opts.as_ref() }) else {
        return Ok(resolved);
    };

    let unknown = opts.present & !OPT_KNOWN;
    if unknown != 0 {
        return Err(Failure::invalid_arg(format!(
            "invalid option `present`: unknown presence bits {unknown:#x}"
        )));
    }

    if opts.present & OPT_WRITE_BUFFER_SIZE != 0 {
        resolved.engine.write_buffer_size = as_usize("write_buffer_size", opts.write_buffer_size)?;
    }
    if opts.present & OPT_BLOCK_CACHE_SIZE != 0 {
        resolved.engine.block_cache_size = as_usize("block_cache_size", opts.block_cache_size)?;
    }
    if opts.present & OPT_MAX_BACKGROUND_COMPACTIONS != 0 {
        resolved.engine.max_background_compactions = as_usize(
            "max_background_compactions",
            opts.max_background_compactions,
        )?;
    }
    if opts.present & OPT_TRANSACTION_KEYS_INLINE != 0 {
        resolved.engine.transaction_keys_inline =
            as_usize("transaction_keys_inline", opts.transaction_keys_inline)?;
    }
    if opts.present & OPT_COMPRESSION != 0 {
        resolved.engine.compression = match opts.compression {
            COMPRESSION_NONE => CompressionType::None,
            COMPRESSION_SNAPPY => CompressionType::Snappy,
            COMPRESSION_LZ4 => CompressionType::Lz4,
            other => {
                return Err(Failure::invalid_arg(format!(
                    "invalid option `compression`: unknown codec {other}"
                )));
            }
        };
    }
    if opts.present & OPT_DURABILITY != 0 {
        resolved.engine.durability = match opts.durability {
            DURABILITY_IMMEDIATE => DurabilityMode::Immediate,
            DURABILITY_EVENTUAL => DurabilityMode::Eventual,
            other => {
                return Err(Failure::invalid_arg(format!(
                    "invalid option `durability`: unknown mode {other}"
                )));
            }
        };
    }
    if opts.present & OPT_ISOLATION != 0 {
        resolved.isolation = match opts.isolation {
            ISOLATION_READ_COMMITTED => IsolationLevel::ReadCommitted,
            ISOLATION_SNAPSHOT => IsolationLevel::SnapshotIsolation,
            ISOLATION_SERIALIZABLE => IsolationLevel::Serializable,
            ISOLATION_REPEATABLE_READ => IsolationLevel::RepeatableRead,
            other => {
                return Err(Failure::invalid_arg(format!(
                    "invalid option `isolation`: unknown level {other}"
                )));
            }
        };
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A struct with nothing set, as `calloc` or a Go zero value gives it.
    fn empty() -> RegolithOptions {
        RegolithOptions {
            present: 0,
            write_buffer_size: 0,
            block_cache_size: 0,
            max_background_compactions: 0,
            transaction_keys_inline: 0,
            compression: 0,
            durability: 0,
            isolation: 0,
        }
    }

    fn resolve(opts: &RegolithOptions) -> Result<ResolvedOptions, Failure> {
        unsafe { options_from(opts) }
    }

    /// Every field this module can touch, compared against a reference.
    /// A new exposed field gets a line here and the rest of the tests
    /// keep working.
    fn assert_same_as(got: &ResolvedOptions, want: &ResolvedOptions) {
        assert_eq!(got.engine.write_buffer_size, want.engine.write_buffer_size);
        assert_eq!(got.engine.block_cache_size, want.engine.block_cache_size);
        assert_eq!(
            got.engine.max_background_compactions,
            want.engine.max_background_compactions
        );
        assert_eq!(
            got.engine.transaction_keys_inline,
            want.engine.transaction_keys_inline
        );
        assert_eq!(got.engine.compression, want.engine.compression);
        assert_eq!(got.engine.durability, want.engine.durability);
        assert_eq!(got.isolation, want.isolation);
    }

    #[test]
    fn null_means_defaults() {
        let resolved = unsafe { options_from(std::ptr::null()) }.unwrap();
        assert_same_as(&resolved, &ResolvedOptions::default());
    }

    #[test]
    fn zeroed_struct_means_defaults() {
        // The whole point of the presence mask: a zeroed struct must not
        // set `block_cache_size = 0` (cache disabled) or
        // `max_background_compactions = 0` (no worker).
        let resolved = resolve(&empty()).unwrap();
        assert_same_as(&resolved, &ResolvedOptions::default());
        // regolith's own default, so every benchmark number recorded
        // before this field existed still describes the same engine.
        assert_eq!(resolved.isolation, IsolationLevel::SnapshotIsolation);
    }

    #[test]
    fn each_field_is_set_only_when_its_bit_is() {
        let mut opts = empty();
        opts.write_buffer_size = 8 * 1024 * 1024;
        opts.block_cache_size = 4 * 1024 * 1024;
        opts.max_background_compactions = 2;
        opts.transaction_keys_inline = 128;
        opts.compression = COMPRESSION_NONE;
        opts.durability = DURABILITY_IMMEDIATE;
        opts.isolation = ISOLATION_SERIALIZABLE;

        // Values present, no bits: still every default.
        assert_same_as(&resolve(&opts).unwrap(), &ResolvedOptions::default());

        // One bit at a time: that field moves, the others do not.
        let defaults = Options::default();

        opts.present = OPT_WRITE_BUFFER_SIZE;
        let resolved = resolve(&opts).unwrap();
        assert_eq!(resolved.engine.write_buffer_size, 8 * 1024 * 1024);
        assert_eq!(resolved.engine.block_cache_size, defaults.block_cache_size);
        assert_eq!(resolved.engine.compression, defaults.compression);
        assert_eq!(resolved.isolation, IsolationLevel::default());

        opts.present = OPT_BLOCK_CACHE_SIZE;
        let resolved = resolve(&opts).unwrap();
        assert_eq!(resolved.engine.block_cache_size, 4 * 1024 * 1024);
        assert_eq!(
            resolved.engine.write_buffer_size,
            defaults.write_buffer_size
        );

        opts.present = OPT_MAX_BACKGROUND_COMPACTIONS;
        assert_eq!(resolve(&opts).unwrap().engine.max_background_compactions, 2);

        opts.present = OPT_TRANSACTION_KEYS_INLINE;
        assert_eq!(resolve(&opts).unwrap().engine.transaction_keys_inline, 128);

        opts.present = OPT_COMPRESSION;
        assert_eq!(
            resolve(&opts).unwrap().engine.compression,
            CompressionType::None
        );

        opts.present = OPT_DURABILITY;
        assert_eq!(
            resolve(&opts).unwrap().engine.durability,
            DurabilityMode::Immediate
        );

        opts.present = OPT_ISOLATION;
        let resolved = resolve(&opts).unwrap();
        assert_eq!(resolved.isolation, IsolationLevel::Serializable);
        assert_eq!(
            resolved.engine.write_buffer_size,
            defaults.write_buffer_size
        );
    }

    #[test]
    fn zero_is_distinguishable_from_unset() {
        let defaults = Options::default();

        // max_background_compactions: 0 means "compact on the calling
        // thread", which is not the default.
        assert_ne!(defaults.max_background_compactions, 0);
        let mut opts = empty();
        opts.present = OPT_MAX_BACKGROUND_COMPACTIONS;
        assert_eq!(resolve(&opts).unwrap().engine.max_background_compactions, 0);

        // block_cache_size: 0 disables the cache.
        assert_ne!(defaults.block_cache_size, 0);
        let mut opts = empty();
        opts.present = OPT_BLOCK_CACHE_SIZE;
        assert_eq!(resolve(&opts).unwrap().engine.block_cache_size, 0);

        // transaction_keys_inline: 0 never indexes.
        assert_ne!(defaults.transaction_keys_inline, 0);
        let mut opts = empty();
        opts.present = OPT_TRANSACTION_KEYS_INLINE;
        assert_eq!(resolve(&opts).unwrap().engine.transaction_keys_inline, 0);
    }

    #[test]
    fn every_compression_codec_maps() {
        for (discriminant, want) in [
            (COMPRESSION_NONE, CompressionType::None),
            (COMPRESSION_SNAPPY, CompressionType::Snappy),
            (COMPRESSION_LZ4, CompressionType::Lz4),
        ] {
            let mut opts = empty();
            opts.present = OPT_COMPRESSION;
            opts.compression = discriminant;
            assert_eq!(resolve(&opts).unwrap().engine.compression, want);
        }
    }

    #[test]
    fn every_isolation_level_maps() {
        for (discriminant, want) in [
            (ISOLATION_READ_COMMITTED, IsolationLevel::ReadCommitted),
            (ISOLATION_SNAPSHOT, IsolationLevel::SnapshotIsolation),
            (ISOLATION_SERIALIZABLE, IsolationLevel::Serializable),
            (ISOLATION_REPEATABLE_READ, IsolationLevel::RepeatableRead),
        ] {
            let mut opts = empty();
            opts.present = OPT_ISOLATION;
            opts.isolation = discriminant;
            assert_eq!(resolve(&opts).unwrap().isolation, want);
        }
    }

    #[test]
    fn unknown_enum_discriminant_is_rejected_by_name() {
        use crate::INVALID_ARG;

        let mut opts = empty();
        opts.present = OPT_COMPRESSION;
        opts.compression = 99;
        let failure = resolve(&opts).unwrap_err();
        assert_eq!(failure.status, INVALID_ARG);
        assert!(
            failure
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("compression"))
        );

        let mut opts = empty();
        opts.present = OPT_DURABILITY;
        opts.durability = 7;
        let failure = resolve(&opts).unwrap_err();
        assert_eq!(failure.status, INVALID_ARG);
        assert!(
            failure
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("durability"))
        );

        let mut opts = empty();
        opts.present = OPT_ISOLATION;
        opts.isolation = 4;
        let failure = resolve(&opts).unwrap_err();
        assert_eq!(failure.status, INVALID_ARG);
        assert!(
            failure
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("isolation"))
        );
    }

    #[test]
    fn unknown_presence_bit_is_rejected() {
        use crate::INVALID_ARG;

        let mut opts = empty();
        opts.present = 1 << 40;
        let failure = resolve(&opts).unwrap_err();
        assert_eq!(failure.status, INVALID_ARG);
        assert!(
            failure
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("present"))
        );
    }
}
