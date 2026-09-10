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
 *   else is a failure and no out-param has been written, except where a
 *   function's comment says otherwise.
 *
 * Error detail
 *   For REGOLITH_ERR_OTHER, REGOLITH_ERR_PANIC and
 *   REGOLITH_ERR_TXN_CONFLICT a human-readable detail string is stored in
 *   a thread-local slot. Read it with regolith_last_error_message(), which
 *   returns a freshly allocated NUL-terminated string (or NULL), and
 *   release it with regolith_free_string(). The slot belongs to the
 *   calling OS thread, so read it on the same thread that got the status,
 *   before making another call on that thread.
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
 *   is how a nil Go slice arrives.
 *
 * Output bytes
 *   Outputs are allocated by this library and handed over through
 *   (uint8_t **ptr, size_t *len) out-params. The caller must copy them
 *   (C.GoBytes) and then release them with regolith_free_buf(ptr, len),
 *   passing back exactly the pair it received. A zero-length output does
 *   not need freeing but freeing it is harmless.
 *
 * Borrowed output bytes
 *   The point reads are different: regolith_db_get_borrowed and
 *   regolith_txn_get_borrowed allocate and copy nothing. They hand back a
 *   const pointer into memory the engine already owns, through
 *   (const uint8_t **val, size_t *val_len), plus an opaque RegolithValue
 *   handle holding the reference count that keeps that memory valid.
 *
 *   The pointer is valid from the call returning REGOLITH_OK until
 *   regolith_release_value is called on the handle, and not one
 *   instruction longer. The caller must always release it, on every path
 *   including failing ones, and should do it in the same call that copies
 *   the bytes - a handle pins what owns the value, so one held on keeps an
 *   SSTable block or a memtable arena chunk resident no matter what the
 *   engine would rather do with it. Do not free a borrowed pointer with
 *   regolith_free_buf; it is not owned.
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
 *      regolith_iter_next IS visible; one buffered midway through an
 *      iteration is not, until the next rebuild.
 *
 * Thread safety
 *   A RegolithDb handle may be used from multiple threads at once. A
 *   RegolithTxn handle may be read and written from multiple threads at
 *   once, but commit/discard/free must not race with anything else on it.
 *   A RegolithIter handle is not thread-safe; use it from one thread at a
 *   time. A RegolithValue is read-only bytes plus an atomic refcount, so
 *   it may be read from any thread, but release it exactly once.
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
/* Anything else; see regolith_last_error_message(). */
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
 * Utilities.
 * ------------------------------------------------------------------ */

/* Does nothing. Exists to measure raw cgo call overhead; it is marked
 * #[inline(never)] on the Rust side so it is a real call. */
void regolith_noop(void);

/* Last error detail for the calling thread, freshly allocated, or NULL.
 * Release with regolith_free_string. */
char *regolith_last_error_message(void);

/* Release a string from regolith_last_error_message. NULL is a no-op. */
void regolith_free_string(char *s);

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

/* Open (or create) a store. `path` must be valid UTF-8. Engine options
 * are regolith's defaults; nothing is tuned here. On REGOLITH_OK, *out
 * owns a handle to be released with regolith_db_close. */
int32_t regolith_db_open(const uint8_t *path, size_t path_len,
                         RegolithDb **out);

/* Close the store and free the handle. The handle is invalid afterwards
 * even when a non-OK status is returned. All derived transactions and
 * iterators must already be closed. */
int32_t regolith_db_close(RegolithDb *db);

/* Read a key without copying it. On REGOLITH_OK, (*val, *val_len) is a
 * borrowed view of bytes the engine owns and *handle keeps them alive;
 * copy the bytes and then call regolith_release_value(*handle), always
 * and promptly. REGOLITH_ERR_NOT_FOUND when absent, and a zero-length
 * value yields (NULL, 0) - neither produces a handle. See "Borrowed
 * output bytes" above for the full rule. */
int32_t regolith_db_get_borrowed(RegolithDb *db, const uint8_t *key,
                                 size_t key_len, const uint8_t **val,
                                 size_t *val_len, RegolithValue **handle);

/* Test for a key, writing 0 or 1 to *found. */
int32_t regolith_db_has(RegolithDb *db, const uint8_t *key, size_t key_len,
                        uint8_t *found);

/* Write a value, overwriting any existing entry. */
int32_t regolith_db_set(RegolithDb *db, const uint8_t *key, size_t key_len,
                        const uint8_t *value, size_t value_len);

/* Delete a key. Deleting an absent key is REGOLITH_OK. */
int32_t regolith_db_delete(RegolithDb *db, const uint8_t *key, size_t key_len);

/* Delete every entry in the store (corekv.Dropable.DropAll). */
int32_t regolith_db_drop_all(RegolithDb *db);

/* Create an iterator over a snapshot of the store taken now. `opts` may
 * be NULL, meaning full forward iteration with values. On REGOLITH_OK,
 * *out owns a handle to be released with regolith_iter_close. */
int32_t regolith_db_iter(RegolithDb *db, const RegolithIterOptions *opts,
                         RegolithIter **out);

/* Begin a transaction (optimistic, snapshot isolation). Pass readonly
 * non-zero to have regolith_txn_set/delete return
 * REGOLITH_ERR_READ_ONLY_TXN; regolith itself has no read-only mode, so
 * the flag is enforced by this layer. On REGOLITH_OK, *out owns a handle
 * to be released with regolith_txn_free. */
int32_t regolith_db_txn(RegolithDb *db, uint8_t readonly, RegolithTxn **out);

/* --------------------------------------------------------------------
 * Transaction.
 * ------------------------------------------------------------------ */

/* Read a key through the transaction, seeing its own buffered writes.
 * Borrowed exactly as regolith_db_get_borrowed, including the obligation
 * to release *handle; the value is independent of the transaction, so a
 * handle stays valid across its commit or discard. */
int32_t regolith_txn_get_borrowed(RegolithTxn *txn, const uint8_t *key,
                                  size_t key_len, const uint8_t **val,
                                  size_t *val_len, RegolithValue **handle);

/* Test for a key through the transaction, writing 0 or 1 to *found. */
int32_t regolith_txn_has(RegolithTxn *txn, const uint8_t *key, size_t key_len,
                         uint8_t *found);

/* Buffer a write. REGOLITH_ERR_READ_ONLY_TXN on a read-only txn. */
int32_t regolith_txn_set(RegolithTxn *txn, const uint8_t *key, size_t key_len,
                         const uint8_t *value, size_t value_len);

/* Buffer a delete. REGOLITH_ERR_READ_ONLY_TXN on a read-only txn. */
int32_t regolith_txn_delete(RegolithTxn *txn, const uint8_t *key,
                            size_t key_len);

/* Create an iterator over the transaction: its buffered writes merged
 * over its begin snapshot. The write set is sampled on first advance and
 * on each rebuild, not here. See ordering contracts 1 and 3 above. */
int32_t regolith_txn_iter(RegolithTxn *txn, const RegolithIterOptions *opts,
                          RegolithIter **out);

/* Validate and apply the transaction. REGOLITH_ERR_TXN_CONFLICT when
 * another writer touched a validated key first; the transaction is
 * resolved either way and the caller should retry from a new one.
 * REGOLITH_ERR_DISCARDED if already resolved. The handle stays
 * allocated; release it with regolith_txn_free. */
int32_t regolith_txn_commit(RegolithTxn *txn);

/* Discard the transaction, dropping its buffered writes. Idempotent:
 * REGOLITH_OK even if already resolved. The handle stays allocated;
 * release it with regolith_txn_free. */
int32_t regolith_txn_discard(RegolithTxn *txn);

/* Free the transaction handle, discarding first if still unresolved.
 * Exactly once per handle, after all its iterators are closed. */
int32_t regolith_txn_free(RegolithTxn *txn);

/* --------------------------------------------------------------------
 * Iterator.
 *
 * An iterator starts un-positioned with a pending reset, so the first
 * regolith_iter_next positions it on the first in-range entry rather
 * than advancing past it. This matches corekv's Iterator contract.
 * ------------------------------------------------------------------ */

/* Advance (or, after construction/reset, position at the start of the
 * range). Writes 0 or 1 to *valid. */
int32_t regolith_iter_next(RegolithIter *it, uint8_t *valid);

/* Seek, clamped to the configured range: a target below `start` becomes
 * `start`, and under `reverse` a target at or above `end` becomes the top
 * of the range. Forward lands on the smallest key >= the target, reverse
 * on the greatest key <= it. Writes 0 or 1 to *valid. Clears any pending
 * reset, so a following regolith_iter_next advances from here. */
int32_t regolith_iter_seek(RegolithIter *it, const uint8_t *key,
                           size_t key_len, uint8_t *valid);

/* Mark for re-iteration: the next regolith_iter_next returns to the
 * start of the range. Never fails on a live handle. */
int32_t regolith_iter_reset(RegolithIter *it);

/* Copy out the current key. REGOLITH_ERR_NOT_FOUND when the iterator is
 * not positioned on a valid in-range entry. */
int32_t regolith_iter_key(RegolithIter *it, uint8_t **key, size_t *key_len);

/* Copy out the current value, or a zero-length buffer when the iterator
 * was created with keys_only. REGOLITH_ERR_NOT_FOUND when not
 * positioned on a valid in-range entry. */
int32_t regolith_iter_value(RegolithIter *it, uint8_t **val, size_t *val_len);

/* Close the iterator and free its handle. Exactly once per handle. */
int32_t regolith_iter_close(RegolithIter *it);

#ifdef __cplusplus
}
#endif

#endif /* REGOLITH_FFI_H */
