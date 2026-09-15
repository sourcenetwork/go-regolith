package regolith

// #include "regolith_ffi.h"
import "C"

import "fmt"

// Options are the engine settings [OpenWith] can apply.
//
// The zero value means "regolith's defaults, every field", so it is exactly
// what [Open] uses.  A field is applied only when it is explicitly set: the
// numeric fields are pointers and the enum fields have an explicit zero member
// meaning "unset", because for several of these settings zero is a real,
// different choice rather than an absence of one.  Setting
// MaxBackgroundCompactions to 0 moves compaction onto the calling thread,
// BlockCacheSize to 0 disables the block cache, and TransactionKeysInline to 0
// stops a transaction ever indexing its buffer - none of which a zero-means-
// default struct could express.
//
// Use [Uint64] to set a numeric field inline:
//
//	db, err := regolith.OpenWith(path, regolith.Options{
//		TransactionKeysInline: regolith.Uint64(8),
//	})
//
// Only a small subset of regolith's own `Options` is reachable so far.  Adding
// another field is a field here, a field and a presence bit in the C struct,
// and one line in each direction.
type Options struct {
	// WriteBufferSize is the memtable size in bytes before a flush.  It must be
	// greater than zero.  Engine default: 64 MB.
	WriteBufferSize *uint64

	// BlockCacheSize is the cache size in bytes for decompressed data blocks.
	// Zero disables the block cache entirely.  Engine default: 512 MB.
	BlockCacheSize *uint64

	// MaxBackgroundCompactions is the number of background threads available for
	// compaction.  Zero starts no worker at all, and compaction then runs on
	// whichever thread asks for it.  Engine default: 1.
	MaxBackgroundCompactions *uint64

	// TransactionKeysInline is the number of keys one transaction buffers before
	// it builds a hash index over them.  Below it, a lookup walks the buffer;
	// above it, the buffer indexes itself at the cost of a table.  Zero never
	// indexes.  Set it to the number of keys the workload's transactions
	// actually touch.  Engine default: 32.
	TransactionKeysInline *uint64

	// Compression is the block compression codec used at every level.  Engine
	// default: [CompressionLZ4].
	Compression Compression

	// Durability is when a write is flushed to disk.  Engine default:
	// [DurabilityEventual].
	Durability Durability

	// Isolation is the isolation level of every transaction the store begins.
	// It is a store-level setting rather than a per-transaction one because
	// that is all corekv's `NewTxn(readonly bool)` leaves room for.  Engine
	// default: [IsolationSnapshot].
	Isolation Isolation
}

// Compression is a block compression codec for SSTable data blocks.
type Compression uint32

const (
	// CompressionDefault leaves the codec at the engine default (LZ4).  It is
	// the zero value, so an [Options] that says nothing about compression gets
	// it.
	CompressionDefault Compression = iota

	// CompressionNone stores blocks uncompressed: the least CPU, the largest
	// on-disk footprint.
	CompressionNone

	// CompressionSnappy is fast with a modest ratio.
	CompressionSnappy

	// CompressionLZ4 is the engine default: decompression slightly faster than
	// Snappy at a comparable ratio.
	CompressionLZ4
)

// Durability is when a write is flushed to disk.
type Durability uint32

const (
	// DurabilityDefault leaves the mode at the engine default (eventual).  It is
	// the zero value, so an [Options] that says nothing about durability gets
	// it.
	DurabilityDefault Durability = iota

	// DurabilityImmediate flushes to disk on every write, which is safe against
	// both process and OS crashes.
	DurabilityImmediate

	// DurabilityEventual is the engine default: the OS flushes eventually, and a
	// process crash is still covered by the write-ahead log.
	DurabilityEventual
)

// Isolation is how much a transaction is protected against concurrent commits,
// which is to say how much of it is validated at commit time.
type Isolation uint32

const (
	// IsolationDefault leaves the level at the engine default (snapshot
	// isolation).  It is the zero value, so an [Options] that says nothing about
	// isolation gets it, and a store opened that way behaves exactly as it did
	// before this option existed.
	IsolationDefault Isolation = iota

	// IsolationReadCommitted validates only the keys the transaction wrote.  A
	// key it merely read is never checked.
	IsolationReadCommitted

	// IsolationSnapshot is the engine default: it validates what the transaction
	// wrote, which is what prevents a lost update.  A plain read is still
	// unvalidated, so two transactions can each read what the other overwrites,
	// write disjoint keys, and both commit - write skew.
	IsolationSnapshot

	// IsolationSerializable validates every key the transaction read as well, so
	// a concurrent commit to any of them aborts it and write skew is
	// unreachable.  The cost is paid by the transaction committing second and is
	// proportional to its read set, which includes every key an iterator opened
	// through [Txn.NewIter] yielded.  A read set is a set of keys, not a
	// predicate: a key inserted into a scanned range by a concurrent commit is
	// not detected.
	IsolationSerializable

	// IsolationRepeatableRead validates every key a point read returned, as
	// IsolationSerializable does, and records an iterator's walk per stretch
	// rather than per key, so a key the iterator merely yielded is never
	// validated on its own: Adya's PL-2.99 over snapshot isolation.  A key it
	// yielded that the transaction then writes is still validated, through the
	// write.  It is the level for a scan over a set that concurrent writers only
	// ever add to or reclaim, such as a head set derived from keys: validated
	// per key, that scan aborts writers no serial order needed to abort and
	// costs one commit check per key walked.
	IsolationRepeatableRead
)

// Presence bits for the C options struct, mirroring the REGOLITH_OPT_* macros
// in the header.  Named here rather than used inline so the mapping is visible
// from Go and testable without cgo.
const (
	optWriteBufferSize          = uint64(C.REGOLITH_OPT_WRITE_BUFFER_SIZE)
	optBlockCacheSize           = uint64(C.REGOLITH_OPT_BLOCK_CACHE_SIZE)
	optMaxBackgroundCompactions = uint64(C.REGOLITH_OPT_MAX_BACKGROUND_COMPACTIONS)
	optTransactionKeysInline    = uint64(C.REGOLITH_OPT_TRANSACTION_KEYS_INLINE)
	optCompression              = uint64(C.REGOLITH_OPT_COMPRESSION)
	optDurability               = uint64(C.REGOLITH_OPT_DURABILITY)
	optIsolation                = uint64(C.REGOLITH_OPT_ISOLATION)
)

// Uint64 returns a pointer to the given value, for setting a numeric [Options]
// field inline.
func Uint64(v uint64) *uint64 {
	return &v
}

// toC converts the options into the flat C struct the FFI layer reads, building
// the presence mask from the fields that are actually set.
//
// The result holds no Go pointers, so a pointer to it may cross into C.
func (o Options) toC() (C.RegolithOptions, error) {
	var c C.RegolithOptions

	if o.WriteBufferSize != nil {
		c.present |= C.uint64_t(optWriteBufferSize)
		c.write_buffer_size = C.uint64_t(*o.WriteBufferSize)
	}
	if o.BlockCacheSize != nil {
		c.present |= C.uint64_t(optBlockCacheSize)
		c.block_cache_size = C.uint64_t(*o.BlockCacheSize)
	}
	if o.MaxBackgroundCompactions != nil {
		c.present |= C.uint64_t(optMaxBackgroundCompactions)
		c.max_background_compactions = C.uint64_t(*o.MaxBackgroundCompactions)
	}
	if o.TransactionKeysInline != nil {
		c.present |= C.uint64_t(optTransactionKeysInline)
		c.transaction_keys_inline = C.uint64_t(*o.TransactionKeysInline)
	}

	switch o.Compression {
	case CompressionDefault:
	case CompressionNone:
		c.present |= C.uint64_t(optCompression)
		c.compression = C.REGOLITH_COMPRESSION_NONE
	case CompressionSnappy:
		c.present |= C.uint64_t(optCompression)
		c.compression = C.REGOLITH_COMPRESSION_SNAPPY
	case CompressionLZ4:
		c.present |= C.uint64_t(optCompression)
		c.compression = C.REGOLITH_COMPRESSION_LZ4
	default:
		return c, fmt.Errorf("%w: invalid option `Compression`: unknown codec %d",
			ErrInvalidArgument, uint32(o.Compression))
	}

	switch o.Durability {
	case DurabilityDefault:
	case DurabilityImmediate:
		c.present |= C.uint64_t(optDurability)
		c.durability = C.REGOLITH_DURABILITY_IMMEDIATE
	case DurabilityEventual:
		c.present |= C.uint64_t(optDurability)
		c.durability = C.REGOLITH_DURABILITY_EVENTUAL
	default:
		return c, fmt.Errorf("%w: invalid option `Durability`: unknown mode %d",
			ErrInvalidArgument, uint32(o.Durability))
	}

	switch o.Isolation {
	case IsolationDefault:
	case IsolationReadCommitted:
		c.present |= C.uint64_t(optIsolation)
		c.isolation = C.REGOLITH_ISOLATION_READ_COMMITTED
	case IsolationSnapshot:
		c.present |= C.uint64_t(optIsolation)
		c.isolation = C.REGOLITH_ISOLATION_SNAPSHOT
	case IsolationSerializable:
		c.present |= C.uint64_t(optIsolation)
		c.isolation = C.REGOLITH_ISOLATION_SERIALIZABLE
	case IsolationRepeatableRead:
		c.present |= C.uint64_t(optIsolation)
		c.isolation = C.REGOLITH_ISOLATION_REPEATABLE_READ
	default:
		return c, fmt.Errorf("%w: invalid option `Isolation`: unknown level %d",
			ErrInvalidArgument, uint32(o.Isolation))
	}

	return c, nil
}
