package regolith

import (
	"bytes"
	"errors"
	"fmt"
	"os"
	"slices"
	"sync"
	"testing"
)

// newDB opens a store in a temporary directory that is removed, along with the
// store, when the test finishes.
func newDB(t *testing.T) *DB {
	t.Helper()

	db, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	t.Cleanup(func() {
		if err := db.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})

	return db
}

// seed writes the keys a, b, c, d and e with values prefixed by `v`.
func seed(t *testing.T, db *DB) {
	t.Helper()

	for _, key := range []string{"a", "b", "c", "d", "e"} {
		if err := db.Set([]byte(key), []byte("v"+key)); err != nil {
			t.Fatalf("set %s: %v", key, err)
		}
	}
}

// drain walks the given iterator to exhaustion, returning the keys it yielded
// and, where values were requested, asserting that each value matches its key.
func drain(t *testing.T, it *Iter, withValues bool) []string {
	t.Helper()

	keys := []string{}
	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			return keys
		}

		key := it.Key()
		keys = append(keys, string(key))

		value, err := it.Value()
		if err != nil {
			t.Fatalf("value: %v", err)
		}
		switch {
		case !withValues:
			if value != nil {
				t.Errorf("expected no value for %s, got %s", key, value)
			}
		case !bytes.Equal(value, append([]byte("v"), key...)):
			t.Errorf("unexpected value for %s: %s", key, value)
		}
	}
}

func drainKeys(t *testing.T, it *Iter) []string {
	t.Helper()

	keys := []string{}
	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			return keys
		}
		keys = append(keys, string(it.Key()))
	}
}

func closeIter(t *testing.T, it *Iter) {
	t.Helper()

	if err := it.Close(); err != nil {
		t.Errorf("iterator close: %v", err)
	}
}

func assertKeys(t *testing.T, expected, actual []string) {
	t.Helper()

	if fmt.Sprint(expected) != fmt.Sprint(actual) {
		t.Errorf("expected keys %v, got %v", expected, actual)
	}
}

func TestOpenAndClose(t *testing.T) {
	db, err := Open(t.TempDir())
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	// Closing again must not free the handle a second time.
	if err := db.Close(); err != nil {
		t.Fatalf("second close: %v", err)
	}
}

// TestOpenUnusablePath covers the failure path out of the FFI layer, including
// the error detail read back from it.
func TestOpenUnusablePath(t *testing.T) {
	path := t.TempDir() + "/a-file-not-a-directory"
	if err := os.WriteFile(path, []byte("not a store"), 0o600); err != nil {
		t.Fatalf("write file: %v", err)
	}

	_, err := Open(path)
	if err == nil {
		t.Fatal("expected an error opening a path that is not a directory")
	}
}

func TestSetGetHasDelete(t *testing.T) {
	db := newDB(t)

	if err := db.Set([]byte("k"), []byte("v")); err != nil {
		t.Fatalf("set: %v", err)
	}

	value, err := db.Get([]byte("k"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "v" {
		t.Errorf("expected v, got %s", value)
	}

	has, err := db.Has([]byte("k"))
	if err != nil {
		t.Fatalf("has: %v", err)
	}
	if !has {
		t.Error("expected k to be found")
	}

	// Overwriting.
	if err := db.Set([]byte("k"), []byte("v2")); err != nil {
		t.Fatalf("set: %v", err)
	}
	value, err = db.Get([]byte("k"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "v2" {
		t.Errorf("expected v2, got %s", value)
	}

	if err := db.Delete([]byte("k")); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if _, err := db.Get([]byte("k")); !errors.Is(err, ErrNotFound) {
		t.Errorf("expected ErrNotFound, got %v", err)
	}
	has, err = db.Has([]byte("k"))
	if err != nil {
		t.Fatalf("has: %v", err)
	}
	if has {
		t.Error("expected k to be gone")
	}

	// Deleting a key that is not there is not an error.
	if err := db.Delete([]byte("k")); err != nil {
		t.Errorf("delete of an absent key: %v", err)
	}

	// An empty value reads back as nil.
	if err := db.Set([]byte("empty"), nil); err != nil {
		t.Fatalf("set: %v", err)
	}
	value, err = db.Get([]byte("empty"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if value != nil {
		t.Errorf("expected a nil value, got %q", value)
	}
}

func TestGetMissingReturnsNotFound(t *testing.T) {
	db := newDB(t)

	value, err := db.Get([]byte("nope"))
	if !errors.Is(err, ErrNotFound) {
		t.Errorf("expected ErrNotFound, got %v", err)
	}
	if value != nil {
		t.Errorf("expected a nil value, got %s", value)
	}
}

func TestOperationsAfterCloseReturnErrClosed(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	iter, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}

	// The ordering contract: iterators and transactions go first.
	if err := iter.Close(); err != nil {
		t.Fatalf("iterator close: %v", err)
	}
	txn.Discard()

	if err := db.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}

	if _, err := db.Get([]byte("a")); !errors.Is(err, ErrClosed) {
		t.Errorf("get: expected ErrClosed, got %v", err)
	}
	if _, err := db.Has([]byte("a")); !errors.Is(err, ErrClosed) {
		t.Errorf("has: expected ErrClosed, got %v", err)
	}
	if err := db.Set([]byte("a"), []byte("va")); !errors.Is(err, ErrClosed) {
		t.Errorf("set: expected ErrClosed, got %v", err)
	}
	if err := db.Delete([]byte("a")); !errors.Is(err, ErrClosed) {
		t.Errorf("delete: expected ErrClosed, got %v", err)
	}
	if err := db.DropAll(); !errors.Is(err, ErrClosed) {
		t.Errorf("drop all: expected ErrClosed, got %v", err)
	}
	if _, err := db.NewIter(IterOptions{}); !errors.Is(err, ErrClosed) {
		t.Errorf("new iter: expected ErrClosed, got %v", err)
	}

	// A transaction cannot be created after the close at all: the failure is
	// reported here rather than deferred to every call on the transaction.
	if _, err := db.NewTxn(false); !errors.Is(err, ErrClosed) {
		t.Errorf("new txn: expected ErrClosed, got %v", err)
	}

	// And so does the store's own Close, which must stay a no-op.
	if err := db.Close(); err != nil {
		t.Fatalf("second close: %v", err)
	}
}

func TestIteratorForward(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, true))
}

func TestIteratorReverse(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{Reverse: true})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"e", "d", "c", "b", "a"}, drain(t, it, true))
}

func TestIteratorPrefix(t *testing.T) {
	db := newDB(t)
	for _, key := range []string{"ab", "ba", "bb", "bc", "ca"} {
		if err := db.Set([]byte(key), []byte("v"+key)); err != nil {
			t.Fatalf("set: %v", err)
		}
	}

	it, err := db.NewIter(IterOptions{Prefix: []byte("b")})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"ba", "bb", "bc"}, drain(t, it, true))
}

func TestIteratorStartEndIsEndExclusive(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{
		Start: []byte("b"),
		End:   []byte("d"),
	})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"b", "c"}, drain(t, it, true))
}

func TestIteratorStartEndReverse(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{
		Start:   []byte("b"),
		End:     []byte("d"),
		Reverse: true,
	})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"c", "b"}, drain(t, it, true))
}

func TestIteratorSeek(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}

	// An exact match, then an inexact one, which lands on the next key up.
	for target, expected := range map[string]string{"c": "c", "bb": "c"} {
		found, err := it.Seek([]byte(target))
		if err != nil {
			t.Fatalf("seek: %v", err)
		}
		if !found {
			t.Fatalf("expected seek to %s to find something", target)
		}
		if string(it.Key()) != expected {
			t.Errorf("expected seek to %s to land on %s, got %s", target, expected, it.Key())
		}
	}

	// Seeking past the end of the data finds nothing.
	found, err := it.Seek([]byte("z"))
	if err != nil {
		t.Fatalf("seek: %v", err)
	}
	if found {
		t.Errorf("expected seek past the end to find nothing, got %s", it.Key())
	}
	// And the iterator is then at an invalid location.
	if it.Key() != nil {
		t.Errorf("expected a nil key at an invalid location, got %s", it.Key())
	}
	value, err := it.Value()
	if err != nil {
		t.Fatalf("value: %v", err)
	}
	if value != nil {
		t.Errorf("expected a nil value at an invalid location, got %s", value)
	}
	closeIter(t, it)

	// A reverse seek lands on the greatest key at or below the target.
	it, err = db.NewIter(IterOptions{Reverse: true})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	found, err = it.Seek([]byte("bb"))
	if err != nil {
		t.Fatalf("seek: %v", err)
	}
	if !found || string(it.Key()) != "b" {
		t.Errorf("expected a reverse seek to bb to land on b, got %s (%t)", it.Key(), found)
	}
	assertKeys(t, []string{"a"}, drain(t, it, true))
}

func TestIteratorSeekIsClampedToTheRange(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{Start: []byte("c"), End: []byte("e")})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	// Below `start`, so clamped up to it.
	found, err := it.Seek([]byte("a"))
	if err != nil {
		t.Fatalf("seek: %v", err)
	}
	if !found || string(it.Key()) != "c" {
		t.Errorf("expected a clamped seek to land on c, got %s (%t)", it.Key(), found)
	}

	// At `end`, which is exclusive, so out of range.
	found, err = it.Seek([]byte("e"))
	if err != nil {
		t.Fatalf("seek: %v", err)
	}
	if found {
		t.Errorf("expected a seek to the exclusive end to find nothing, got %s", it.Key())
	}
}

func TestIteratorReset(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, true))

	// Without a reset the iterator stays exhausted.
	assertKeys(t, []string{}, drain(t, it, true))

	it.Reset()
	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, true))

	// A reset part way through is just as good.
	if _, err := it.Next(); err != nil {
		t.Fatalf("next: %v", err)
	}
	it.Reset()
	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, true))
}

func TestIteratorKeysOnly(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{KeysOnly: true})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, false))
}

func TestIteratorSeesASnapshotFromItsCreation(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)

	if err := db.Set([]byte("f"), []byte("vf")); err != nil {
		t.Fatalf("set: %v", err)
	}

	assertKeys(t, []string{"a", "b", "c", "d", "e"}, drain(t, it, true))
}

func TestIteratorDoubleCloseIsSafe(t *testing.T) {
	db := newDB(t)

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	closeIter(t, it)
	closeIter(t, it)
}

func TestTxnCommitIsVisible(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()

	if err := txn.Set([]byte("f"), []byte("vf")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	if err := txn.Delete([]byte("a")); err != nil {
		t.Fatalf("txn delete: %v", err)
	}

	// The transaction sees its own writes, the store does not, yet.
	value, err := txn.Get([]byte("f"))
	if err != nil {
		t.Fatalf("txn get: %v", err)
	}
	if string(value) != "vf" {
		t.Errorf("expected vf, got %s", value)
	}
	if _, err := txn.Get([]byte("a")); !errors.Is(err, ErrNotFound) {
		t.Errorf("txn get: expected ErrNotFound, got %v", err)
	}
	if _, err := db.Get([]byte("f")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get: expected ErrNotFound, got %v", err)
	}

	if err := txn.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}

	value, err = db.Get([]byte("f"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "vf" {
		t.Errorf("expected vf, got %s", value)
	}
	if _, err := db.Get([]byte("a")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get: expected ErrNotFound, got %v", err)
	}

	// A committed transaction is resolved, and the deferred discard above must
	// not free its handle a second time.
	if err := txn.Commit(); !errors.Is(err, ErrDiscarded) {
		t.Errorf("second commit: expected ErrDiscarded, got %v", err)
	}
	if _, err := txn.Get([]byte("f")); !errors.Is(err, ErrDiscarded) {
		t.Errorf("get after commit: expected ErrDiscarded, got %v", err)
	}
}

func TestTxnDiscardIsInvisible(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	if err := txn.Set([]byte("f"), []byte("vf")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	txn.Discard()
	// Discarding twice must not free the handle twice.
	txn.Discard()

	if _, err := db.Get([]byte("f")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get: expected ErrNotFound, got %v", err)
	}
	if err := txn.Set([]byte("g"), []byte("vg")); !errors.Is(err, ErrDiscarded) {
		t.Errorf("set after discard: expected ErrDiscarded, got %v", err)
	}
	if err := txn.Commit(); !errors.Is(err, ErrDiscarded) {
		t.Errorf("commit after discard: expected ErrDiscarded, got %v", err)
	}
}

func TestTxnConflict(t *testing.T) {
	db := newDB(t)
	if err := db.Set([]byte("k"), []byte("v0")); err != nil {
		t.Fatalf("set: %v", err)
	}

	// Both transactions begin before either commits, and both write the same
	// key, so snapshot-isolation validation must reject the second.
	first, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer first.Discard()
	second, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer second.Discard()

	if err := first.Set([]byte("k"), []byte("v1")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	if err := second.Set([]byte("k"), []byte("v2")); err != nil {
		t.Fatalf("txn set: %v", err)
	}

	if err := first.Commit(); err != nil {
		t.Fatalf("first commit: %v", err)
	}
	if err := second.Commit(); !errors.Is(err, ErrConflict) {
		t.Errorf("second commit: expected ErrConflict, got %v", err)
	}

	value, err := db.Get([]byte("k"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "v1" {
		t.Errorf("expected v1, got %s", value)
	}
}

func TestReadOnlyTxnRejectsWrites(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(true)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()

	if err := txn.Set([]byte("f"), []byte("vf")); !errors.Is(err, ErrReadOnly) {
		t.Errorf("set: expected ErrReadOnly, got %v", err)
	}
	if err := txn.Delete([]byte("a")); !errors.Is(err, ErrReadOnly) {
		t.Errorf("delete: expected ErrReadOnly, got %v", err)
	}

	// Reads still work.
	value, err := txn.Get([]byte("a"))
	if err != nil {
		t.Fatalf("get: %v", err)
	}
	if string(value) != "va" {
		t.Errorf("expected va, got %s", value)
	}
	has, err := txn.Has([]byte("a"))
	if err != nil {
		t.Fatalf("has: %v", err)
	}
	if !has {
		t.Error("expected a to be found")
	}
}

func TestTxnIteratorSeesBufferedWrites(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()

	// An insert, an overwrite and a delete, all uncommitted.
	if err := txn.Set([]byte("bb"), []byte("vbb")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	if err := txn.Set([]byte("c"), []byte("vc2")); err != nil {
		t.Fatalf("txn set: %v", err)
	}
	if err := txn.Delete([]byte("d")); err != nil {
		t.Fatalf("txn delete: %v", err)
	}

	it, err := txn.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}

	keys := []string{}
	for {
		hasNext, err := it.Next()
		if err != nil {
			t.Fatalf("next: %v", err)
		}
		if !hasNext {
			break
		}
		keys = append(keys, string(it.Key()))
		if string(it.Key()) == "c" {
			value, err := it.Value()
			if err != nil {
				t.Fatalf("value: %v", err)
			}
			if string(value) != "vc2" {
				t.Errorf("expected the buffered value vc2, got %s", value)
			}
		}
	}
	assertKeys(t, []string{"a", "b", "bb", "c", "e"}, keys)

	// Reset re-walks the same merged view.
	it.Reset()
	assertKeys(t, []string{"a", "b", "bb", "c", "e"}, drainKeys(t, it))
	closeIter(t, it)

	// Reverse, bounded and prefixed transaction iteration.
	it, err = txn.NewIter(IterOptions{Reverse: true})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	assertKeys(t, []string{"e", "c", "bb", "b", "a"}, drainKeys(t, it))
	closeIter(t, it)

	it, err = txn.NewIter(IterOptions{Start: []byte("b"), End: []byte("c")})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	assertKeys(t, []string{"b", "bb"}, drainKeys(t, it))
	closeIter(t, it)

	it, err = txn.NewIter(IterOptions{Prefix: []byte("b")})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	assertKeys(t, []string{"b", "bb"}, drainKeys(t, it))
	closeIter(t, it)
}

func TestDropAll(t *testing.T) {
	db := newDB(t)
	seed(t, db)

	if err := db.DropAll(); err != nil {
		t.Fatalf("drop all: %v", err)
	}

	if _, err := db.Get([]byte("a")); !errors.Is(err, ErrNotFound) {
		t.Errorf("get: expected ErrNotFound, got %v", err)
	}

	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer closeIter(t, it)
	assertKeys(t, []string{}, drainKeys(t, it))

	// The store is still usable.
	if err := db.Set([]byte("a"), []byte("va")); err != nil {
		t.Fatalf("set: %v", err)
	}
}

// TestLifecycle hammers the store through a few thousand set/get/iterate cycles
// in order to smoke out handle lifecycle and leak bugs that the unit tests,
// each of which makes only a handful of FFI calls, would never reach.
func TestLifecycle(t *testing.T) {
	const count = 3000

	db := newDB(t)

	for i := range count {
		key := []byte(fmt.Sprintf("key:%06d", i))
		if err := db.Set(key, bytes.Repeat([]byte("v"), 64)); err != nil {
			t.Fatalf("set: %v", err)
		}

		value, err := db.Get(key)
		if err != nil {
			t.Fatalf("get: %v", err)
		}
		if len(value) != 64 {
			t.Fatalf("expected a 64 byte value, got %d", len(value))
		}

		// Iterate a small window, and every so often the whole store, through
		// both a transaction and the store itself.
		opts := IterOptions{
			Start: key,
			End:   []byte(fmt.Sprintf("key:%06d", i+10)),
		}
		if i%500 == 0 {
			opts = IterOptions{}
		}

		it, err := db.NewIter(opts)
		if err != nil {
			t.Fatalf("new iter: %v", err)
		}
		drainKeys(t, it)
		it.Reset()
		drainKeys(t, it)
		closeIter(t, it)

		txn, err := db.NewTxn(false)
		if err != nil {
			t.Fatalf("new txn: %v", err)
		}
		if err := txn.Set(key, []byte("txn")); err != nil {
			t.Fatalf("txn set: %v", err)
		}
		txnIter, err := txn.NewIter(opts)
		if err != nil {
			t.Fatalf("txn new iter: %v", err)
		}
		drainKeys(t, txnIter)
		closeIter(t, txnIter)

		if i%2 == 0 {
			if err := txn.Commit(); err != nil {
				t.Fatalf("commit: %v", err)
			}
		}
		txn.Discard()
	}
}

// TestConcurrentUse drives one store from several goroutines at once, which is
// the only way to exercise the close lock and the transaction lock under `-race`.
func TestConcurrentUse(t *testing.T) {
	const (
		workers = 8
		rounds  = 200
	)

	db := newDB(t)
	seed(t, db)

	var wg sync.WaitGroup
	for w := range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()

			prefix := fmt.Sprintf("w%d:", w)
			for i := range rounds {
				key := []byte(fmt.Sprintf("%s%04d", prefix, i))
				if err := db.Set(key, []byte("v")); err != nil {
					t.Errorf("set: %v", err)
					return
				}
				if _, err := db.Get(key); err != nil {
					t.Errorf("get: %v", err)
					return
				}

				it, err := db.NewIter(IterOptions{Prefix: []byte(prefix)})
				if err != nil {
					t.Errorf("new iter: %v", err)
					return
				}
				for {
					hasNext, err := it.Next()
					if err != nil {
						t.Errorf("next: %v", err)
						break
					}
					if !hasNext {
						break
					}
					it.Key()
				}
				if err := it.Close(); err != nil {
					t.Errorf("iterator close: %v", err)
					return
				}

				txn, err := db.NewTxn(false)
				if err != nil {
					t.Errorf("new txn: %v", err)
					return
				}
				if err := txn.Set(key, []byte("txn")); err != nil {
					t.Errorf("txn set: %v", err)
					txn.Discard()
					return
				}
				// A conflict here is legitimate: the workers share no keys but
				// they do share the store, and a lost validation race is the
				// documented outcome.
				if err := txn.Commit(); err != nil && !errors.Is(err, ErrConflict) {
					t.Errorf("commit: %v", err)
				}
				txn.Discard()
			}
		}()
	}

	wg.Wait()
}

// TestTxnIterSamplesWritesOnRebuild pins when a transaction iterator picks up
// the transaction's buffered writes: on first advance and on every rebuild,
// rather than when NewIter returns.  This was documented the other way round
// before it was measured, so it is asserted here rather than described.
func TestTxnIterSamplesWritesOnRebuild(t *testing.T) {
	db := newDB(t)

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()

	if err := txn.Set([]byte("a"), []byte("1")); err != nil {
		t.Fatalf("set a: %v", err)
	}

	it, err := txn.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer it.Close()

	// Buffered after NewIter but before the first Next: sampled, so visible.
	if err := txn.Set([]byte("b"), []byte("2")); err != nil {
		t.Fatalf("set b: %v", err)
	}

	if keys := drainKeys(t, it); !slices.Equal(keys, []string{"a", "b"}) {
		t.Errorf("write buffered before the first Next should be visible, got %v", keys)
	}

	// Buffered midway through an iteration: not visible until a rebuild.
	it.Reset()
	ok, err := it.Next()
	if err != nil || !ok {
		t.Fatalf("first Next after reset: ok=%v err=%v", ok, err)
	}
	if err := txn.Set([]byte("c"), []byte("3")); err != nil {
		t.Fatalf("set c: %v", err)
	}
	if keys := drainKeys(t, it); slices.Equal(keys, []string{"b", "c"}) {
		t.Errorf("write buffered mid-iteration should not appear, got %v", keys)
	}

	// A rebuild re-samples, so it appears now.
	it.Reset()
	if keys := drainKeys(t, it); !slices.Equal(keys, []string{"a", "b", "c"}) {
		t.Errorf("Reset should re-sample buffered writes, got %v", keys)
	}
}
