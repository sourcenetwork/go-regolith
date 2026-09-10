package regolith

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

// borrowAll walks the iterator and collects, per entry, the value as seen by
// each of the three value paths, so that a caller can assert they agree.
//
// The borrowed bytes are copied immediately, inside the callback, because that
// is the only place they are valid.
type valueViews struct {
	key      string
	value    []byte
	borrowed []byte
	appended []byte
	calls    int
}

// collectValueViews drains it, reading every entry through Value, BorrowValue
// and AppendValue.
//
// The AppendValue destination is deliberately a single buffer re-used across the
// whole walk, with a sentinel prefix, so that both the no-allocation path and
// the "prefix survives" requirement are exercised on every entry.
func collectValueViews(t *testing.T, it *Iter) []valueViews {
	t.Helper()

	const prefix = "keep-me:"

	out := []valueViews{}
	buf := make([]byte, 0, 8)

	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			return out
		}

		view := valueViews{key: string(it.Key())}

		value, err := it.Value()
		if err != nil {
			t.Fatalf("value %s: %v", view.key, err)
		}
		view.value = value

		err = it.BorrowValue(func(borrowed []byte) error {
			view.calls++
			view.borrowed = append([]byte(nil), borrowed...)

			return nil
		})
		if err != nil {
			t.Fatalf("borrow value %s: %v", view.key, err)
		}

		buf = append(buf[:0], prefix...)
		buf, err = it.AppendValue(buf)
		if err != nil {
			t.Fatalf("append value %s: %v", view.key, err)
		}
		if got := string(buf[:len(prefix)]); got != prefix {
			t.Fatalf("append value %s clobbered dst: %q", view.key, got)
		}
		view.appended = append([]byte(nil), buf[len(prefix):]...)

		out = append(out, view)
	}
}

// assertValueViewsAgree checks that all three value paths saw the same bytes for
// every entry, and that the callback was called exactly once per entry.
//
// bytes.Equal rather than a nil-sensitive comparison, because the paths are
// allowed to differ in nil-ness: an empty value is nil from Value and an empty
// non-nil slice out of a copy of a zero-length borrow.
func assertValueViewsAgree(t *testing.T, views []valueViews) {
	t.Helper()

	for _, view := range views {
		if view.calls != 1 {
			t.Errorf("%s: callback called %d times, expected once", view.key, view.calls)
		}
		if !bytes.Equal(view.value, view.borrowed) {
			t.Errorf("%s: BorrowValue saw %q, Value saw %q", view.key, view.borrowed, view.value)
		}
		if !bytes.Equal(view.value, view.appended) {
			t.Errorf("%s: AppendValue saw %q, Value saw %q", view.key, view.appended, view.value)
		}
	}
}

// bigValue builds a value too large to be mistaken for an inline one, and
// distinct per key so that a mix-up between entries cannot pass.
func bigValue(key string, size int) []byte {
	value := make([]byte, 0, size)
	for len(value) < size {
		value = append(value, key...)
		value = append(value, '-')
	}

	return value[:size]
}

// newSpanningDB opens a store whose data deliberately spans a flushed SSTable
// and a live memtable, and returns the values it wrote, keyed by key.
//
// The two halves have different owners on the other side of the boundary - a
// block the block cache holds, versus a memtable arena chunk - and
// `regolith_iter_batch_value` borrows from whichever one the entry came from, so
// a scan that crosses the boundary is the case worth testing.  A tiny write
// buffer is what forces the flush: the first half of the keys overflows it.
func newSpanningDB(t *testing.T, keyCount, valueSize int) (*DB, map[string][]byte) {
	t.Helper()

	path := t.TempDir()
	db, err := OpenWith(path, Options{
		// Small enough that the first half of the data below cannot fit, so a
		// flush is forced, and the engine's minimum is comfortably cleared.
		WriteBufferSize: Uint64(64 * 1024),
	})
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() {
		if err := db.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})

	expected := make(map[string][]byte, keyCount)
	write := func(i int) {
		key := fmt.Sprintf("key-%04d", i)
		value := bigValue(key, valueSize)
		if err := db.Set([]byte(key), value); err != nil {
			t.Fatalf("set %s: %v", key, err)
		}
		expected[key] = value
	}

	// The flushed half.
	for i := 0; i < keyCount/2; i++ {
		write(i)
	}

	if !hasSSTable(t, path) {
		t.Fatalf(
			"no SSTable was flushed by %d keys of %d bytes; the test is not covering the case it claims to",
			keyCount/2,
			valueSize,
		)
	}

	// The memtable half, written after the flush so it is still in memory.
	for i := keyCount / 2; i < keyCount; i++ {
		write(i)
	}

	return db, expected
}

// hasSSTable reports whether the store at the given path has flushed at least
// one SSTable to disk.
func hasSSTable(t *testing.T, path string) bool {
	t.Helper()

	found := false
	err := filepath.WalkDir(path, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !d.IsDir() && filepath.Ext(p) == ".sst" {
			found = true
		}

		return nil
	})
	if err != nil {
		t.Fatalf("walk %s: %v", path, err)
	}

	return found
}

// TestIterBorrowAndAppendAgreeWithValueAcrossMemtableAndSSTable is the central
// test of the two borrowing paths: every entry of a scan that spans a flushed
// SSTable and a live memtable must read back identically through all three value
// paths, in both directions and at both a small and a large value size.
func TestIterBorrowAndAppendAgreeWithValueAcrossMemtableAndSSTable(t *testing.T) {
	for _, size := range []struct {
		name      string
		keys      int
		valueSize int
	}{
		// Small enough to be stored inline, and numerous enough to span several
		// batches so that a refill happens mid-scan.
		{name: "small values", keys: 2000, valueSize: 48},
		// Large enough that the value bytes dominate, and that a batch hits its
		// retained-bytes cap rather than its entry count.
		{name: "large values", keys: 200, valueSize: 32 * 1024},
	} {
		for _, reverse := range []bool{false, true} {
			direction := "forward"
			if reverse {
				direction = "reverse"
			}

			t.Run(size.name+"/"+direction, func(t *testing.T) {
				db, expected := newSpanningDB(t, size.keys, size.valueSize)

				it, err := db.NewIter(IterOptions{Reverse: reverse})
				if err != nil {
					t.Fatalf("new iter: %v", err)
				}
				defer closeIter(t, it)

				views := collectValueViews(t, it)
				if len(views) != len(expected) {
					t.Fatalf("expected %d entries, got %d", len(expected), len(views))
				}
				assertValueViewsAgree(t, views)

				// Agreeing with each other is not enough: they could agree on
				// the wrong entry's bytes, which is exactly the failure a
				// mis-indexed batch would produce.
				for _, view := range views {
					if !bytes.Equal(view.borrowed, expected[view.key]) {
						t.Fatalf(
							"%s: borrowed %d bytes, expected the %d written",
							view.key,
							len(view.borrowed),
							len(expected[view.key]),
						)
					}
				}
			})
		}
	}
}

// TestIterBorrowValueOverTxnReadsBufferedWrites covers the other owner of value
// bytes: a transaction's own write buffer, which an iterator over a transaction
// merges with the store's data.
func TestIterBorrowValueOverTxnReadsBufferedWrites(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()

	if err := txn.Set([]byte("c"), []byte("buffered-c")); err != nil {
		t.Fatalf("set: %v", err)
	}
	if err := txn.Set([]byte("f"), []byte("buffered-f")); err != nil {
		t.Fatalf("set: %v", err)
	}

	it, err := txn.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	views := collectValueViews(t, it)
	assertValueViewsAgree(t, views)

	got := map[string]string{}
	for _, view := range views {
		got[view.key] = string(view.borrowed)
	}
	expected := map[string]string{
		"a": "va", "b": "vb", "c": "buffered-c", "d": "vd", "e": "ve", "f": "buffered-f",
	}
	if fmt.Sprint(expected) != fmt.Sprint(got) {
		t.Errorf("expected %v, got %v", expected, got)
	}
}

// TestIterBorrowValueReturnsCallbackErrorUnchanged pins the contract that
// corekv's ValueBorrower documents: the callback's error comes back as-is, not
// wrapped, so a caller can compare it against their own sentinel.
func TestIterBorrowValueReturnsCallbackErrorUnchanged(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	if _, err := it.Next(); err != nil {
		t.Fatalf("next: %v", err)
	}

	sentinel := errors.New("from the callback")
	err = it.BorrowValue(func([]byte) error {
		return sentinel
	})
	if err != sentinel { //nolint:errorlint // identity is the whole point here.
		t.Errorf("expected the callback's own error back, got %v", err)
	}
}

// TestIterBorrowAndAppendKeysOnly checks that both paths honour KeysOnly exactly
// as Value does - nil, and no error - rather than reading a value the iterator
// was told not to fetch.
func TestIterBorrowAndAppendKeysOnly(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{KeysOnly: true})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}

		calls := 0
		err = it.BorrowValue(func(value []byte) error {
			calls++
			if value != nil {
				t.Errorf("%s: expected nil under KeysOnly, got %q", it.Key(), value)
			}

			return nil
		})
		if err != nil {
			t.Fatalf("borrow value: %v", err)
		}
		if calls != 1 {
			t.Errorf("%s: callback called %d times, expected once", it.Key(), calls)
		}

		dst := []byte("prefix")
		dst, err = it.AppendValue(dst)
		if err != nil {
			t.Fatalf("append value: %v", err)
		}
		if string(dst) != "prefix" {
			t.Errorf("%s: expected dst unchanged under KeysOnly, got %q", it.Key(), dst)
		}
	}
}

// TestIterBorrowAndAppendAtInvalidPosition checks the three positions at which
// there is no entry to read - before the first Next, after exhaustion, and after
// a Reset - behave as Value does there.
func TestIterBorrowAndAppendAtInvalidPosition(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertNoValue := func(position string) {
		t.Helper()

		value, err := it.Value()
		if err != nil {
			t.Fatalf("%s: value: %v", position, err)
		}
		if value != nil {
			t.Errorf("%s: expected nil from Value, got %q", position, value)
		}

		calls := 0
		err = it.BorrowValue(func(borrowed []byte) error {
			calls++
			if borrowed != nil {
				t.Errorf("%s: expected nil from BorrowValue, got %q", position, borrowed)
			}

			return nil
		})
		if err != nil {
			t.Fatalf("%s: borrow value: %v", position, err)
		}
		if calls != 1 {
			t.Errorf("%s: callback called %d times, expected once", position, calls)
		}

		dst, err := it.AppendValue([]byte("prefix"))
		if err != nil {
			t.Fatalf("%s: append value: %v", position, err)
		}
		if string(dst) != "prefix" {
			t.Errorf("%s: expected dst unchanged, got %q", position, dst)
		}
	}

	assertNoValue("before the first Next")

	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}
	}
	assertNoValue("after exhaustion")

	it.Reset()
	assertNoValue("after a Reset")
}

// TestIterAppendValueGrowsAndPreservesAReusedBuffer checks the property the
// interface exists for: one buffer, re-used across a whole iteration, with
// whatever the caller put in front of the value left alone each time.
func TestIterAppendValueGrowsAndPreservesAReusedBuffer(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	// Deliberately too small for even one value plus the prefix, so the first
	// append has to grow it and the later ones can re-use the grown capacity.
	buf := make([]byte, 0, 1)

	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}

		key := string(it.Key())

		buf = append(buf[:0], "prefix/"...)
		buf, err = it.AppendValue(buf)
		if err != nil {
			t.Fatalf("append value: %v", err)
		}

		if expected := "prefix/v" + key; string(buf) != expected {
			t.Errorf("expected %q, got %q", expected, buf)
		}
	}
}
