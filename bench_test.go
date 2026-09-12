package regolith

import (
	"bytes"
	"fmt"
	"testing"
)

// benchShapes is the key count x value size grid every write benchmark in
// this package reports against, so grouped-write costs stay comparable
// across benchmark functions.
var benchShapes = []struct{ keys, value int }{
	{100, 64}, {100, 4096}, {1000, 64}, {1000, 4096},
}

// benchDB opens a store in a benchmark-scoped temporary directory, closed
// when the benchmark finishes.
func benchDB(b *testing.B) *DB {
	b.Helper()

	db, err := Open(b.TempDir())
	if err != nil {
		b.Fatalf("open: %v", err)
	}
	b.Cleanup(func() {
		if err := db.Close(); err != nil {
			b.Errorf("close: %v", err)
		}
	})

	return db
}

// benchKeys returns n keys in the corekv bench harness's own shape, built
// once outside the timed loop so formatting them is not part of what is
// measured.
func benchKeys(n int) [][]byte {
	keys := make([][]byte, n)
	for i := range n {
		keys[i] = []byte(fmt.Sprintf("key:%012d", i))
	}

	return keys
}

// benchValue returns an n-byte value of repeated 'v' bytes.
func benchValue(n int) []byte {
	return bytes.Repeat([]byte{'v'}, n)
}

// reportPerKey adds a ns/key metric to b, computed from the benchmark's own
// elapsed time and iteration count, so it must be called after the timed
// loop has finished.
func reportPerKey(b *testing.B, keys int) {
	b.Helper()
	b.ReportMetric(float64(b.Elapsed().Nanoseconds())/float64(b.N)/float64(keys), "ns/key")
}

// benchPrefill writes every key with one WriteBatch through db.Write,
// outside the timed loop, so every benchmark in this file starts from the
// same on-disk shape without that setup counting toward what any of them
// measures.
func benchPrefill(b *testing.B, db *DB, keys [][]byte, value []byte) {
	b.Helper()

	wb := NewWriteBatch(0)
	for _, key := range keys {
		wb.Set(key, value)
	}
	if err := db.Write(wb); err != nil {
		b.Fatalf("prefill write: %v", err)
	}
}

// BenchmarkTxnSet groups keys writes into one transaction per iteration, the
// grouped-write baseline [BenchmarkWriteBatch] is measured against.  It
// mirrors corekv/bench/workload.go's BatchWrite lane without the corekv
// adapter.
func BenchmarkTxnSet(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)
			benchPrefill(b, db, keys, value)

			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				txn, err := db.NewTxn(false)
				if err != nil {
					b.Fatalf("new txn: %v", err)
				}
				for _, key := range keys {
					if err := txn.Set(key, value); err != nil {
						b.Fatalf("set: %v", err)
					}
				}
				if err := txn.Commit(); err != nil {
					b.Fatalf("commit: %v", err)
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}

// BenchmarkGet reads every key through [DB.Get], a hit each time.  It is one
// of the four read baselines for the boundary directives that keep the
// out-param locals of a call off the Go heap (regolith.go:42-103).
func BenchmarkGet(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)
			benchPrefill(b, db, keys, value)

			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				for _, key := range keys {
					got, err := db.Get(key)
					if err != nil {
						b.Fatalf("get: %v", err)
					}
					// A dropped read would still pass a benchmark that never
					// looks at the result, so check the length every time.
					if len(got) != len(value) {
						b.Fatalf("get: got %d bytes, want %d", len(got), len(value))
					}
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}

// BenchmarkHas tests every key through [DB.Has], a hit each time, the
// allocation-free counterpart of [BenchmarkGet]: the borrowed value is never
// copied, so a run that reports an allocation has grown one.
func BenchmarkHas(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)
			benchPrefill(b, db, keys, value)

			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				for _, key := range keys {
					found, err := db.Has(key)
					if err != nil {
						b.Fatalf("has: %v", err)
					}
					if !found {
						b.Fatalf("has: %q not found", key)
					}
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}

// BenchmarkTxnGet reads every key through one read-only transaction's
// [Txn.Get], opened once outside the timed loop, so what it measures is the
// read itself rather than the transaction around it.
func BenchmarkTxnGet(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)
			benchPrefill(b, db, keys, value)

			txn, err := db.NewTxn(true)
			if err != nil {
				b.Fatalf("new txn: %v", err)
			}
			b.Cleanup(txn.Discard)

			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				for _, key := range keys {
					got, err := txn.Get(key)
					if err != nil {
						b.Fatalf("txn get: %v", err)
					}
					if len(got) != len(value) {
						b.Fatalf("txn get: got %d bytes, want %d", len(got), len(value))
					}
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}

// BenchmarkTxnHas tests every key through one read-only transaction's
// [Txn.Has], opened once outside the timed loop, the allocation-free
// counterpart of [BenchmarkTxnGet].
func BenchmarkTxnHas(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)
			benchPrefill(b, db, keys, value)

			txn, err := db.NewTxn(true)
			if err != nil {
				b.Fatalf("new txn: %v", err)
			}
			b.Cleanup(txn.Discard)

			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				for _, key := range keys {
					found, err := txn.Has(key)
					if err != nil {
						b.Fatalf("txn has: %v", err)
					}
					if !found {
						b.Fatalf("txn has: %q not found", key)
					}
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}
