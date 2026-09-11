package regolith

import (
	"errors"
	"fmt"
	"strings"
	"sync"
	"testing"
)

// TestSuccessPathAllocatesNothing pins the #cgo noescape/nocallback contract:
// a successful call across the FFI boundary must not put its out-param
// locals on the Go heap.
//
// This package calls 19 regolith_* functions with a real out-param local (a
// pointer to a stack variable the callee writes through, as opposed to a nil
// error pointer or an input-only value), and every one of them is exercised
// below: db.Set/Get/Has/Delete (regolith_db_set/get_borrowed/has/delete),
// txn.Set/Get/Has/Delete (regolith_txn_set/get_borrowed/has/delete),
// iter.Value and iter.BorrowValue (regolith_iter_batch_value), iter.Seek
// (regolith_iter_seek and the refill behind it, regolith_iter_next_batch),
// db.NewTxn+Commit (regolith_db_txn and regolith_txn_commit),
// db.NewIter+Close and txn.NewIter+Close (regolith_db_iter, regolith_txn_iter
// and regolith_iter_close), OpenWith+Close (regolith_db_open_with_options and
// regolith_db_close), and db.DropAll (regolith_db_drop_all).  Removing either
// the noescape line or the nocallback line of any of those 19 functions makes
// the corresponding local escape and the subtests that call it catch it; so
// does reintroducing a Go-heap allocated error detail on the success path.
//
// What an allocation count cannot catch: regolith_free_buf,
// regolith_release_value, regolith_error_free, regolith_error_message, and
// the nil-error calls this package makes to regolith_iter_reset,
// regolith_txn_discard and regolith_txn_free take no out-param local at all,
// so removing their directive cannot change what AllocsPerRun measures - a
// passing test proves nothing about those seven. Nor can it see a detail
// that is allocated and freed entirely on the Rust side (only a non-empty
// message, copied onto the Go heap through C.GoString, errors.go:61-63,
// shows up here), or prove that a function actually honours the directives
// it carries - keeps no Go pointer past the call, never calls back into Go -
// as opposed to merely never having been caught misbehaving.
func TestSuccessPathAllocatesNothing(t *testing.T) {
	db := newDB(t)

	// Hoisted out of the closures below: a captured local that only the
	// closure itself writes does not by itself force an allocation, but
	// keeping the key and value here matches how a caller would reuse a
	// buffer across many calls, which is the case this test is protecting.
	key := []byte("k")
	value := []byte("12345678")

	if err := db.Set(key, value); err != nil {
		t.Fatalf("seed: %v", err)
	}

	txn, err := db.NewTxn(false)
	if err != nil {
		t.Fatalf("new txn: %v", err)
	}
	defer txn.Discard()
	if err := txn.Set(key, value); err != nil {
		t.Fatalf("txn seed: %v", err)
	}

	// More entries than one batch holds, so the refill behind the seek below
	// walks a full batch rather than a short or empty one.
	for i := range 2 * batchEntries {
		if err := db.Set(fmt.Appendf(nil, "i%04d", i), value); err != nil {
			t.Fatalf("seed iter: %v", err)
		}
	}
	it, err := db.NewIter(IterOptions{})
	if err != nil {
		t.Fatalf("new iter: %v", err)
	}
	defer it.Close()
	if ok, err := it.Next(); !ok || err != nil {
		t.Fatalf("first next: ok=%v err=%v", ok, err)
	}
	seekKey := []byte("i0100")

	// A directory of its own, separate from db's: OpenWith+Close below opens
	// and closes a whole second store many times over, and must not disturb
	// the store every other case in this table reads from.
	openPath := t.TempDir()

	// A store of its own, separate from db's: DropAll below empties whatever
	// it points at, and db is the store db.Get, db.Has, db.Delete and the
	// iterator cases above read from and write to.
	dropDB := newDB(t)
	if err := dropDB.Set(key, value); err != nil {
		t.Fatalf("drop seed: %v", err)
	}

	cases := []struct {
		name string
		fn   func()
		want float64
		// runs overrides the default 1000 samples AllocsPerRun takes; zero
		// means the default.  Lowered for a case whose call does real
		// engine-side work per run, to keep the table's total cost down.
		runs int
	}{
		{"db.Set", func() {
			if err := db.Set(key, value); err != nil {
				t.Fatalf("set: %v", err)
			}
		}, 0, 0},
		{"db.Get", func() {
			// The one allocation on the read path: the C.GoBytes copy of
			// the borrowed value.  Its presence is what proves the count
			// is real, not an artifact of AllocsPerRun's own bookkeeping.
			if _, err := db.Get(key); err != nil {
				t.Fatalf("get: %v", err)
			}
		}, 1, 0},
		{"db.Has", func() {
			if _, err := db.Has(key); err != nil {
				t.Fatalf("has: %v", err)
			}
		}, 0, 0},
		{"db.Delete", func() {
			if err := db.Delete(key); err != nil {
				t.Fatalf("delete: %v", err)
			}
		}, 0, 0},
		{"txn.Set", func() {
			if err := txn.Set(key, value); err != nil {
				t.Fatalf("txn set: %v", err)
			}
		}, 0, 0},
		{"txn.Has", func() {
			if _, err := txn.Has(key); err != nil {
				t.Fatalf("txn has: %v", err)
			}
		}, 0, 0},
		{"txn.Get", func() {
			// The C.GoBytes copy, as for db.Get.  Placed before txn.Delete:
			// once that runs, the transaction buffers a tombstone for key
			// instead of the seeded value.
			if _, err := txn.Get(key); err != nil {
				t.Fatalf("txn get: %v", err)
			}
		}, 1, 0},
		{"txn.Delete", func() {
			if err := txn.Delete(key); err != nil {
				t.Fatalf("txn delete: %v", err)
			}
		}, 0, 0},
		{"iter.Value", func() {
			// The C.GoBytes copy, as for db.Get.
			if _, err := it.Value(); err != nil {
				t.Fatalf("value: %v", err)
			}
		}, 1, 0},
		{"iter.BorrowValue", func() {
			if err := it.BorrowValue(func([]byte) error { return nil }); err != nil {
				t.Fatalf("borrow value: %v", err)
			}
		}, 0, 0},
		{"iter.Seek", func() {
			// The one allocation is the Go copy of the batch frame the
			// refill behind the seek makes; the seek itself allocates
			// nothing.
			if ok, err := it.Seek(seekKey); !ok || err != nil {
				t.Fatalf("seek: ok=%v err=%v", ok, err)
			}
		}, 1, 0},
		{"db.NewTxn+Commit", func() {
			// The one allocation is the Txn handle itself.
			x, err := db.NewTxn(false)
			if err != nil {
				t.Fatalf("new txn: %v", err)
			}
			if err := x.Commit(); err != nil {
				t.Fatalf("commit: %v", err)
			}
		}, 1, 0},
		{"db.NewIter+Close", func() {
			// The Iter handle and its options.
			x, err := db.NewIter(IterOptions{})
			if err != nil {
				t.Fatalf("new iter: %v", err)
			}
			if err := x.Close(); err != nil {
				t.Fatalf("iter close: %v", err)
			}
		}, 2, 0},
		{"txn.NewIter+Close", func() {
			x, err := txn.NewIter(IterOptions{})
			if err != nil {
				t.Fatalf("txn new iter: %v", err)
			}
			if err := x.Close(); err != nil {
				t.Fatalf("iter close: %v", err)
			}
		}, 2, 0},
		{"OpenWith+Close", func() {
			// The DB handle (&DB{...}) plus the heap copy of the path:
			// []byte(path) does not escape (regolith.go:166), but a
			// non-escaping conversion only stays on the stack up to 32
			// bytes, and t.TempDir() paths run longer than that.
			x, err := OpenWith(openPath, Options{})
			if err != nil {
				t.Fatalf("open with: %v", err)
			}
			if err := x.Close(); err != nil {
				t.Fatalf("close: %v", err)
			}
		}, 2, 20},
		{"db.DropAll", func() {
			if err := dropDB.DropAll(); err != nil {
				t.Fatalf("drop all: %v", err)
			}
		}, 0, 20},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			runs := c.runs
			if runs == 0 {
				runs = 1000
			}
			if got := testing.AllocsPerRun(runs, c.fn); got != c.want {
				t.Errorf("%v allocs per run, want %v", got, c.want)
			}
		})
	}
}

// TestInvalidArgumentDetailIsPerCall drives many goroutines through Set and
// Delete at once, each rejected with a detail unique to that goroutine, to
// prove the detail arrives with the status of the one call that produced it
// rather than through any state a migrated goroutine or a reused OS thread
// could see stale or belonging to someone else.
func TestInvalidArgumentDetailIsPerCall(t *testing.T) {
	const (
		workers = 16
		rounds  = 100
		// regolith's default max_key_size.  If that default ever rises,
		// this test fails loudly: Delete below starts succeeding instead
		// of returning the detail the assertions check.
		maxKeySize = 8 << 20
	)

	db := newDB(t)
	ok := []byte("ok")

	var wg sync.WaitGroup
	for w := range workers {
		wg.Add(1)
		go func() {
			defer wg.Done()

			// One byte over the limit, with w more added so the length
			// itself marks which goroutine's detail this is.
			key := make([]byte, maxKeySize+1+w)

			for range rounds {
				if err := db.Set(ok, ok); err != nil {
					t.Errorf("set: %v", err)
					return
				}
				err := db.Delete(key)
				if !errors.Is(err, ErrInvalidArgument) {
					t.Errorf("delete: expected ErrInvalidArgument, got %v", err)
					return
				}
				want := fmt.Sprintf("key length %d ", len(key))
				if !strings.Contains(err.Error(), want) {
					t.Errorf("delete: expected detail to contain %q, got %v", want, err)
					return
				}
			}
		}()
	}

	wg.Wait()
}

// TestSetRejectsAnOversizedKeyWithDetail checks that the engine's own detail
// text crosses with exactly one "invalid argument: " prefix: the engine's
// Display already carries one, and the FFI layer must keep the reason rather
// than re-wrapping the whole message a second time.
func TestSetRejectsAnOversizedKeyWithDetail(t *testing.T) {
	db := newDB(t)

	key := make([]byte, 8<<20+1)
	err := db.Set(key, nil)
	if !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("expected ErrInvalidArgument, got %v", err)
	}
	if want := "key length 8388609 "; !strings.Contains(err.Error(), want) {
		t.Errorf("expected detail to contain %q, got %v", want, err)
	}
	if got := strings.Count(err.Error(), "invalid argument: "); got != 1 {
		t.Errorf("expected exactly one %q prefix, got %d in %v", "invalid argument: ", got, err)
	}
}
