package regolith

import "encoding/binary"

// Frame tags.  Must match OP_SET and OP_DELETE in ffi/src/batch.rs.
const (
	opSet    = 1
	opDelete = 2
)

// WriteBatch collects sets and deletes for [DB.Write] to apply in one call.
// The zero value is an empty batch ready for use.
//
// Ops are packed as they are added into one buffer that crosses the FFI
// boundary once per [DB.Write], as
//
//	set:    [1][u64 key len][key][u64 value len][value]
//	delete: [2][u64 key len][key]
//
// with little-endian lengths, repeated.  The buffer is kept across
// [WriteBatch.Reset] so a batch reused for the same shape allocates nothing
// after its first fill.  Its memory is the batch's contents plus a few bytes
// per op, and it is the caller's to bound: [WriteBatch.Size] is the number to
// watch.
//
// A batch is applied atomically (every op or none, one write-ahead log
// record) with the store's durability ([Options.Durability]), the same as
// [DB.Set].  It is not a transaction: nothing is validated against concurrent
// writers, there is no snapshot and no reads, and it cannot be discarded once
// written.  Ops on the same key apply in the order they were added, so the
// last one wins.
//
// A WriteBatch is not safe for concurrent use.  Distinct batches may be
// written concurrently.
type WriteBatch struct {
	buf []byte
	n   int
}

// NewWriteBatch returns an empty batch whose buffer can already hold
// sizeHint bytes of packed ops, so a batch of known size fills without
// growing.  A hint of zero, or a negative one, is the zero value.
func NewWriteBatch(sizeHint int) *WriteBatch {
	return &WriteBatch{buf: make([]byte, 0, max(sizeHint, 0))}
}

// Set adds a write of value at key.
func (b *WriteBatch) Set(key, value []byte) {
	b.buf = append(b.buf, opSet)
	b.buf = appendField(b.buf, key)
	b.buf = appendField(b.buf, value)
	b.n++
}

// Delete adds a delete of key.  Deleting a key that has no entry is not an
// error when the batch is written.
func (b *WriteBatch) Delete(key []byte) {
	b.buf = append(b.buf, opDelete)
	b.buf = appendField(b.buf, key)
	b.n++
}

// Len is the number of ops in the batch.
func (b *WriteBatch) Len() int { return b.n }

// Size is the number of packed bytes the batch holds, which is what its
// memory and the cost of writing it scale with.
func (b *WriteBatch) Size() int { return len(b.buf) }

// Reset empties the batch, keeping its buffer for reuse.
func (b *WriteBatch) Reset() {
	b.buf, b.n = b.buf[:0], 0
}

// appendField packs one length-prefixed byte string.
func appendField(buf, field []byte) []byte {
	buf = binary.LittleEndian.AppendUint64(buf, uint64(len(field)))

	return append(buf, field...)
}
