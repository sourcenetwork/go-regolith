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

			prefill, err := db.NewTxn(false)
			if err != nil {
				b.Fatalf("prefill new txn: %v", err)
			}
			for _, key := range keys {
				if err := prefill.Set(key, value); err != nil {
					b.Fatalf("prefill set: %v", err)
				}
			}
			if err := prefill.Commit(); err != nil {
				b.Fatalf("prefill commit: %v", err)
			}

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
