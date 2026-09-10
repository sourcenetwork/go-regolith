package regolith

import (
	"errors"
	"strings"
	"testing"
)

// exercise writes, reads and commits through the given store, so a test can
// assert that a setting produced a working one rather than only an open one.
func exercise(t *testing.T, db *DB) {
	t.Helper()

	if err := db.Set([]byte("k"), []byte("v")); err != nil {
		t.Fatalf("set: %v", err)
	}
	value, err := db.Get([]byte("k"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "v" {
		t.Fatalf("get: got %q, want %q", value, "v")
	}

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()
	if err := txn.Set([]byte("t"), []byte("tv")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	if err := txn.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if value, err := db.Get([]byte("t")); err != nil || string(value) != "tv" {
		t.Fatalf("get after commit: got %q, %v", value, err)
	}
}

// openWith opens a store with the given options, closing it when the test
// finishes.
func openWith(t *testing.T, opts Options) *DB {
	t.Helper()

	db, err := OpenWith(t.TempDir(), opts)
	if err != nil {
		t.Fatalf("open with options: %v", err)
	}
	t.Cleanup(func() {
		if err := db.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})

	return db
}

func TestOpenWithZeroOptionsIsOpen(t *testing.T) {
	// The guarantee existing callers rely on: a zero Options is the engine
	// defaults, exactly as Open has always used.
	exercise(t, openWith(t, Options{}))

	var zero Options
	cOpts, err := zero.toC()
	if err != nil {
		t.Fatalf("toC: %v", err)
	}
	if cOpts.present != 0 {
		t.Errorf("zero options set presence bits %#x, want none", uint64(cOpts.present))
	}
}

func TestOpenWithEachFieldProducesAWorkingStore(t *testing.T) {
	for name, opts := range map[string]Options{
		"WriteBufferSize":          {WriteBufferSize: Uint64(1 << 20)},
		"BlockCacheSize":           {BlockCacheSize: Uint64(2 << 20)},
		"BlockCacheSizeDisabled":   {BlockCacheSize: Uint64(0)},
		"MaxBackgroundCompactions": {MaxBackgroundCompactions: Uint64(2)},
		"NoBackgroundCompactions":  {MaxBackgroundCompactions: Uint64(0)},
		"TransactionKeysInline":    {TransactionKeysInline: Uint64(128)},
		"NoTransactionIndex":       {TransactionKeysInline: Uint64(0)},
		"CompressionNone":          {Compression: CompressionNone},
		"CompressionSnappy":        {Compression: CompressionSnappy},
		"CompressionLZ4":           {Compression: CompressionLZ4},
		"DurabilityImmediate":      {Durability: DurabilityImmediate},
		"DurabilityEventual":       {Durability: DurabilityEventual},
		"Everything": {
			WriteBufferSize:          Uint64(4 << 20),
			BlockCacheSize:           Uint64(8 << 20),
			MaxBackgroundCompactions: Uint64(1),
			TransactionKeysInline:    Uint64(8),
			Compression:              CompressionSnappy,
			Durability:               DurabilityEventual,
		},
	} {
		t.Run(name, func(t *testing.T) {
			exercise(t, openWith(t, opts))
		})
	}
}

func TestOptionsPresenceMaskTracksWhatWasSet(t *testing.T) {
	// Unset must not be the same as zero, and the presence mask is what makes
	// the difference.  A field set to 0 carries its bit; an absent field does
	// not, whatever the rest of the struct holds.
	for name, test := range map[string]struct {
		opts Options
		bit  uint64
	}{
		"WriteBufferSize": {
			Options{WriteBufferSize: Uint64(1 << 20)},
			optWriteBufferSize,
		},
		"BlockCacheSizeZero": {
			Options{BlockCacheSize: Uint64(0)},
			optBlockCacheSize,
		},
		"MaxBackgroundCompactionsZero": {
			Options{MaxBackgroundCompactions: Uint64(0)},
			optMaxBackgroundCompactions,
		},
		"TransactionKeysInlineZero": {
			Options{TransactionKeysInline: Uint64(0)},
			optTransactionKeysInline,
		},
		"Compression": {
			Options{Compression: CompressionNone},
			optCompression,
		},
		"Durability": {
			Options{Durability: DurabilityImmediate},
			optDurability,
		},
	} {
		t.Run(name, func(t *testing.T) {
			cOpts, err := test.opts.toC()
			if err != nil {
				t.Fatalf("toC: %v", err)
			}
			if uint64(cOpts.present) != test.bit {
				t.Errorf("presence mask is %#x, want exactly %#x",
					uint64(cOpts.present), test.bit)
			}
		})
	}
}

func TestOpenWithRejectsAValueTheEngineRefuses(t *testing.T) {
	// regolith requires a non-zero write buffer, and validates before doing any
	// filesystem work.  The error has to name the field rather than clamping.
	_, err := OpenWith(t.TempDir(), Options{WriteBufferSize: Uint64(0)})
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("open: got %v, want an invalid-argument error", err)
	}
	if got := err.Error(); !strings.Contains(got, "write_buffer_size") {
		t.Errorf("error does not name the field: %v", got)
	}
}

func TestOpenWithRejectsAnUnknownEnumValue(t *testing.T) {
	for name, test := range map[string]struct {
		opts  Options
		field string
	}{
		"Compression": {Options{Compression: Compression(99)}, "Compression"},
		"Durability":  {Options{Durability: Durability(99)}, "Durability"},
	} {
		t.Run(name, func(t *testing.T) {
			_, err := OpenWith(t.TempDir(), test.opts)
			if !errors.Is(err, ErrInvalidArgument) {
				t.Fatalf("open: got %v, want an invalid-argument error", err)
			}
			if got := err.Error(); !strings.Contains(got, test.field) {
				t.Errorf("error does not name the field: %v", got)
			}
		})
	}
}

func TestOpenWithNoBackgroundCompactionsStillFlushes(t *testing.T) {
	// The setting a zero-means-default scheme would have made unreachable: no
	// compaction worker, so compaction happens on the calling thread.  A write
	// buffer small enough to rotate several times proves that path is live.
	db := openWith(t, Options{
		MaxBackgroundCompactions: Uint64(0),
		WriteBufferSize:          Uint64(64 << 10),
	})

	value := make([]byte, 4096)
	for i := range value {
		value[i] = 'x'
	}
	for i := 0; i < 64; i++ {
		key := []byte{'k', byte(i)}
		if err := db.Set(key, value); err != nil {
			t.Fatalf("set %d: %v", i, err)
		}
	}
	if got, err := db.Get([]byte{'k', 0}); err != nil || len(got) != len(value) {
		t.Fatalf("get: got %d bytes, %v", len(got), err)
	}
}
