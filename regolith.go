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
// with snapshot isolation by default, so a commit that lost a validation race
// returns [ErrConflict] and should be retried from a new transaction.  A store
// opened with [OpenWith] can choose another level; see [Options.Isolation].
//
// # Limitations
//
// Handles have an ordering contract: an [Iter] created from a [Txn] must be
// closed before that transaction is committed or discarded, and every [Txn] and
// [Iter] must be resolved before [DB.Close].
//
// # Engine options
//
// [Open] uses regolith's defaults.  [OpenWith] takes an [Options] whose zero
// value means exactly the same thing, and only the fields explicitly set on it
// are applied; see [Options] for why that is not the same as "fields left at
// zero".  Only a small subset of regolith's own `Options` is reachable so far -
// the rest of that type is mostly trait-object hooks with no C representation.
package regolith

// #cgo CFLAGS: -I${SRCDIR}/ffi/include
// #cgo LDFLAGS: ${SRCDIR}/ffi/target/release/libregolith_ffi.a
//
// /*
//  * Every C function this package calls neither keeps a Go pointer past the
//  * call nor calls back into Go, which is what the two directives below
//  * assert per function.  With them the out-param locals below stay on the
//  * goroutine's stack instead of escaping to the heap on every call.  cgo
//  * rejects a directive naming a function this package does not call, so
//  * the list has to track the calls exactly.
//  */
// #cgo noescape regolith_db_close
// #cgo nocallback regolith_db_close
// #cgo noescape regolith_db_delete
// #cgo nocallback regolith_db_delete
// #cgo noescape regolith_db_drop_all
// #cgo nocallback regolith_db_drop_all
// #cgo noescape regolith_db_get_borrowed
// #cgo nocallback regolith_db_get_borrowed
// #cgo noescape regolith_db_has
// #cgo nocallback regolith_db_has
// #cgo noescape regolith_db_iter
// #cgo nocallback regolith_db_iter
// #cgo noescape regolith_db_open_with_options
// #cgo nocallback regolith_db_open_with_options
// #cgo noescape regolith_db_set
// #cgo nocallback regolith_db_set
// #cgo noescape regolith_db_txn
// #cgo nocallback regolith_db_txn
// #cgo noescape regolith_error_free
// #cgo nocallback regolith_error_free
// #cgo noescape regolith_error_message
// #cgo nocallback regolith_error_message
// #cgo noescape regolith_free_buf
// #cgo nocallback regolith_free_buf
// #cgo noescape regolith_iter_batch_value
// #cgo nocallback regolith_iter_batch_value
// #cgo noescape regolith_iter_close
// #cgo nocallback regolith_iter_close
// #cgo noescape regolith_iter_next_batch
// #cgo nocallback regolith_iter_next_batch
// #cgo noescape regolith_iter_reset
// #cgo nocallback regolith_iter_reset
// #cgo noescape regolith_iter_seek
// #cgo nocallback regolith_iter_seek
// #cgo noescape regolith_release_value
// #cgo nocallback regolith_release_value
// #cgo noescape regolith_txn_commit
// #cgo nocallback regolith_txn_commit
// #cgo noescape regolith_txn_delete
// #cgo nocallback regolith_txn_delete
// #cgo noescape regolith_txn_discard
// #cgo nocallback regolith_txn_discard
// #cgo noescape regolith_txn_free
// #cgo nocallback regolith_txn_free
// #cgo noescape regolith_txn_get_borrowed
// #cgo nocallback regolith_txn_get_borrowed
// #cgo noescape regolith_txn_has
// #cgo nocallback regolith_txn_has
// #cgo noescape regolith_txn_iter
// #cgo nocallback regolith_txn_iter
// #cgo noescape regolith_txn_set
// #cgo nocallback regolith_txn_set
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

// Open opens (or creates) a regolith store at the given path, with regolith's
// default engine options.
//
// It is [OpenWith] with a zero [Options].
func Open(path string) (*DB, error) {
	return OpenWith(path, Options{})
}

// OpenWith opens (or creates) a regolith store at the given path with the given
// engine options.
//
// A zero [Options] means the engine defaults, so `OpenWith(path, Options{})` is
// [Open].  Only the fields actually set on the options are applied; everything
// else is left wherever regolith's defaults put it.
//
// An invalid setting is rejected rather than clamped, with an error naming the
// offending field.  regolith validates its options before touching the
// filesystem, so a rejected call creates nothing.
func OpenWith(path string, opts Options) (*DB, error) {
	cPath := []byte(path)

	// `cOpts` is a Go value holding no Go pointers - every field is an integer -
	// so handing C a pointer to it is legal under the cgo pointer rules without
	// the malloc dance that `iterOptions` needs.  The FFI layer only reads it
	// for the duration of the call.
	cOpts, err := opts.toC()
	if err != nil {
		return nil, err
	}

	var db *C.RegolithDb
	var cerr *C.RegolithError
	status := C.regolith_db_open_with_options(
		bytePtr(cPath), C.size_t(len(cPath)),
		&cOpts, &db, &cerr,
	)
	if err := statusToErr(status, cerr); err != nil {
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
	var cerr *C.RegolithError
	status := C.regolith_db_get_borrowed(
		db.db,
		bytePtr(key), C.size_t(len(key)),
		&val, &valLen, &handle, &cerr,
	)

	return copyBorrowed(val, valLen, handle, status, cerr)
}

// Has reports whether the given key has an entry.
func (db *DB) Has(key []byte) (bool, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return false, ErrClosed
	}

	var found C.uint8_t
	var cerr *C.RegolithError
	status := C.regolith_db_has(db.db, bytePtr(key), C.size_t(len(key)), &found, &cerr)
	if err := statusToErr(status, cerr); err != nil {
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

	var cerr *C.RegolithError
	status := C.regolith_db_set(
		db.db,
		bytePtr(key), C.size_t(len(key)),
		bytePtr(value), C.size_t(len(value)),
		&cerr,
	)

	return statusToErr(status, cerr)
}

// Delete removes the entry at the given key.  Deleting a key that has no entry
// is not an error.
func (db *DB) Delete(key []byte) error {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return ErrClosed
	}

	var cerr *C.RegolithError
	status := C.regolith_db_delete(db.db, bytePtr(key), C.size_t(len(key)), &cerr)

	return statusToErr(status, cerr)
}

// DropAll deletes every entry in the store.  The store stays usable afterwards.
func (db *DB) DropAll() error {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return ErrClosed
	}

	var cerr *C.RegolithError

	return statusToErr(C.regolith_db_drop_all(db.db, &cerr), cerr)
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

	var cerr *C.RegolithError

	// The handle is invalid after this call whatever it returns.
	return statusToErr(C.regolith_db_close(db.db, &cerr), cerr)
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
	var cerr *C.RegolithError
	status := C.regolith_db_iter(db.db, cOpts.opts, &it, &cerr)
	if err := statusToErr(status, cerr); err != nil {
		return nil, err
	}

	return &Iter{db: db, i: it, keysOnly: opts.KeysOnly}, nil
}

// NewTxn begins a transaction, which must be resolved with [Txn.Commit] or
// [Txn.Discard].
//
// Transactions are optimistic, at the isolation level the store was opened with
// ([Options.Isolation], snapshot isolation unless it was set).  regolith itself
// has no read-only mode, so `readOnly` is enforced by the FFI layer: writes on
// such a transaction return [ErrReadOnly].
func (db *DB) NewTxn(readOnly bool) (*Txn, error) {
	db.closeLk.RLock()
	defer db.closeLk.RUnlock()
	if db.closed.Load() {
		return nil, ErrClosed
	}

	var t *C.RegolithTxn
	var cerr *C.RegolithError
	status := C.regolith_db_txn(db.db, cBool(readOnly), &t, &cerr)
	if err := statusToErr(status, cerr); err != nil {
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
	var cerr *C.RegolithError
	status := C.regolith_txn_get_borrowed(
		t.t,
		bytePtr(key), C.size_t(len(key)),
		&val, &valLen, &handle, &cerr,
	)

	return copyBorrowed(val, valLen, handle, status, cerr)
}

// Has reports whether the given key has an entry as the transaction sees it.
func (t *Txn) Has(key []byte) (bool, error) {
	if err := t.begin(); err != nil {
		return false, err
	}
	defer t.lk.RUnlock()

	var found C.uint8_t
	var cerr *C.RegolithError
	status := C.regolith_txn_has(t.t, bytePtr(key), C.size_t(len(key)), &found, &cerr)
	if err := statusToErr(status, cerr); err != nil {
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

	var cerr *C.RegolithError
	status := C.regolith_txn_set(
		t.t,
		bytePtr(key), C.size_t(len(key)),
		bytePtr(value), C.size_t(len(value)),
		&cerr,
	)

	return statusToErr(status, cerr)
}

// Delete buffers a delete of the given key.  Deleting a key that has no entry
// is not an error.  It returns [ErrReadOnly] on a read-only transaction.
func (t *Txn) Delete(key []byte) error {
	if err := t.begin(); err != nil {
		return err
	}
	defer t.lk.RUnlock()

	var cerr *C.RegolithError
	status := C.regolith_txn_delete(t.t, bytePtr(key), C.size_t(len(key)), &cerr)

	return statusToErr(status, cerr)
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
	var cerr *C.RegolithError
	status := C.regolith_txn_iter(t.t, cOpts.opts, &it, &cerr)
	if err := statusToErr(status, cerr); err != nil {
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

	var cerr *C.RegolithError
	err := statusToErr(C.regolith_txn_commit(t.t, &cerr), cerr)

	t.resolved = true
	C.regolith_txn_free(t.t, nil)

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
	// Neither status is checked, hence the nil `err` on both.
	C.regolith_txn_discard(t.t, nil)
	t.resolved = true
	C.regolith_txn_free(t.t, nil)
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
// releases it, mapping the given status and detail.
//
// Nothing is allocated on a non-OK status, so there is nothing to free then.
func takeBuf(ptr *C.uint8_t, length C.size_t, status C.int32_t, detail *C.RegolithError) ([]byte, error) {
	if err := statusToErr(status, detail); err != nil {
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
	detail *C.RegolithError,
) ([]byte, error) {
	// Releasing is a boundary crossing, and the FFI produces no handle for a
	// miss or for a present-but-empty value, so a nil check here is worth about
	// one crossing on every one of those reads - which is most of the cost of a
	// miss.  A nil handle is still safe to pass, this is purely the saving.
	if handle != nil {
		defer C.regolith_release_value(handle)
	}

	if err := statusToErr(status, detail); err != nil {
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
