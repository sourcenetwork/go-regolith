//! Store-level entry points: open/close, point reads, point and batch
//! writes, `DropAll`, and the two handle factories (iterator, transaction).

use std::sync::Arc;

use regolith::OptimisticTransactionDb;

use crate::batch;
use crate::iter::{RegolithIter, RegolithIterOptions};
use crate::options::{RegolithOptions, options_from};
use crate::txn::{RegolithTxn, TxnInner};
use crate::{
    Failure, NOT_FOUND, RegolithError, RegolithValue, guard, in_bytes, out_bool, out_borrowed,
};

/// Opaque store handle.
///
/// Always an [`OptimisticTransactionDb`]: non-transactional operations go
/// straight to the `Db` underneath it, so one handle serves both modes
/// and there is no second open path to keep consistent. Optimistic +
/// snapshot isolation is the closest match to badger's semantics, which
/// is what the corekv test suite expects.
///
/// The isolation level is held by the `OptimisticTransactionDb` itself
/// (`with_isolation` at open), not by a field here, so there is only one
/// place for [`regolith_db_txn`] to read it from.
pub struct RegolithDb {
    /// `Arc` because `begin_transaction_owned` takes `&Arc<Self>`.
    pub(crate) inner: Arc<OptimisticTransactionDb>,
}

/// Resolve a handle pointer to a reference, or fail.
macro_rules! db_ref {
    ($ptr:expr) => {
        match unsafe { $ptr.as_ref() } {
            Some(db) => db,
            None => return Err(Failure::invalid_arg("null db handle")),
        }
    };
}

/// Open (or create) a store at `path` with regolith's default options.
///
/// Exactly [`regolith_db_open_with_options`] with a null `opts`.
///
/// # Safety
/// `path` must be valid for `path_len` bytes; `out` must be writable;
/// `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_open(
    path: *const u8,
    path_len: usize,
    out: *mut *mut RegolithDb,
    err: *mut *mut RegolithError,
) -> i32 {
    unsafe { regolith_db_open_with_options(path, path_len, std::ptr::null(), out, err) }
}

/// Open (or create) a store at `path` with the engine options in `opts`.
///
/// A null `opts`, or one whose `present` mask is empty, is the defaults
/// path. Only the fields whose presence bit is set are applied; see
/// [`RegolithOptions`].
///
/// Invalid values are rejected rather than clamped. An unknown enum
/// discriminant or presence bit is [`INVALID_ARG`](crate::INVALID_ARG) from this layer; a
/// value regolith itself refuses comes back from its own
/// `Options::validate`, which runs before any filesystem work. Either
/// way the detail message names the offending field.
///
/// # Safety
/// `path` must be valid for `path_len` bytes; `opts` must be null or
/// point at a valid [`RegolithOptions`] for the duration of the call;
/// `out` must be writable; `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_open_with_options(
    path: *const u8,
    path_len: usize,
    opts: *const RegolithOptions,
    out: *mut *mut RegolithDb,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Failure::invalid_arg("null out-param"));
        }
        let Some(bytes) = (unsafe { in_bytes(path, path_len) }) else {
            return Err(Failure::invalid_arg("null path"));
        };
        let Ok(path) = std::str::from_utf8(bytes) else {
            return Err(Failure::invalid_arg("path is not valid utf-8"));
        };
        let options = unsafe { options_from(opts) }?;
        let db =
            OptimisticTransactionDb::open(path, options.engine)?.with_isolation(options.isolation);
        let handle = Box::new(RegolithDb {
            inner: Arc::new(db),
        });
        unsafe { *out = Box::into_raw(handle) };
        Ok(())
    })
}

/// Close the store and free the handle. The handle is invalid afterwards
/// even if a non-OK status is returned.
///
/// # Safety
/// `db` must be a handle from [`regolith_db_open`], not yet closed. All
/// iterators and transactions derived from it must already be closed.
/// `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_close(
    db: *mut RegolithDb,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        if db.is_null() {
            return Err(Failure::invalid_arg("null db handle"));
        }
        let handle = unsafe { Box::from_raw(db) };
        handle.inner.db().close()?;
        Ok(())
    })
}

/// Read `key`, lending the caller the engine's own bytes.
///
/// `get_slice` rather than `get`: `Db::get` is `get_slice` followed by
/// `DbSlice::into_vec`, which can only move the bytes for a heap-owned
/// slice at refcount 1. For a value read out of an SSTable block or a
/// memtable arena - the common case - it copies and allocates. Handing
/// the slice out instead leaves the caller's copy as the only one.
///
/// Returns [`NOT_FOUND`] with no handle produced when absent.
///
/// # Safety
/// `key` must be valid for `key_len` bytes. On [`OK`](crate::OK),
/// `(*val, *val_len)` is **borrowed**: it stays valid only until
/// `regolith_release_value` is called on `*handle`, which the caller
/// must always do. See [`crate::RegolithValue`] for why promptly. `err`
/// must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_get_borrowed(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    val: *mut *const u8,
    val_len: *mut usize,
    value_handle: *mut *mut RegolithValue,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        let Some(slice) = handle.inner.db().get_slice(key)? else {
            return Err(Failure::code(NOT_FOUND));
        };
        unsafe { out_borrowed(slice, val, val_len, value_handle) }
    })
}

/// Test for the presence of `key`, writing 0/1 to `found`.
///
/// # Safety
/// As [`regolith_db_get_borrowed`]; `found` must be writable; `err` must
/// be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_has(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    found: *mut u8,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        let found_value = handle.inner.db().has(key)?;
        unsafe { out_bool(found_value, found) }
    })
}

/// Write `value` at `key`, overwriting any existing entry.
///
/// # Safety
/// `key` and `value` must be valid for their lengths. Neither is
/// retained past the call. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_set(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        let (Some(key), Some(value)) = (unsafe { in_bytes(key, key_len) }, unsafe {
            in_bytes(value, value_len)
        }) else {
            return Err(Failure::invalid_arg("null key or value"));
        };
        handle.inner.db().put(key, value)?;
        Ok(())
    })
}

/// Delete `key`. Deleting an absent key is not an error.
///
/// # Safety
/// `key` must be valid for `key_len` bytes. `err` must be null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_delete(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        handle.inner.db().delete(key)?;
        Ok(())
    })
}

/// Apply a batch of sets and deletes atomically: every op lands or none
/// does, as one WAL record and one contiguous sequence range. There is no
/// conflict check and no snapshot; this is `Db::write`, not a
/// transaction, and it passes write-stall admission like a plain put.
///
/// `ops` is a frame of `ops_len` bytes, decoded by `crate::batch`. A
/// zero-length frame is an empty batch and succeeds without writing. A
/// frame that does not decode is [`INVALID_ARG`](crate::INVALID_ARG) with a
/// detail naming the op, and nothing is written. Ops on the same key apply
/// in frame order, so the last one wins. Key and value sizes are checked
/// by the engine before anything is applied, so a refused batch writes
/// nothing either. Nor does a batch whose WAL record (`crate::batch`'s own
/// accounting: the frame's bytes plus 8 per set, 12 per delete, plus 4)
/// would exceed 1073741824 bytes; that, too, is `INVALID_ARG` naming the
/// op, checked before any op reaches the engine.
///
/// # Safety
/// `ops` must be valid for `ops_len` bytes for the duration of the call
/// and is not retained. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_write(
    db: *mut RegolithDb,
    ops: *const u8,
    ops_len: usize,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        let Some(frame) = (unsafe { in_bytes(ops, ops_len) }) else {
            return Err(Failure::invalid_arg("null write batch"));
        };
        let batch = batch::decode(frame)?;
        handle.inner.db().write(batch)?;
        Ok(())
    })
}

/// Delete every entry in the store (`corekv.Dropable`).
///
/// # Safety
/// `db` must be a live handle. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_drop_all(
    db: *mut RegolithDb,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        handle.inner.db().drop_all()?;
        Ok(())
    })
}

/// Create an iterator over a snapshot of the store taken now.
///
/// # Safety
/// `opts` must be null (meaning "defaults") or point at a valid
/// [`RegolithIterOptions`] whose byte pointers are valid for the
/// duration of the call. `out` must be writable. `err` must be null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_iter(
    db: *mut RegolithDb,
    opts: *const RegolithIterOptions,
    out: *mut *mut RegolithIter,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        if out.is_null() {
            return Err(Failure::invalid_arg("null out-param"));
        }
        let bounds = unsafe { crate::iter::bounds_from(opts) };
        let iter = RegolithIter::over_snapshot(handle.inner.db().snapshot(), bounds);
        unsafe { *out = Box::into_raw(Box::new(iter)) };
        Ok(())
    })
}

/// Begin a transaction at the store's configured isolation level - the
/// one `isolation` in [`RegolithOptions`] selected at open, which is
/// regolith's own default when it was left unset.
///
/// `readonly` is enforced by this layer: regolith has no read-only
/// transaction mode, so the flag is carried on the handle and writes are
/// rejected with `REGOLITH_ERR_READ_ONLY_TXN`.
///
/// # Safety
/// `db` must be live; `out` must be writable. `err` must be null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_txn(
    db: *mut RegolithDb,
    readonly: u8,
    out: *mut *mut RegolithTxn,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = db_ref!(db);
        if out.is_null() {
            return Err(Failure::invalid_arg("null out-param"));
        }
        let owned = handle
            .inner
            .begin_transaction_owned(handle.inner.isolation());
        let txn = RegolithTxn {
            inner: Arc::new(TxnInner::new(owned, readonly != 0)),
        };
        unsafe { *out = Box::into_raw(Box::new(txn)) };
        Ok(())
    })
}
