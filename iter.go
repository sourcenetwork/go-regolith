package regolith

// #include <stdlib.h>
// #include "regolith_ffi.h"
import "C"

import (
	"sync/atomic"
)

// Iter is an iterator over a regolith snapshot or transaction.
//
// An iterator starts un-positioned, so the first [Iter.Next] lands on the first
// entry in the range rather than advancing past it.
//
// The fussier iteration semantics - exclusive `End`, `Prefix` overriding `Start`
// and `End`, seek clamping, and the "pending reset" start position - are
// implemented on the other side of the FFI boundary, where the engine's cursor
// is, so there is nothing to reimplement here.
//
// Iterator handles are not thread-safe, so neither is this type.  The store must
// not be closed while one is alive, and an iterator created from a [Txn] must be
// closed before that transaction is resolved.
type Iter struct {
	db       *DB
	i        *C.RegolithIter
	keysOnly bool

	// closed makes a second [Iter.Close] a no-op rather than a double free.
	closed atomic.Bool
}

// Next advances the iterator, or positions it at the start of its range when it
// has just been created or [Iter.Reset].  It reports whether the iterator now
// sits at a valid entry.
func (it *Iter) Next() (bool, error) {
	if it.db.closed.Load() {
		return false, ErrClosed
	}

	var valid C.uint8_t
	status := C.regolith_iter_next(it.i, &valid)
	if err := statusToErr(status); err != nil {
		return false, err
	}

	return valid != 0, nil
}

// Seek moves the iterator to the given key, clamped to its range: forwards it
// lands on the smallest key at or above the target, in reverse on the greatest
// key at or below it.  It reports whether the iterator now sits at a valid
// entry.
//
// A following [Iter.Next] advances from the seek position.
func (it *Iter) Seek(key []byte) (bool, error) {
	if it.db.closed.Load() {
		return false, ErrClosed
	}

	var valid C.uint8_t
	status := C.regolith_iter_seek(it.i, bytePtr(key), C.size_t(len(key)), &valid)
	if err := statusToErr(status); err != nil {
		return false, err
	}

	return valid != 0, nil
}

// Reset returns the iterator to the start of its range, allowing re-iteration.
//
// Resetting an iterator over a transaction also re-samples that transaction's
// buffered writes, so writes made since the iteration began become visible
// after a Reset.  Writes made midway through an iteration do not appear until
// such a rebuild.
func (it *Iter) Reset() {
	// The status is not reported, and the call can only fail on a handle that is
	// already unusable.
	C.regolith_iter_reset(it.i)
}

// Key returns the key at the current iterator location, or nil if the iterator
// is not at a valid location.
//
// The store being closed does not affect this, as the entry is held by the
// iterator handle and not read back out of the store.  The iterator itself must
// still be live: [Iter.Close] frees the handle, and nothing may be called on it
// afterwards.
func (it *Iter) Key() []byte {
	var key *C.uint8_t
	var keyLen C.size_t
	status := C.regolith_iter_key(it.i, &key, &keyLen)

	// No error is reported here; an invalid location yields a not-found status,
	// and nothing else here is actionable by the caller.
	value, _ := takeBuf(key, keyLen, status)

	return value
}

// Value returns the value at the current iterator location, or nil if the
// iterator is not at a valid location or was created with `KeysOnly`.  An entry
// holding an empty value also reads back as nil.
//
// As with [Iter.Key], this keeps working after the store has been closed,
// because the entry is owned by the iterator handle - which must itself still be
// live.
func (it *Iter) Value() ([]byte, error) {
	if it.keysOnly {
		return nil, nil
	}

	var val *C.uint8_t
	var valLen C.size_t
	status := C.regolith_iter_value(it.i, &val, &valLen)
	if status == C.REGOLITH_ERR_NOT_FOUND {
		// The iterator is not at a valid location.
		return nil, nil
	}

	return takeBuf(val, valLen, status)
}

// Close releases the iterator.  Closing an already closed iterator is a no-op.
//
// It remains safe to call after the store has been closed, as the handle is
// owned by the FFI layer and closing it touches nothing else.
func (it *Iter) Close() error {
	if !it.closed.CompareAndSwap(false, true) {
		return nil
	}

	return statusToErr(C.regolith_iter_close(it.i))
}
