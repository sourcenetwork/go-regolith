package regolith

import (
	"bytes"
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"
)

// TestWriteBatchRoundTrip checks the zero value, Len, Size and that every
// set written through a batch reads back.
func TestWriteBatchRoundTrip(t *testing.T) {
	db := newDB(t)

	var wb WriteBatch
	entries := []struct{ key, value string }{
		{"a", "1"}, {"b", "22"}, {"c", "333"}, {"d", "4444"}, {"e", "55555"},
	}
	wantSize := 0
	for _, e := range entries {
		wb.Set([]byte(e.key), []byte(e.value))
		wantSize += 1 + 8 + len(e.key) + 8 + len(e.value)
	}
	if got := wb.Len(); got != len(entries) {
		t.Fatalf("Len() = %d, want %d", got, len(entries))
	}
	if got := wb.Size(); got != wantSize {
		t.Fatalf("Size() = %d, want %d", got, wantSize)
	}

	if err := db.Write(&wb); err != nil {
		t.Fatalf("write: %v", err)
	}
	for _, e := range entries {
		got, err := db.Get([]byte(e.key))
		if err != nil {
			t.Fatalf("get %s: %v", e.key, err)
		}
		if string(got) != e.value {
			t.Errorf("get %s = %q, want %q", e.key, got, e.value)
		}
	}
	if got := wb.Len(); got != len(entries) {
		t.Errorf("Len() after write = %d, want %d", got, len(entries))
	}
}

// TestWriteBatchDelete checks that a batch of deletes removes the keys it
// names, leaves the rest, and that deleting an absent key is not an error.
func TestWriteBatchDelete(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	var wb WriteBatch
	wb.Delete([]byte("b"))
	wb.Delete([]byte("d"))
	wb.Delete([]byte("zz"))

	if err := db.Write(&wb); err != nil {
		t.Fatalf("write: %v", err)
	}
	if _, err := db.Get([]byte("b")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get b: expected ErrNotFound, got %v", err)
	}
	if _, err := db.Get([]byte("d")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get d: expected ErrNotFound, got %v", err)
	}
	got, err := db.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get a: %v", err)
	}
	if string(got) != "va" {
		t.Errorf("get a = %q, want %q", got, "va")
	}
	hasZZ, err := db.Has([]byte("zz"))
	if err != nil {
		t.Fatalf("has zz: %v", err)
	}
	if hasZZ {
		t.Errorf("has zz: expected false")
	}
}

// TestWriteBatchMixed checks that sets and deletes on overlapping keys apply
// in the order they were added.
func TestWriteBatchMixed(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	var wb WriteBatch
	wb.Set([]byte("x"), []byte("1"))
	wb.Delete([]byte("a"))
	wb.Set([]byte("a"), []byte("new"))
	wb.Delete([]byte("y"))
	wb.Set([]byte("c"), []byte("c2"))
	wb.Delete([]byte("c"))

	if err := db.Write(&wb); err != nil {
		t.Fatalf("write: %v", err)
	}

	got, err := db.Get([]byte("x"))
	if err != nil {
		t.Fatalf("get x: %v", err)
	}
	if string(got) != "1" {
		t.Errorf("get x = %q, want %q", got, "1")
	}
	got, err = db.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get a: %v", err)
	}
	if string(got) != "new" {
		t.Errorf("get a = %q, want %q", got, "new")
	}
	if _, err := db.Get([]byte("c")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get c: expected ErrNotFound, got %v", err)
	}
	got, err = db.Get([]byte("b"))
	if err != nil {
		t.Fatalf("get b: %v", err)
	}
	if string(got) != "vb" {
		t.Errorf("get b = %q, want %q", got, "vb")
	}
}

// TestWriteBatchEmptyIsNoOp checks that a nil or empty batch writes nothing
// and does not panic, whatever NewWriteBatch's hint was.
func TestWriteBatchEmptyIsNoOp(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	if err := db.Write(&WriteBatch{}); err != nil {
		t.Errorf("write zero value: %v", err)
	}
	if err := db.Write(NewWriteBatch(0)); err != nil {
		t.Errorf("write NewWriteBatch(0): %v", err)
	}
	if err := db.Write(nil); err != nil {
		t.Errorf("write nil: %v", err)
	}

	for _, key := range []string{"a", "b", "c", "d", "e"} {
		got, err := db.Get([]byte(key))
		if err != nil {
			t.Fatalf("get %s: %v", key, err)
		}
		if string(got) != "v"+key {
			t.Errorf("get %s = %q, want %q", key, got, "v"+key)
		}
	}

	wb := NewWriteBatch(64)
	if got := wb.Len(); got != 0 {
		t.Errorf("NewWriteBatch(64).Len() = %d, want 0", got)
	}
	if got := wb.Size(); got != 0 {
		t.Errorf("NewWriteBatch(64).Size() = %d, want 0", got)
	}

	// A negative hint must not be used as a buffer length.
	_ = NewWriteBatch(-1)
}

// TestWriteBatchReuseAfterReset checks that Reset empties a batch for reuse
// and that a write leaves the batch intact, so writing it again re-applies
// the same ops.
func TestWriteBatchReuseAfterReset(t *testing.T) {
	db := newDB(t)

	var wb WriteBatch
	wb.Set([]byte("a"), []byte("1"))
	if err := db.Write(&wb); err != nil {
		t.Fatalf("write a: %v", err)
	}

	wb.Reset()
	if got := wb.Len(); got != 0 {
		t.Errorf("Len() after reset = %d, want 0", got)
	}
	if got := wb.Size(); got != 0 {
		t.Errorf("Size() after reset = %d, want 0", got)
	}

	wb.Set([]byte("b"), []byte("2"))
	if err := db.Write(&wb); err != nil {
		t.Fatalf("write b: %v", err)
	}

	got, err := db.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get a: %v", err)
	}
	if string(got) != "1" {
		t.Errorf("get a = %q, want %q", got, "1")
	}
	got, err = db.Get([]byte("b"))
	if err != nil {
		t.Fatalf("get b: %v", err)
	}
	if string(got) != "2" {
		t.Errorf("get b = %q, want %q", got, "2")
	}

	// Write leaves the batch intact: writing it again without Reset
	// re-applies the same op (b) even though a was deleted in between.
	if err := db.Delete([]byte("a")); err != nil {
		t.Fatalf("delete a: %v", err)
	}
	if err := db.Write(&wb); err != nil {
		t.Fatalf("rewrite: %v", err)
	}
	if _, err := db.Get([]byte("a")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get a after rewrite: expected ErrNotFound, got %v", err)
	}
	got, err = db.Get([]byte("b"))
	if err != nil {
		t.Fatalf("get b after rewrite: %v", err)
	}
	if string(got) != "2" {
		t.Errorf("get b after rewrite = %q, want %q", got, "2")
	}

	// A fresh batch written twice with a delete in between: each write is
	// independent of the store's state at the time it runs.
	var fresh WriteBatch
	fresh.Set([]byte("a"), []byte("1"))
	if err := db.Write(&fresh); err != nil {
		t.Fatalf("first fresh write: %v", err)
	}
	if err := db.Delete([]byte("a")); err != nil {
		t.Fatalf("delete a again: %v", err)
	}
	if err := db.Write(&fresh); err != nil {
		t.Fatalf("second fresh write: %v", err)
	}
	got, err = db.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get a after second fresh write: %v", err)
	}
	if string(got) != "1" {
		t.Errorf("get a after second fresh write = %q, want %q", got, "1")
	}
}

// TestWriteBatchLarge writes 1000 keys of 4 KiB values in one batch and
// checks every key reads back correctly and that Size reports the exact
// packed byte count.
func TestWriteBatchLarge(t *testing.T) {
	db := newDB(t)

	const n = 1000
	wb := NewWriteBatch(0)
	values := make([][]byte, n)
	for i := range n {
		key := []byte(fmt.Sprintf("key:%012d", i))
		value := bytes.Repeat([]byte{byte(i)}, 4096)
		values[i] = value
		wb.Set(key, value)
	}

	wantSize := n * (1 + 8 + 16 + 8 + 4096)
	if got := wb.Size(); got != wantSize {
		t.Fatalf("Size() = %d, want %d", got, wantSize)
	}

	if err := db.Write(wb); err != nil {
		t.Fatalf("write: %v", err)
	}

	for i := range n {
		key := []byte(fmt.Sprintf("key:%012d", i))
		got, err := db.Get(key)
		if err != nil {
			t.Fatalf("get %s: %v", key, err)
		}
		if !bytes.Equal(got, values[i]) {
			t.Errorf("get %s: value mismatch", key)
		}
	}

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer it.Close()
	count := 0
	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}
		count++
	}
	if count != n {
		t.Errorf("iterator count = %d, want %d", count, n)
	}
}

// TestWriteBatchAfterCloseFails checks that Write on a closed store returns
// ErrClosed even for an empty batch, and that Close stays idempotent.
func TestWriteBatchAfterCloseFails(t *testing.T) {
	db, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	var wb WriteBatch
	wb.Set([]byte("a"), []byte("1"))
	if err := db.Write(&wb); !errors.Is(err, ErrClosed) {
		t.Errorf("write: expected ErrClosed, got %v", err)
	}
	if err := db.Write(&WriteBatch{}); !errors.Is(err, ErrClosed) {
		t.Errorf("write empty: expected ErrClosed, got %v", err)
	}

	if err := db.Close(); err != nil {
		t.Fatalf("second close: %v", err)
	}
}

// TestWriteBatchConcurrent drives 8 goroutines, each writing its own batch of
// keys every round, to run the write path under -race and catch a shared
// scratch buffer in the packer.
func TestWriteBatchConcurrent(t *testing.T) {
	const (
		workers  = 8
		rounds   = 50
		keysEach = 20
	)

	db := newDB(t)

	var wg sync.WaitGroup
	for w := range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()

			prefix := fmt.Sprintf("w%d:", w)
			first := []byte(fmt.Sprintf("%s%04d", prefix, 0))
			wb := NewWriteBatch(0)
			for r := range rounds {
				wb.Reset()
				value := []byte(fmt.Sprintf("%d", r))
				for i := range keysEach {
					key := []byte(fmt.Sprintf("%s%04d", prefix, i))
					wb.Set(key, value)
				}
				if err := db.Write(wb); err != nil {
					t.Errorf("write: %v", err)
					return
				}
				got, err := db.Get(first)
				if err != nil {
					t.Errorf("get: %v", err)
					return
				}
				if string(got) != fmt.Sprintf("%d", r) {
					t.Errorf("get = %q, want %q", got, fmt.Sprintf("%d", r))
					return
				}
			}
		}()
	}
	wg.Wait()

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer it.Close()
	count := 0
	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}
		count++
	}
	if want := workers * keysEach; count != want {
		t.Errorf("iterator count = %d, want %d", count, want)
	}
}

// TestWriteBatchRejectsAnOversizedKeyWithDetail checks that an oversized key
// inside a batch is rejected with the engine's own detail, undoubled, and
// that nothing in the batch was written.
func TestWriteBatchRejectsAnOversizedKeyWithDetail(t *testing.T) {
	db := newDB(t)

	var wb WriteBatch
	wb.Set([]byte("a"), []byte("1"))
	wb.Set(make([]byte, 8<<20+1), nil)

	err := db.Write(&wb)
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("expected ErrInvalidArgument, got %v", err)
	}
	if want := "key length 8388609 "; !strings.Contains(err.Error(), want) {
		t.Errorf("expected detail to contain %q, got %v", want, err)
	}
	if got := strings.Count(err.Error(), "invalid argument: "); got != 1 {
		t.Errorf("expected exactly one %q prefix, got %d in %v", "invalid argument: ", got, err)
	}

	if _, err := db.Get([]byte("a")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get a: expected ErrNotFound, got %v", err)
	}
}

// TestWriteBatchMalformedFrameIsRejected drives the FFI decoder from the Go
// side with a hand-built, malformed frame, checking that the Rust detail
// crosses intact and that the store is unharmed afterwards.
func TestWriteBatchMalformedFrameIsRejected(t *testing.T) {
	db := newDB(t)

	wb := &WriteBatch{buf: []byte{9}, n: 1}
	err := db.Write(wb)
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("unknown tag: expected ErrInvalidArgument, got %v", err)
	}
	if want := "unknown tag 9"; !strings.Contains(err.Error(), want) {
		t.Errorf("expected detail to contain %q, got %v", want, err)
	}

	wb = &WriteBatch{buf: []byte{opSet, 5, 0, 0, 0, 0, 0, 0, 0, 'a'}, n: 1}
	err = db.Write(wb)
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("truncated key: expected ErrInvalidArgument, got %v", err)
	}
	if want := "cut off inside its key"; !strings.Contains(err.Error(), want) {
		t.Errorf("expected detail to contain %q, got %v", want, err)
	}

	// The store is unharmed: a normal write still works afterwards.
	if err := db.Set([]byte("a"), []byte("1")); err != nil {
		t.Fatalf("set: %v", err)
	}
	got, err := db.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(got) != "1" {
		t.Errorf("get a = %q, want %q", got, "1")
	}
}

// TestWriteBatchWriteAllocatesNothing pins the #cgo noescape/nocallback
// contract on regolith_db_write, as TestSuccessPathAllocatesNothing does for
// the other entry points.
func TestWriteBatchWriteAllocatesNothing(t *testing.T) {
	db := newDB(t)

	wb := NewWriteBatch(0)
	wb.Set([]byte("a"), []byte("1"))
	wb.Set([]byte("b"), []byte("2"))

	if got := testing.AllocsPerRun(1000, func() {
		if err := db.Write(wb); err != nil {
			t.Fatalf("write: %v", err)
		}
	}); got != 0 {
		t.Errorf("%v allocs per run, want 0", got)
	}
}

// BenchmarkWriteBatch is BenchmarkTxnSet's grouped-write counterpart: same
// shapes, same prefill, but through one WriteBatch and one DB.Write per
// iteration instead of a transaction.
func BenchmarkWriteBatch(b *testing.B) {
	for _, shape := range benchShapes {
		b.Run(fmt.Sprintf("keys=%d/value=%d", shape.keys, shape.value), func(b *testing.B) {
			db := benchDB(b)
			keys := benchKeys(shape.keys)
			value := benchValue(shape.value)

			prefill := NewWriteBatch(0)
			for _, key := range keys {
				prefill.Set(key, value)
			}
			if err := db.Write(prefill); err != nil {
				b.Fatalf("prefill write: %v", err)
			}

			wb := NewWriteBatch(0)
			b.ReportAllocs()
			b.ResetTimer()
			for range b.N {
				wb.Reset()
				for _, key := range keys {
					wb.Set(key, value)
				}
				if err := db.Write(wb); err != nil {
					b.Fatalf("write: %v", err)
				}
			}
			reportPerKey(b, shape.keys)
		})
	}
}
