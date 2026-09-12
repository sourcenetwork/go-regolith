package regolith

// #include <stdlib.h>
// #include "regolith_ffi.h"
import "C"

import (
	"encoding/binary"
	"sync/atomic"
	"unsafe"
)

// batchEntries is how many entries one [Iter.Next] refill asks the FFI layer to
// walk.
//
// Iteration used to cross the boundary three times per entry - advance, key,
// value - and a crossing is about 29ns.  A batch amortises the advance and the
// key over `batchEntries` entries, leaving one crossing per entry actually read
// for its value.  256 is large enough that the per-entry share of the refill is
// noise and small enough that the keys it frames and the values it pins stay a
// transient cost.
const batchEntries = 256

// entry locates one key inside [Iter.buf].
type entry struct {
	kOff, kLen int32
}

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
// Entries arrive a batch of keys at a time, so most [Iter.Next] calls are served
// from Go memory without crossing the boundary at all.  Values are not batched:
// they stay on the other side as reference-counted handles and only the ones
// [Iter.Value] is actually called for are copied, which is what keeps a scan
// that reads keys only from paying for value bytes.
//
// Iterator handles are not thread-safe, so neither is this type.  The store must
// not be closed while one is alive, and an iterator created from a [Txn] must be
// closed before that transaction is resolved.
type Iter struct {
	db       *DB
	i        *C.RegolithIter
	keysOnly bool

	// buf holds the current batch's framed keys, and ents indexes into it.  A
	// fresh buf is allocated per batch and never reused, because [Iter.Key]
	// hands out subslices of it that the caller is entitled to keep.
	buf  []byte
	ents []entry

	// pos is the current entry: ents[pos], when 0 <= pos < len(ents).  Anything
	// else - including the zero value, with no batch loaded yet - means the
	// iterator is not positioned.
	pos int

	// done records that a refill came back empty, so the range is exhausted and
	// there is nothing left to ask for.
	done bool

	// closed makes a second [Iter.Close] a no-op rather than a double free.
	closed atomic.Bool
}

// Next advances the iterator, or positions it at the start of its range when it
// has just been created or [Iter.Reset].  It reports whether the iterator now
// sits at a valid entry.
//
// A read failure part way through the range surfaces here, at the refill that
// hits it, rather than from [Iter.Value].
func (it *Iter) Next() (bool, error) {
	if it.db.closed.Load() {
		return false, ErrClosed
	}

	if it.pos+1 < len(it.ents) {
		it.pos++

		return true, nil
	}
	if it.done {
		it.pos = len(it.ents)

		return false, nil
	}

	return it.refill()
}

// refill walks the next batch of keys and positions on the first of them.
func (it *Iter) refill() (bool, error) {
	var out *C.uint8_t
	var outLen, count C.size_t
	var cerr *C.RegolithError

	status := C.regolith_iter_next_batch(it.i, C.size_t(batchEntries), &out, &outLen, &count, &cerr)

	// The batch is gone either way: on a failure nothing was handed over, and on
	// success it is about to be replaced.
	it.ents, it.pos = it.ents[:0], 0

	frame, err := takeBuf(out, outLen, status, cerr)
	if err != nil {
		return false, err
	}
	if count == 0 {
		// An empty batch - and only an empty batch - means the range is
		// exhausted.  A short one can also be the FFI layer capping how many
		// value bytes it pins at once.
		it.done = true

		return false, nil
	}

	// One Go allocation per batch, handed out as subslices by [Iter.Key].
	it.buf = frame
	for at := 0; at < len(frame); {
		keyLen := int32(binary.LittleEndian.Uint32(frame[at:]))
		at += 4
		it.ents = append(it.ents, entry{kOff: int32(at), kLen: keyLen})
		at += int(keyLen)
	}

	return true, nil
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

	it.invalidate()

	var valid C.uint8_t
	var cerr *C.RegolithError
	status := C.regolith_iter_seek(it.i, bytePtr(key), C.size_t(len(key)), &valid, &cerr)
	if err := statusToErr(status, cerr); err != nil {
		return false, err
	}
	if valid == 0 {
		// Nothing to seek to, so nothing further to walk either.
		it.done = true

		return false, nil
	}

	// The batch taken after a seek starts on the entry sought rather than
	// stepping past it, so the iterator is positioned on it here and the next
	// [Iter.Next] moves on as documented.
	return it.refill()
}

// Reset returns the iterator to the start of its range, allowing re-iteration.
//
// Resetting an iterator over a transaction also re-samples that transaction's
// buffered writes, so writes made since the iteration began become visible
// after a Reset.  Writes made midway through an iteration do not appear until
// such a rebuild.
func (it *Iter) Reset() {
	it.invalidate()

	// The status is not reported, and the call can only fail on a handle that is
	// already unusable.
	C.regolith_iter_reset(it.i, nil)
}

// invalidate drops the current batch, which a seek or a reset makes stale on
// both sides of the boundary.
func (it *Iter) invalidate() {
	it.ents, it.pos, it.done = it.ents[:0], 0, false
}

// Key returns the key at the current iterator location, or nil if the iterator
// is not at a valid location.
//
// The returned slice is part of the batch buffer rather than a copy, and that
// buffer is never reused, so it stays valid for as long as the caller keeps it.
//
// The store being closed does not affect this, as the key is already in Go
// memory.  The iterator itself must still be live: [Iter.Close] frees the
// handle, and nothing may be called on it afterwards.
func (it *Iter) Key() []byte {
	if it.pos < 0 || it.pos >= len(it.ents) {
		return nil
	}
	e := it.ents[it.pos]

	return it.buf[e.kOff : e.kOff+e.kLen]
}

// Value returns the value at the current iterator location, or nil if the
// iterator is not at a valid location or was created with `KeysOnly`.  An entry
// holding an empty value also reads back as nil.
//
// As with [Iter.Key], this keeps working after the store has been closed,
// because the value is owned by the iterator handle - which must itself still be
// live.
func (it *Iter) Value() ([]byte, error) {
	if it.keysOnly || it.pos < 0 || it.pos >= len(it.ents) {
		return nil, nil
	}

	// Locals, not fields: with #cgo noescape/nocallback these do not escape to
	// the heap, so there is nothing left to gain from holding them on the
	// iterator between calls.
	var val *C.uint8_t
	var valLen C.size_t
	var cerr *C.RegolithError

	// Borrowed, not owned: the FFI layer is holding a reference to these bytes
	// on behalf of the current batch, so there is nothing to free.
	status := C.regolith_iter_batch_value(it.i, C.size_t(it.pos), &val, &valLen, &cerr)
	if err := statusToErr(status, cerr); err != nil {
		return nil, err
	}
	if valLen == 0 {
		return nil, nil
	}

	return C.GoBytes(unsafe.Pointer(val), C.int(valLen)), nil
}

// Close releases the iterator.  Closing an already closed iterator is a no-op.
//
// It remains safe to call after the store has been closed, as the handle is
// owned by the FFI layer and closing it touches nothing else.
func (it *Iter) Close() error {
	if !it.closed.CompareAndSwap(false, true) {
		return nil
	}

	var cerr *C.RegolithError

	return statusToErr(C.regolith_iter_close(it.i, &cerr), cerr)
}

// BorrowValue hands the value at the current iterator location to fn without
// copying it, and returns fn's error unchanged.
//
// fn is called exactly once unless the value could not be read at all, and is
// called with nil when there is no value to hand over: an iterator created with
// `KeysOnly`, one not at a valid location, or an entry holding an empty value.
// That mirrors [Iter.Value] returning nil in the same three cases.
//
// # Lifetime of the borrowed bytes
//
// The slice is a window onto memory the engine owns, not a copy.  The batch that
// [Iter.Next] walked holds a reference-counted handle to the value behind every
// one of its entries, and that reference is what keeps the bytes - an SSTable
// block, or a memtable arena chunk - resident.  Nothing in Go is keeping them
// alive, so the slice is valid for exactly as long as the batch is, and not one
// instruction longer.
//
// Four things end a batch: [Iter.Next] refilling it, [Iter.Seek], [Iter.Reset]
// and [Iter.Close].  For the duration of the call fn makes none of them happen
// by itself, which is what makes the borrow safe; the caller must not make them
// happen either.  Concretely:
//
//   - fn must not retain the slice, store it, or hand it to anything that will
//     read it later.  Reading it after BorrowValue has returned is a
//     use-after-free, silent and unchecked.  Copy the bytes - or use
//     [Iter.AppendValue], or [Iter.Value] - if they are needed afterwards.
//   - fn must not mutate the slice.  These are the engine's bytes, shared with
//     whatever else is reading that block.
//   - fn must not call back into this iterator.  [Iter.Next], [Iter.Seek],
//     [Iter.Reset] and [Iter.Close] would each invalidate the very bytes fn is
//     holding, from underneath it.  This is documented rather than detected:
//     [Iter.Reset] has no way to report an error, so a guard could only cover
//     three of the four and a guard that covers most of the hazard is worse than
//     a stated contract, because it invites callers to rely on it.  The other
//     corekv stores implementing this interface do not detect it either.
//
// Holding a batch open is not free even when used correctly - its handles pin
// what owns their bytes - so a borrow is meant to be short.
func (it *Iter) BorrowValue(fn func(value []byte) error) error {
	return it.borrowValue(fn)
}

// AppendValue appends the value at the current iterator location to dst and
// returns the extended slice, following the convention of the stdlib's Append*
// functions.  Anything already in dst is preserved.
//
// This is the middle ground between [Iter.Value] and [Iter.BorrowValue]: the
// value bytes are still copied, once, but into a buffer the caller owns and can
// re-use across a whole iteration, so there is no allocation when dst has the
// capacity for them.  Unlike a borrowed slice the result is the caller's to keep.
//
// dst is returned unchanged, and no error, when there is no value to append - an
// iterator created with `KeysOnly`, one not at a valid location, or an entry
// holding an empty value - mirroring [Iter.Value] returning nil.
func (it *Iter) AppendValue(dst []byte) ([]byte, error) {
	err := it.borrowValue(func(value []byte) error {
		dst = append(dst, value...)

		return nil
	})
	if err != nil {
		return nil, err
	}

	return dst, nil
}

// borrowValue is the shared read path behind [Iter.BorrowValue] and
// [Iter.AppendValue]: it resolves the current entry's value to a slice over the
// engine's own memory and calls fn with it, returning fn's error untouched.
//
// The slice is built with [unsafe.Slice] rather than [C.GoBytes] precisely
// because a copy is what is being avoided; see [Iter.BorrowValue] for what keeps
// that memory alive and what the caller must not do with it.
func (it *Iter) borrowValue(fn func(value []byte) error) error {
	if it.keysOnly || it.pos < 0 || it.pos >= len(it.ents) {
		return fn(nil)
	}

	var val *C.uint8_t
	var valLen C.size_t
	var cerr *C.RegolithError

	// Borrowed, not owned: the FFI layer is holding a reference to these bytes
	// on behalf of the current batch, so there is nothing to free.
	status := C.regolith_iter_batch_value(it.i, C.size_t(it.pos), &val, &valLen, &cerr)
	if err := statusToErr(status, cerr); err != nil {
		return err
	}
	if valLen == 0 {
		// An empty value has no owner to point at, and the FFI layer yields a nil
		// pointer for it, which is not something to build a slice over.
		return fn(nil)
	}

	return fn(unsafe.Slice((*byte)(unsafe.Pointer(val)), int(valLen)))
}
