// Package regolith is a Go binding for regolith, an embedded key-value engine
// written in Rust.  It is reached through the hand-written C ABI in `ffi/`,
// which is compiled into a static library and linked into the resulting Go
// binary.
//
// # Build requirement
//
// This is cgo over a Rust staticlib, so `make ffi` must have been run in the
// module directory before `go build`, otherwise the link fails with a missing
// `ffi/target/release/libregolith_ffi.a`.  A Rust toolchain is therefore needed
// to build anything that imports this package, and a plain `go get` will not
// produce a working one.  See the README.
//
// # Usage
//
// Open a store with [Open], read and write through [DB], iterate with
// [DB.NewIter], and group writes with [DB.NewTxn].  Transactions are optimistic
// with snapshot isolation, so a commit that lost a validation race returns
// [ErrConflict] and should be retried from a new transaction.
//
// # Limitations
//
// Handles have an ordering contract: an [Iter] created from a [Txn] must be
// closed before that transaction is committed or discarded, and every [Txn] and
// [Iter] must be resolved before [DB.Close].
//
// regolith's own `Options` are not exposed across the FFI yet, so a store is
// always opened with the engine defaults and [Open] takes no options parameter.
// This is a current limitation of the FFI layer, not a design choice.
package regolith

// #cgo CFLAGS: -I${SRCDIR}/ffi/include
// #cgo LDFLAGS: ${SRCDIR}/ffi/target/release/libregolith_ffi.a
// #include <stdlib.h>
// #include "regolith_ffi.h"
import "C"

import (
	"sync"
	"sync/atomic"
	"unsafe"
)

// DB is an open regolith store.
//
// It is safe for concurrent use.  Every [Txn] and [Iter] derived from it must be
// resolved before [DB.Close]; calls made after a close return [ErrClosed].
type DB struct {
	db *C.RegolithDb

	// `regolith_db_close` frees the store handle, even when it returns an error,
	// so every call that touches `db` has to be excluded from a concurrent
	// close or it would be a use-after-free.  `closeLk` does that, and `closed`
	// both makes the flag cheap to read from the iterator and transaction paths
	// (which do not touch `db`) and makes a second [DB.Close] a no-op instead of
	// a double free.
	closed  atomic.Bool
	closeLk sync.RWMutex
}

// IterOptions configures an iterator.  The zero value iterates the whole store
// forwards, with values.
type IterOptions struct {
	// Prefix restricts iteration to keys beginning with it, including the key
	// exactly equal to it.  It overrides Start and End.
	Prefix []byte

	// Start is the inclusive lower bound of the range.
	Start []byte

	// End is the exclusive upper bound of the range.
	End []byte

	// Reverse walks the range in descending key order.
	Reverse bool

	// KeysOnly skips value reads, making [Iter.Value] return nil.
	KeysOnly bool
}

// Open opens (or creates) a regolith store at the given path.
//
// There is no options parameter because regolith's `Options` are not yet
// exposed across the FFI boundary; the engine defaults are always used.
func Open(path string) (*DB, error) {
	cPath := []byte(path)

	var db *C.RegolithDb
	status := C.regolith_db_open(bytePtr(cPath), C.size_t(len(cPath)), &db)
	if err := statusToErr(status); err != nil {
		return nil, err
	}

	return &DB{db: db}, nil
}

// Get returns the value stored at the given key, or [ErrNotFound] if there is
// no entry for it.
//
// A key holding an empty value reads back as a nil slice.
func (db *DB) Get(key []byte) ([]byte, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return nil, ErrClosed
	}

	var val *C.uint8_t
	var valLen C.size_t
	var handle *C.RegolithValue
	status := C.regolith_db_get_borrowed(
		db.db,
		bytePtr(key), C.size_t(len(key)),
		&val, &valLen, &handle,
	)

	return copyBorrowed(val, valLen, handle, status)
}

// Has reports whether the given key has an entry.
func (db *DB) Has(key []byte) (bool, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return false, ErrClosed
	}

	var found C.uint8_t
	status := C.regolith_db_has(db.db, bytePtr(key), C.size_t(len(key)), &found)
	if err := statusToErr(status); err != nil {
		return false, err
	}

	return found != 0, nil
}

// Set writes the given value at the given key, overwriting any existing entry.
func (db *DB) Set(key, value []byte) error {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return ErrClosed
	}

	status := C.regolith_db_set(
		db.db,
		bytePtr(key), C.size_t(len(key)),
		bytePtr(value), C.size_t(len(value)),
	)

	return statusToErr(status)
}

// Delete removes the entry at the given key.  Deleting a key that has no entry
// is not an error.
func (db *DB) Delete(key []byte) error {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return ErrClosed
	}

	status := C.regolith_db_delete(db.db, bytePtr(key), C.size_t(len(key)))

	return statusToErr(status)
}

// DropAll deletes every entry in the store.  The store stays usable afterwards.
func (db *DB) DropAll() error {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return ErrClosed
	}

	return statusToErr(C.regolith_db_drop_all(db.db))
}

// Close closes the store and releases its handle.
//
// Every [Txn] and [Iter] derived from the store must already be resolved.
// Closing an already closed store is a no-op.
func (db *DB) Close() error {
	db.closeLk.Lock()
	defer db.closeLk.Unlock()
	if !db.closed.CompareAndSwap(false, true) {
		// The handle has already been freed by a previous call; closing again
		// would be a double free.
		return nil
	}

	// The handle is invalid after this call whatever it returns.
	return statusToErr(C.regolith_db_close(db.db))
}

// NewIter returns an iterator over a snapshot of the store taken now, so writes
// made after this call are invisible to it.
//
// The iterator must be closed with [Iter.Close].
func (db *DB) NewIter(opts IterOptions) (*Iter, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return nil, ErrClosed
	}

	// No transaction is needed here: the store-level iterator reads from a
	// snapshot taken by the FFI layer, which owns everything it needs.  There is
	// therefore nothing for the iterator to close besides itself.
	cOpts := newIterOptions(opts)
	defer cOpts.free()

	var it *C.RegolithIter
	status := C.regolith_db_iter(db.db, cOpts.opts, &it)
	if err := statusToErr(status); err != nil {
		return nil, err
	}

	return &Iter{db: db, i: it, keysOnly: opts.KeysOnly}, nil
}

// NewTxn begins a transaction, which must be resolved with [Txn.Commit] or
// [Txn.Discard].
//
// Transactions are optimistic with snapshot isolation.  regolith itself has no
// read-only mode, so `readOnly` is enforced by the FFI layer: writes on such a
// transaction return [ErrReadOnly].
func (db *DB) NewTxn(readOnly bool) (*Txn, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return nil, ErrClosed
	}

	var t *C.RegolithTxn
	status := C.regolith_db_txn(db.db, cBool(readOnly), &t)
	if err := statusToErr(status); err != nil {
		return nil, err
	}

	return &Txn{t: t, db: db}, nil
}

// Txn is a regolith transaction: optimistic, with snapshot isolation.  It reads
// its own buffered writes, and those writes become visible to others only on a
// successful [Txn.Commit].
//
// Reads and writes on one transaction are safe for concurrent use, but
// resolution is not concurrent with them.  Every [Iter] created from a
// transaction must be closed before it is resolved.
//
// Handle lifecycle: `regolith_txn_commit` and `regolith_txn_discard` resolve the
// transaction but leave the handle allocated, and `regolith_txn_free` must be
// called exactly once per handle.  Both [Txn.Commit] and [Txn.Discard] therefore
// free the handle themselves and mark the transaction resolved, and whichever of
// the two is called second does nothing.  That covers the usual pattern of a
// deferred `Discard` alongside a `Commit`, and it means a committed handle is
// freed as soon as it becomes useless.
type Txn struct {
	t  *C.RegolithTxn
	db *DB

	// resolved is true once the transaction has been committed or discarded and
	// its handle freed.  Guarded by `lk`.
	resolved bool

	// lk excludes resolution from concurrent use of the handle.  regolith
	// allows concurrent reads and writes on one transaction, but commit,
	// discard and free must not race with anything else on it.
	lk sync.RWMutex
}

// begin returns the error that should be returned instead of using the handle,
// if any, holding the read side of `lk` when it returns nil.
//
// The caller must RUnlock `lk` when it is done with the handle.
func (t *Txn) begin() error {
	t.lk.RLock()
	switch {
	case t.db.closed.Load():
		t.lk.RUnlock()
		return ErrClosed
	case t.resolved:
		t.lk.RUnlock()
		return ErrDiscarded
	default:
		return nil
	}
}

// Get returns the value stored at the given key as the transaction sees it,
// including its own buffered writes, or [ErrNotFound] if there is no entry.
//
// A key holding an empty value reads back as a nil slice.
func (t *Txn) Get(key []byte) ([]byte, error) {
	if err := t.begin(); err != nil {
		return nil, err
	}
	defer t.lk.RUnlock()

	var val *C.uint8_t
	var valLen C.size_t
	var handle *C.RegolithValue
	status := C.regolith_txn_get_borrowed(
		t.t,
		bytePtr(key), C.size_t(len(key)),
		&val, &valLen, &handle,
	)

	return copyBorrowed(val, valLen, handle, status)
}

// Has reports whether the given key has an entry as the transaction sees it.
func (t *Txn) Has(key []byte) (bool, error) {
	if err := t.begin(); err != nil {
		return false, err
	}
	defer t.lk.RUnlock()

	var found C.uint8_t
	status := C.regolith_txn_has(t.t, bytePtr(key), C.size_t(len(key)), &found)
	if err := statusToErr(status); err != nil {
		return false, err
	}

	return found != 0, nil
}

// Set buffers a write of the given value at the given key.  It returns
// [ErrReadOnly] on a read-only transaction.
func (t *Txn) Set(key, value []byte) error {
	if err := t.begin(); err != nil {
		return err
	}
	defer t.lk.RUnlock()

	status := C.regolith_txn_set(
		t.t,
		bytePtr(key), C.size_t(len(key)),
		bytePtr(value), C.size_t(len(value)),
	)

	return statusToErr(status)
}

// Delete buffers a delete of the given key.  Deleting a key that has no entry
// is not an error.  It returns [ErrReadOnly] on a read-only transaction.
func (t *Txn) Delete(key []byte) error {
	if err := t.begin(); err != nil {
		return err
	}
	defer t.lk.RUnlock()

	status := C.regolith_txn_delete(t.t, bytePtr(key), C.size_t(len(key)))

	return statusToErr(status)
}

// NewIter returns an iterator over the transaction: its buffered writes merged
// over the snapshot it began at.
//
// The transaction's write set is sampled when the iterator is first advanced,
// and again on every rebuild ([Iter.Reset] or [Iter.Seek]) - not when this
// function returns.  So a write buffered before the first [Iter.Next] is
// visible, a write buffered midway through an iteration is not, and a
// [Iter.Reset] picks up everything buffered so far.
//
// The iterator must be closed before the transaction is committed or discarded.
func (t *Txn) NewIter(opts IterOptions) (*Iter, error) {
	if err := t.begin(); err != nil {
		return nil, err
	}
	defer t.lk.RUnlock()

	cOpts := newIterOptions(opts)
	defer cOpts.free()

	var it *C.RegolithIter
	status := C.regolith_txn_iter(t.t, cOpts.opts, &it)
	if err := statusToErr(status); err != nil {
		return nil, err
	}

	return &Iter{db: t.db, i: it, keysOnly: opts.KeysOnly}, nil
}

// Commit validates and applies the transaction, then releases its handle.
//
// It returns [ErrConflict] when another writer touched a validated key first;
// the transaction is resolved either way, and the work should be retried from a
// new transaction.  A second resolution returns [ErrDiscarded].
func (t *Txn) Commit() error {
	t.lk.Lock()
	defer t.lk.Unlock()
	if t.resolved {
		return ErrDiscarded
	}
	if t.db.closed.Load() {
		return ErrClosed
	}

	// The error detail has to be read before any other FFI call, hence the
	// mapping happening before the handle is freed.
	err := statusToErr(C.regolith_txn_commit(t.t))

	t.resolved = true
	C.regolith_txn_free(t.t)

	return err
}

// Discard drops the transaction's buffered writes and releases its handle.
//
// It is a no-op on an already resolved transaction, so it is safe to defer it
// alongside a [Txn.Commit].
func (t *Txn) Discard() {
	t.lk.Lock()
	defer t.lk.Unlock()
	if t.resolved {
		return
	}

	// `regolith_txn_discard` is idempotent and `regolith_txn_free` discards an
	// unresolved transaction anyway, but discarding explicitly keeps the two
	// steps legible.  Both are called even if the store has been closed, as the
	// handle must be freed regardless and neither touches the store handle.
	C.regolith_txn_discard(t.t)
	t.resolved = true
	C.regolith_txn_free(t.t)
}

// bytePtr returns a pointer to the first byte of the given slice, or nil if it
// is empty.
//
// Handing Go memory to the FFI layer is legal under the cgo pointer rules
// because it does not retain input pointers past the call.  A nil pointer with
// a length of zero is how an empty or nil slice crosses, which the other side
// explicitly handles.
func bytePtr(b []byte) *C.uint8_t {
	if len(b) == 0 {
		return nil
	}

	return (*C.uint8_t)(unsafe.Pointer(&b[0]))
}

func cBool(b bool) C.uint8_t {
	if b {
		return 1
	}

	return 0
}

// takeBuf copies a buffer handed over by the FFI layer into Go memory and
// releases it, mapping the given status.
//
// Nothing is allocated on a non-OK status, so there is nothing to free then.
func takeBuf(ptr *C.uint8_t, length C.size_t, status C.int32_t) ([]byte, error) {
	if err := statusToErr(status); err != nil {
		return nil, err
	}
	if length == 0 {
		// A zero length buffer does not need freeing.
		return nil, nil
	}
	defer C.regolith_free_buf(ptr, length)

	return C.GoBytes(unsafe.Pointer(ptr), C.int(length)), nil
}

// copyBorrowed copies a value the FFI layer lent us into Go memory and then
// releases the handle that was keeping it readable.
//
// The point of the borrowed form is that nothing is copied on the Rust side:
// `val` points straight at the SSTable block or memtable arena chunk the engine
// read the value out of, and `handle` holds the reference count pinning it
// there.  The [C.GoBytes] below is the only copy in the read path.
//
// That pin is why the release is unconditional and why it happens here rather
// than being handed to the caller: a handle left alive keeps a block resident
// even after the block cache has evicted it, so holding one past the read that
// made it leaks engine memory.  Every path releases, including the error ones.
// A not-found read and a zero-length value both produce a nil handle, and
// releasing nil is a no-op, so one deferred call covers every case.
func copyBorrowed(
	ptr *C.uint8_t,
	length C.size_t,
	handle *C.RegolithValue,
	status C.int32_t,
) ([]byte, error) {
	// Releasing is a boundary crossing, and the FFI produces no handle for a
	// miss or for a present-but-empty value, so a nil check here is worth about
	// one crossing on every one of those reads - which is most of the cost of a
	// miss.  A nil handle is still safe to pass, this is purely the saving.
	if handle != nil {
		defer C.regolith_release_value(handle)
	}

	if err := statusToErr(status); err != nil {
		return nil, err
	}
	if length == 0 {
		// A key holding an empty value reads back as a nil slice.
		return nil, nil
	}

	return C.GoBytes(unsafe.Pointer(ptr), C.int(length)), nil
}

// iterOptions is an [IterOptions] in C memory.
//
// The struct and the byte ranges it points at are allocated with malloc rather
// than being a Go struct pointing into Go slices, because cgo forbids handing C
// a pointer to Go memory that itself contains Go pointers.
type iterOptions struct {
	opts *C.RegolithIterOptions
	bufs []unsafe.Pointer
}

func newIterOptions(opts IterOptions) *iterOptions {
	o := &iterOptions{
		opts: (*C.RegolithIterOptions)(C.calloc(1, C.size_t(unsafe.Sizeof(C.RegolithIterOptions{})))),
	}

	o.opts.prefix, o.opts.prefix_len = o.dup(opts.Prefix)
	o.opts.start, o.opts.start_len = o.dup(opts.Start)
	o.opts.end, o.opts.end_len = o.dup(opts.End)
	o.opts.reverse = cBool(opts.Reverse)
	o.opts.keys_only = cBool(opts.KeysOnly)

	return o
}

// dup copies the given slice into C memory, to be released by [iterOptions.free].
func (o *iterOptions) dup(b []byte) (*C.uint8_t, C.size_t) {
	if len(b) == 0 {
		return nil, 0
	}

	ptr := C.CBytes(b)
	o.bufs = append(o.bufs, ptr)

	return (*C.uint8_t)(ptr), C.size_t(len(b))
}

// free releases the options struct.  The FFI layer only needs it for the
// duration of the iterator-creating call.
func (o *iterOptions) free() {
	for _, ptr := range o.bufs {
		C.free(ptr)
	}
	C.free(unsafe.Pointer(o.opts))
}
