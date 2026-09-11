/*
 * regolith_ffi.h - flat C ABI over the regolith embedded key-value engine.
 *
 * Hand-written and kept in sync by hand with the Rust sources under src/ (no
 * cbindgen). If you change one, change the other.
 *
 * ---------------------------------------------------------------------
 * Contract
 * ---------------------------------------------------------------------
 *
 * Status codes
 *   Every function returns int32_t. REGOLITH_OK (0) is success. Anything
 *   else is a failure and no out-param other than *err has been written,
 *   except where a function's comment says otherwise.
 *
 * Error detail
 *   Every status-returning function takes a trailing RegolithError **err.
 *   It may be NULL, meaning the caller does not want detail. Otherwise
 *   the call writes *err exactly once, before returning: NULL on
 *   REGOLITH_OK and for every code that is its own message, or an object
 *   the caller owns for REGOLITH_ERR_INVALID_ARG, REGOLITH_ERR_PANIC and
 *   REGOLITH_ERR_OTHER. Read it with regolith_error_message() and release
 *   it with regolith_error_free(), exactly once. The detail travels with
 *   the status in the same call, so nothing is keyed on the calling
 *   thread and the object may be read and freed from any thread.
 *
 * Panics
 *   Every entry point catches Rust panics and returns
 *   REGOLITH_ERR_PANIC. No unwind crosses this boundary. A handle that
 *   produced a panic should be treated as unusable.
 *
 * Handles
 *   RegolithDb, RegolithTxn and RegolithIter are opaque, heap-allocated
 *   by this library, and released by their matching close/free function.
 *   Passing NULL returns REGOLITH_ERR_INVALID_ARG; it is never
 *   dereferenced. Passing an already-freed handle is undefined.
 *
 * Input bytes
 *   Inputs cross as (const uint8_t *ptr, size_t len) and are NOT retained
 *   past the call: the library copies whatever it needs before returning.
 *   Passing Go memory directly is therefore legal under the cgo pointer
 *   rules. A len of 0 is an empty input regardless of the pointer, which
 *   is how a nil Go slice arrives. A batch of writes is one such input,
 *   packed as the frame described at regolith_db_write.
 *
 * Output bytes
 *   Outputs are allocated by this library and handed over through
 *   (uint8_t **ptr, size_t *len) out-params. The caller must copy them
 *   (C.GoBytes) and then release them with regolith_free_buf(ptr, len),
 *   passing back exactly the pair it received. A zero-length output does
 *   not need freeing but freeing it is harmless.
 *
 * Borrowed output bytes
 *   Three calls hand back a const pointer into memory the engine already
 *   owns, rather than transferring an allocation: regolith_iter_batch_value,
 *   regolith_db_get_borrowed and regolith_txn_get_borrowed. In every case the
 *   bytes are borrowed, not given: copy them, never pass them to
 *   regolith_free_buf, and do not keep the pointer past its validity window.
 *
 *   regolith_iter_batch_value carries no handle. Its bytes belong to the
 *   current batch and stay valid for exactly as long as that batch does; see
 *   the validity window in that function's comment.
 *
 *   The two point reads hand back an opaque RegolithValue handle alongside
 *   the pointer, holding the reference count that keeps the memory valid.
 *   The pointer is valid from the call returning REGOLITH_OK until
 *   regolith_release_value is called on the handle, and not one instruction
 *   longer. The caller must always release it, on every path including
 *   failing ones, and should do it in the same call that copies the bytes -
 *   a handle pins what owns the value, so one held on keeps an SSTable block
 *   or a memtable arena chunk resident no matter what the engine would
 *   rather do with it.
 *
 *   Two cases produce no handle, and NULL is exactly what
 *   regolith_release_value expects, so the caller needs only one
 *   unconditional release: REGOLITH_ERR_NOT_FOUND, which writes no
 *   out-param at all, and a present-but-zero-length value, which is
 *   REGOLITH_OK with (NULL, 0) because an empty value has no owner to pin.
 *
 * Ordering contracts
 *   1. Every RegolithIter derived from a RegolithTxn must be closed
 *      before that transaction is committed, discarded or freed. The
 *      iterator reads at the transaction's snapshot sequence, and the
 *      transaction holds the pin keeping that sequence alive.
 *   2. Every RegolithTxn and RegolithIter must be closed/freed before
 *      regolith_db_close, and every RegolithValue released before it.
 *   3. A RegolithIter over a transaction materialises that transaction's
 *      buffered writes when it is first advanced, and again on each
 *      rebuild (regolith_iter_reset or regolith_iter_seek). A write
 *      buffered after the iterator was created but before its first
 *      regolith_iter_next / regolith_iter_next_batch IS visible; one
 *      buffered midway through an iteration is not, until the next
 *      rebuild.
 *   4. A pointer from regolith_iter_batch_value belongs to the batch it
 *      came from: regolith_iter_next_batch, regolith_iter_seek,
 *      regolith_iter_reset and regolith_iter_close all invalidate it.
 *      Reading one afterwards is undefined.
 *
 * Thread safety
 *   A RegolithDb handle may be used from multiple threads at once. A
 *   RegolithTxn handle may be read and written from multiple threads at
 *   once, but commit/discard/free must not race with anything else on it.
 *   A RegolithIter handle is not thread-safe; use it from one thread at a
 *   time. A RegolithValue is read-only bytes plus an atomic refcount, so
 *   it may be read from any thread, but release it exactly once. A
 *   RegolithError is immutable; read it from any thread, free it exactly
 *   once.
 */

#ifndef REGOLITH_FFI_H
#define REGOLITH_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* --------------------------------------------------------------------
 * Status codes. Must match the constants in src/lib.rs.
 * ------------------------------------------------------------------ */

#define REGOLITH_OK 0
/* No entry for the requested key. Maps to corekv.ErrNotFound. */
#define REGOLITH_ERR_NOT_FOUND 1
/* The database has been closed. Maps to corekv.ErrDBClosed. */
#define REGOLITH_ERR_DB_CLOSED 2
/* Commit-time validation lost a race. Maps to corekv.ErrTxnConflict. */
#define REGOLITH_ERR_TXN_CONFLICT 3
/* Write attempted on a read-only txn. Maps to corekv.ErrReadOnlyTxn. */
#define REGOLITH_ERR_READ_ONLY_TXN 4
/* Transaction already committed or discarded. corekv.ErrDiscardedTxn. */
#define REGOLITH_ERR_DISCARDED 5
/* NULL handle, NULL out-param, or an unusable argument. */
#define REGOLITH_ERR_INVALID_ARG 6
/* A Rust panic was caught at the boundary. */
#define REGOLITH_ERR_PANIC 7
/* Anything else; the detail says what. */
#define REGOLITH_ERR_OTHER 8

/* --------------------------------------------------------------------
 * Opaque handles.
 * ------------------------------------------------------------------ */

typedef struct RegolithDb RegolithDb;
typedef struct RegolithTxn RegolithTxn;
typedef struct RegolithIter RegolithIter;

/* A borrowed value's keep-alive. It owns no bytes of its own: it holds the
 * reference count on whatever inside the engine owns them. See "Borrowed
 * output bytes" above. */
typedef struct RegolithValue RegolithValue;

/* Detail behind a status whose code alone does not say what went wrong;
 * see "Error detail" above. */
typedef struct RegolithError RegolithError;

/* --------------------------------------------------------------------
 * Iteration options.
 *
 * A byte range is absent when its pointer is NULL or its length is 0.
 *
 *   prefix    - only keys beginning with this are yielded, including the
 *               key exactly equal to the prefix. Overrides start/end.
 *   start     - inclusive lower bound.
 *   end       - exclusive upper bound.
 *   reverse   - non-zero to walk in descending key order.
 *   keys_only - non-zero to skip value reads; regolith_iter_value then
 *               yields a zero-length buffer.
 *
 * The struct and the bytes it points at only need to stay valid for the
 * duration of the regolith_db_iter / regolith_txn_iter call.
 * ------------------------------------------------------------------ */

typedef struct RegolithIterOptions {
  const uint8_t *prefix;
  size_t prefix_len;
  const uint8_t *start;
  size_t start_len;
  const uint8_t *end;
  size_t end_len;
  uint8_t reverse;
  uint8_t keys_only;
} RegolithIterOptions;

/* --------------------------------------------------------------------
 * Engine options.
 *
 * A deliberately small subset of regolith's own Options: the fields with
 * measured or plausible impact on this workload, not the whole type
 * (which carries trait-object hooks with no C representation).
 *
 * Unset is not zero. Three of these fields treat 0 as a real setting -
 * max_background_compactions 0 compacts on the calling thread,
 * block_cache_size 0 disables the block cache, transaction_keys_inline 0
 * never indexes a transaction's buffer - so "no opinion" cannot be
 * spelled 0. The `present` bitmask carries which fields were actually
 * set: a field is read only when its REGOLITH_OPT_* bit is set in it,
 * and a zeroed struct therefore means "engine defaults, every field".
 * Setting an unknown bit is REGOLITH_ERR_INVALID_ARG.
 *
 *   write_buffer_size           - memtable bytes before a flush.
 *                                 Default 64 MB. Must be > 0.
 *   block_cache_size            - decompressed-block cache bytes; 0
 *                                 disables the cache. Default 512 MB.
 *   max_background_compactions  - compaction worker threads; 0 runs
 *                                 compaction on the calling thread.
 *                                 Default 1.
 *   transaction_keys_inline     - keys one transaction buffers before it
 *                                 builds a hash index over them; 0 never
 *                                 indexes. Default 32.
 *   compression                 - one of the REGOLITH_COMPRESSION_*
 *                                 values. Default LZ4.
 *   durability                  - one of the REGOLITH_DURABILITY_*
 *                                 values. Default Eventual.
 *   isolation                   - one of the REGOLITH_ISOLATION_*
 *                                 values, used by every transaction the
 *                                 store begins. Default snapshot
 *                                 isolation.
 *
 * `isolation` is the one field that is not an engine option: regolith
 * keeps the level on the transaction database rather than on its
 * Options, so it is applied to the store handle at open and read back
 * from it by regolith_db_txn. Leaving the bit unset therefore keeps the
 * behaviour this ABI has always had - snapshot isolation, aborting on a
 * write-write overlap and admitting write skew. REGOLITH_ISOLATION_
 * SERIALIZABLE additionally validates every key the transaction read, so
 * write skew aborts too, at the cost of a read-set-sized check on the
 * transaction that commits second.
 *
 * Invalid values are rejected, never clamped, and the detail message
 * names the offending field. An unknown enum value or presence bit is
 * refused by this layer; a value regolith itself refuses (a zero
 * write_buffer_size, for one) comes back from its own option validation,
 * which runs before any filesystem work, so a rejected open creates
 * nothing.
 *
 * The struct contains no pointers and only needs to stay valid for the
 * duration of the regolith_db_open_with_options call, like
 * RegolithIterOptions.
 * ------------------------------------------------------------------ */

/* Presence bits for RegolithOptions.present. Must match src/options.rs. */
#define REGOLITH_OPT_WRITE_BUFFER_SIZE (1ULL << 0)
#define REGOLITH_OPT_BLOCK_CACHE_SIZE (1ULL << 1)
#define REGOLITH_OPT_MAX_BACKGROUND_COMPACTIONS (1ULL << 2)
#define REGOLITH_OPT_TRANSACTION_KEYS_INLINE (1ULL << 3)
#define REGOLITH_OPT_COMPRESSION (1ULL << 4)
#define REGOLITH_OPT_DURABILITY (1ULL << 5)
#define REGOLITH_OPT_ISOLATION (1ULL << 6)

/* Values for RegolithOptions.compression. */
#define REGOLITH_COMPRESSION_NONE 0
#define REGOLITH_COMPRESSION_SNAPPY 1
#define REGOLITH_COMPRESSION_LZ4 2

/* Values for RegolithOptions.durability. */
#define REGOLITH_DURABILITY_IMMEDIATE 0
#define REGOLITH_DURABILITY_EVENTUAL 1

/* Values for RegolithOptions.isolation. What each level validates at
 * commit: only what the transaction wrote; that plus keys read for
 * update (regolith's default); or the entire read set. */
#define REGOLITH_ISOLATION_READ_COMMITTED 0
#define REGOLITH_ISOLATION_SNAPSHOT 1
#define REGOLITH_ISOLATION_SERIALIZABLE 2

typedef struct RegolithOptions {
  uint64_t present;
  uint64_t write_buffer_size;
  uint64_t block_cache_size;
  uint64_t max_background_compactions;
  uint64_t transaction_keys_inline;
  uint32_t compression;
  uint32_t durability;
  uint32_t isolation;
} RegolithOptions;

/* --------------------------------------------------------------------
 * Utilities.
 * ------------------------------------------------------------------ */

/* Does nothing. Exists to measure raw cgo call overhead; it is marked
 * #[inline(never)] on the Rust side so it is a real call. */
void regolith_noop(void);

/* The detail's message, NUL-terminated, borrowed from the object and
 * valid until regolith_error_free. NULL for a NULL object. */
const char *regolith_error_message(const RegolithError *err);

/* Release a detail object. NULL is a no-op. Exactly once per object. */
void regolith_error_free(RegolithError *err);

/* Release a buffer handed out by any key/value call. Pass back exactly
 * the (ptr, len) pair received. NULL or len 0 is a no-op. */
void regolith_free_buf(uint8_t *ptr, size_t len);

/* Release a handle from a *_get_borrowed call, unpinning the engine
 * memory the value was read out of. The borrowed pointer that came with
 * it must not be touched afterwards. NULL is a no-op, which is what a
 * not-found or zero-length read produces. */
void regolith_release_value(RegolithValue *handle);

/* --------------------------------------------------------------------
 * Store.
 * ------------------------------------------------------------------ */

/* Open (or create) a store with regolith's default options. `path` must
 * be valid UTF-8. Exactly regolith_db_open_with_options with a NULL
 * `opts`. On REGOLITH_OK, *out owns a handle to be released with
 * regolith_db_close. */
int32_t regolith_db_open(const uint8_t *path, size_t path_len,
                         RegolithDb **out, RegolithError **err);

/* Open (or create) a store with the engine options in `opts`. `opts` may
 * be NULL, and a struct whose `present` mask is empty is equivalent:
 * both mean regolith's defaults for every field. Only the fields whose
 * presence bit is set are applied. See "Engine options" above for the
 * unset-is-not-zero rule and for how invalid values are reported. On
 * REGOLITH_OK, *out owns a handle to be released with
 * regolith_db_close. */
int32_t regolith_db_open_with_options(const uint8_t *path, size_t path_len,
                                      const RegolithOptions *opts,
                                      RegolithDb **out, RegolithError **err);

/* Close the store and free the handle. The handle is invalid afterwards
 * even when a non-OK status is returned. All derived transactions and
 * iterators must already be closed. */
int32_t regolith_db_close(RegolithDb *db, RegolithError **err);

/* Read a key without copying it. On REGOLITH_OK, (*val, *val_len) is a
 * borrowed view of bytes the engine owns and *handle keeps them alive;
 * copy the bytes and then call regolith_release_value(*handle), always
 * and promptly. REGOLITH_ERR_NOT_FOUND when absent, and a zero-length
 * value yields (NULL, 0) - neither produces a handle. See "Borrowed
 * output bytes" above for the full rule. */
int32_t regolith_db_get_borrowed(RegolithDb *db, const uint8_t *key,
                                 size_t key_len, const uint8_t **val,
                                 size_t *val_len, RegolithValue **handle,
                                 RegolithError **err);

/* Test for a key, writing 0 or 1 to *found. */
int32_t regolith_db_has(RegolithDb *db, const uint8_t *key, size_t key_len,
                        uint8_t *found, RegolithError **err);

/* Write a value, overwriting any existing entry. */
int32_t regolith_db_set(RegolithDb *db, const uint8_t *key, size_t key_len,
                        const uint8_t *value, size_t value_len,
                        RegolithError **err);

/* Delete a key. Deleting an absent key is REGOLITH_OK. */
int32_t regolith_db_delete(RegolithDb *db, const uint8_t *key, size_t key_len,
                           RegolithError **err);

/* Apply a batch of sets and deletes atomically: every op lands or none
 * does, as one WAL record and one contiguous sequence range, with the
 * store's durability mode. No conflict check, no snapshot: this is the
 * engine's native batch write, not a transaction.
 *
 * `ops` is a frame of `ops_len` bytes, a sequence of
 *
 *   set:    [uint8_t 1][uint64_t key_len][key][uint64_t value_len][value]
 *   delete: [uint8_t 2][uint64_t key_len][key]
 *
 * lengths little-endian, repeated until the buffer ends. Zero bytes is an
 * empty batch and REGOLITH_OK. A frame that does not decode (unknown tag,
 * a length past the end) is REGOLITH_ERR_INVALID_ARG naming the op, and
 * nothing is written; so is a key or value over the engine's size limit,
 * checked over the whole batch before any op is applied. A batch whose WAL
 * record (the frame's bytes plus 8 per set, 12 per delete, plus 4) would
 * exceed 1073741824 bytes is likewise REGOLITH_ERR_INVALID_ARG naming the
 * op, and nothing is written. Ops on one key apply in frame order, last one
 * wins. The frame is not retained. */
int32_t regolith_db_write(RegolithDb *db, const uint8_t *ops, size_t ops_len,
                          RegolithError **err);

/* Delete every entry in the store (corekv.Dropable.DropAll). */
int32_t regolith_db_drop_all(RegolithDb *db, RegolithError **err);

/* Create an iterator over a snapshot of the store taken now. `opts` may
 * be NULL, meaning full forward iteration with values. On REGOLITH_OK,
 * *out owns a handle to be released with regolith_iter_close. */
int32_t regolith_db_iter(RegolithDb *db, const RegolithIterOptions *opts,
                         RegolithIter **out, RegolithError **err);

/* Begin an optimistic transaction at the store's configured isolation
 * level - the one RegolithOptions.isolation selected at open, which is
 * snapshot isolation when it was left unset. Pass readonly
 * non-zero to have regolith_txn_set/delete return
 * REGOLITH_ERR_READ_ONLY_TXN; regolith itself has no read-only mode, so
 * the flag is enforced by this layer. On REGOLITH_OK, *out owns a handle
 * to be released with regolith_txn_free. */
int32_t regolith_db_txn(RegolithDb *db, uint8_t readonly, RegolithTxn **out,
                        RegolithError **err);

/* --------------------------------------------------------------------
 * Transaction.
 * ------------------------------------------------------------------ */

/* Read a key through the transaction, seeing its own buffered writes.
 * Borrowed exactly as regolith_db_get_borrowed, including the obligation
 * to release *handle; the value is independent of the transaction, so a
 * handle stays valid across its commit or discard. */
int32_t regolith_txn_get_borrowed(RegolithTxn *txn, const uint8_t *key,
                                  size_t key_len, const uint8_t **val,
                                  size_t *val_len, RegolithValue **handle,
                                  RegolithError **err);

/* Test for a key through the transaction, writing 0 or 1 to *found. */
int32_t regolith_txn_has(RegolithTxn *txn, const uint8_t *key, size_t key_len,
                         uint8_t *found, RegolithError **err);

/* Buffer a write. REGOLITH_ERR_READ_ONLY_TXN on a read-only txn. */
int32_t regolith_txn_set(RegolithTxn *txn, const uint8_t *key, size_t key_len,
                         const uint8_t *value, size_t value_len,
                         RegolithError **err);

/* Buffer a delete. REGOLITH_ERR_READ_ONLY_TXN on a read-only txn. */
int32_t regolith_txn_delete(RegolithTxn *txn, const uint8_t *key,
                            size_t key_len, RegolithError **err);

/* Create an iterator over the transaction: its buffered writes merged
 * over its begin snapshot. The write set is sampled on first advance and
 * on each rebuild, not here. See ordering contracts 1 and 3 above. */
int32_t regolith_txn_iter(RegolithTxn *txn, const RegolithIterOptions *opts,
                          RegolithIter **out, RegolithError **err);

/* Validate and apply the transaction. REGOLITH_ERR_TXN_CONFLICT when
 * another writer touched a validated key first; the transaction is
 * resolved either way and the caller should retry from a new one. A
 * conflict carries no detail; the code is the message.
 * REGOLITH_ERR_DISCARDED if already resolved. The handle stays
 * allocated; release it with regolith_txn_free. */
int32_t regolith_txn_commit(RegolithTxn *txn, RegolithError **err);

/* Discard the transaction, dropping its buffered writes. Idempotent:
 * REGOLITH_OK even if already resolved. The handle stays allocated;
 * release it with regolith_txn_free. */
int32_t regolith_txn_discard(RegolithTxn *txn, RegolithError **err);

/* Free the transaction handle, discarding first if still unresolved.
 * Exactly once per handle, after all its iterators are closed. */
int32_t regolith_txn_free(RegolithTxn *txn, RegolithError **err);

/* --------------------------------------------------------------------
 * Iterator.
 *
 * An iterator starts un-positioned with a pending reset, so the first
 * regolith_iter_next positions it on the first in-range entry rather
 * than advancing past it. This matches corekv's Iterator contract.
 *
 * There are two ways to walk: one entry at a time with
 * regolith_iter_next plus regolith_iter_key / regolith_iter_value, which
 * is three crossings per entry; or a batch of keys at a time with
 * regolith_iter_next_batch plus regolith_iter_batch_value. The two share
 * one cursor and may be mixed freely - the batch calls are the bulk path
 * and the single calls the positioning path - with one difference, noted
 * on both: after a regolith_iter_seek, regolith_iter_next advances past
 * the entry sought while regolith_iter_next_batch includes it.
 * ------------------------------------------------------------------ */

/* Advance (or, after construction/reset, position at the start of the
 * range). Writes 0 or 1 to *valid. */
int32_t regolith_iter_next(RegolithIter *it, uint8_t *valid,
                           RegolithError **err);

/* Seek, clamped to the configured range: a target below `start` becomes
 * `start`, and under `reverse` a target at or above `end` becomes the top
 * of the range. Forward lands on the smallest key >= the target, reverse
 * on the greatest key <= it. Writes 0 or 1 to *valid. Clears any pending
 * reset, so a following regolith_iter_next advances from here, while a
 * following regolith_iter_next_batch starts on the entry sought.
 * Invalidates the current batch and its value pointers. */
int32_t regolith_iter_seek(RegolithIter *it, const uint8_t *key,
                           size_t key_len, uint8_t *valid,
                           RegolithError **err);

/* Mark for re-iteration: the next regolith_iter_next returns to the
 * start of the range. Invalidates the current batch and its value
 * pointers. Never fails on a live handle. */
int32_t regolith_iter_reset(RegolithIter *it, RegolithError **err);

/* Copy out the current key. REGOLITH_ERR_NOT_FOUND when the iterator is
 * not positioned on a valid in-range entry. */
int32_t regolith_iter_key(RegolithIter *it, uint8_t **key, size_t *key_len,
                          RegolithError **err);

/* Copy out the current value, or a zero-length buffer when the iterator
 * was created with keys_only. REGOLITH_ERR_NOT_FOUND when not
 * positioned on a valid in-range entry. */
int32_t regolith_iter_value(RegolithIter *it, uint8_t **val, size_t *val_len,
                            RegolithError **err);

/* Advance up to `max_entries` times, framing the KEYS walked into one
 * buffer:
 *
 *     [uint32_t key_len][key bytes]  repeated *out_count times
 *
 * little-endian, one allocation, owned by the caller and released with
 * regolith_free_buf(*out, *out_len) like any other output buffer.
 *
 * The first entry of a batch follows the same positioning rule as
 * regolith_iter_next: after construction or regolith_iter_reset the
 * batch starts at the first in-range entry rather than advancing past
 * it, and after regolith_iter_seek it starts at the entry the seek
 * landed on.
 *
 * *out_count == 0 is the only signal that the range is exhausted. A
 * short batch is NOT one: it also happens when the batch reaches its cap
 * on retained value bytes (1 MiB), or when a read error is being held
 * back so the entries already walked are not lost - that error is
 * returned from the next call. `max_entries` must be at least 1;
 * 0 is REGOLITH_ERR_INVALID_ARG.
 *
 * Values are NOT framed. The call retains one reference-counted value
 * handle per entry instead - no copy on either the forward or the
 * reverse path - addressable with regolith_iter_batch_value. Under
 * keys_only nothing is retained at all. Those handles pin what owns
 * their bytes, which is why a batch is meant to be drained promptly and
 * why its total is capped. */
int32_t regolith_iter_next_batch(RegolithIter *it, size_t max_entries,
                                 uint8_t **out, size_t *out_len,
                                 size_t *out_count, RegolithError **err);

/* Borrowed pointer to value #idx of the current batch, counting from 0
 * in the order the keys were framed. No allocation, and nothing to
 * free.
 *
 * Valid until the next regolith_iter_next_batch, regolith_iter_seek,
 * regolith_iter_reset or regolith_iter_close on this handle; see
 * ordering contract 4. A keys_only iterator yields (NULL, 0) for every
 * index inside the batch, matching regolith_iter_value's empty buffer.
 * An `idx` at or beyond the last batch's *out_count is
 * REGOLITH_ERR_INVALID_ARG. */
int32_t regolith_iter_batch_value(RegolithIter *it, size_t idx,
                                  const uint8_t **val, size_t *val_len,
                                  RegolithError **err);

/* Close the iterator and free its handle. Exactly once per handle. */
int32_t regolith_iter_close(RegolithIter *it, RegolithError **err);

#ifdef __cplusplus
}
#endif

#endif /* REGOLITH_FFI_H */
