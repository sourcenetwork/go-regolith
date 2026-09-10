//! Store-level entry points: open/close, point reads and writes,
//! `DropAll`, and the two handle factories (iterator, transaction).

use std::sync::Arc;

use regolith::{IsolationLevel, OptimisticTransactionDb};

use crate::iter::{RegolithIter, RegolithIterOptions};
use crate::options::{RegolithOptions, options_from};
use crate::txn::{RegolithTxn, TxnInner};
use crate::{
    INVALID_ARG, NOT_FOUND, OK, RegolithValue, guard, in_bytes, out_bool, out_borrowed, set_error,
    status_of,
};

/// Opaque store handle.
///
/// Always an [`OptimisticTransactionDb`]: non-transactional operations go
/// straight to the `Db` underneath it, so one handle serves both modes
/// and there is no second open path to keep consistent. Optimistic +
/// snapshot isolation is the closest match to badger's semantics, which
/// is what the corekv test suite expects.
pub struct RegolithDb {
    /// `Arc` because `begin_transaction_owned` takes `&Arc<Self>`.
    pub(crate) inner: Arc<OptimisticTransactionDb>,
}

/// Resolve a handle pointer to a reference, or fail.
macro_rules! db_ref {
    ($ptr:expr) => {
        match unsafe { $ptr.as_ref() } {
            Some(db) => db,
            None => {
                set_error("null db handle");
                return INVALID_ARG;
            }
        }
    };
}

/// Open (or create) a store at `path` with regolith's default options.
///
/// Exactly [`regolith_db_open_with_options`] with a null `opts`.
///
/// # Safety
/// `path` must be valid for `path_len` bytes; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_open(
    path: *const u8,
    path_len: usize,
    out: *mut *mut RegolithDb,
) -> i32 {
    unsafe { regolith_db_open_with_options(path, path_len, std::ptr::null(), out) }
}

/// Open (or create) a store at `path` with the engine options in `opts`.
///
/// A null `opts`, or one whose `present` mask is empty, is the defaults
/// path. Only the fields whose presence bit is set are applied; see
/// [`RegolithOptions`].
///
/// Invalid values are rejected rather than clamped. An unknown enum
/// discriminant or presence bit is [`INVALID_ARG`] from this layer; a
/// value regolith itself refuses comes back from its own
/// `Options::validate`, which runs before any filesystem work. Either
/// way the detail message names the offending field.
///
/// # Safety
/// `path` must be valid for `path_len` bytes; `opts` must be null or
/// point at a valid [`RegolithOptions`] for the duration of the call;
/// `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_open_with_options(
    path: *const u8,
    path_len: usize,
    opts: *const RegolithOptions,
    out: *mut *mut RegolithDb,
) -> i32 {
    guard(|| {
        if out.is_null() {
            set_error("null out-param");
            return INVALID_ARG;
        }
        let Some(bytes) = (unsafe { in_bytes(path, path_len) }) else {
            set_error("null path");
            return INVALID_ARG;
        };
        let Ok(path) = std::str::from_utf8(bytes) else {
            set_error("path is not valid utf-8");
            return INVALID_ARG;
        };
        let options = match unsafe { options_from(opts) } {
            Ok(options) => options,
            Err(status) => return status,
        };
        match OptimisticTransactionDb::open(path, options) {
            Ok(db) => {
                let handle = Box::new(RegolithDb {
                    inner: Arc::new(db),
                });
                unsafe { *out = Box::into_raw(handle) };
                OK
            }
            Err(e) => status_of(&e),
        }
    })
}

/// Close the store and free the handle. The handle is invalid afterwards
/// even if a non-OK status is returned.
///
/// # Safety
/// `db` must be a handle from [`regolith_db_open`], not yet closed. All
/// iterators and transactions derived from it must already be closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_close(db: *mut RegolithDb) -> i32 {
    guard(|| {
        if db.is_null() {
            set_error("null db handle");
            return INVALID_ARG;
        }
        let handle = unsafe { Box::from_raw(db) };
        match handle.inner.db().close() {
            Ok(()) => OK,
            Err(e) => status_of(&e),
        }
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
/// `key` must be valid for `key_len` bytes. On [`OK`], `(*val, *val_len)`
/// is **borrowed**: it stays valid only until `regolith_release_value` is
/// called on `*handle`, which the caller must always do. See
/// [`crate::RegolithValue`] for why promptly.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_get_borrowed(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    val: *mut *const u8,
    val_len: *mut usize,
    value_handle: *mut *mut RegolithValue,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.db().get_slice(key) {
            Ok(Some(slice)) => unsafe { out_borrowed(slice, val, val_len, value_handle) },
            Ok(None) => NOT_FOUND,
            Err(e) => status_of(&e),
        }
    })
}

/// Test for the presence of `key`, writing 0/1 to `found`.
///
/// # Safety
/// As [`regolith_db_get_borrowed`]; `found` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_has(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    found: *mut u8,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.db().has(key) {
            Ok(found_value) => unsafe { out_bool(found_value, found) },
            Err(e) => status_of(&e),
        }
    })
}

/// Write `value` at `key`, overwriting any existing entry.
///
/// # Safety
/// `key` and `value` must be valid for their lengths. Neither is
/// retained past the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_set(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        let (Some(key), Some(value)) =
            (unsafe { in_bytes(key, key_len) }, unsafe {
                in_bytes(value, value_len)
            })
        else {
            set_error("null key or value");
            return INVALID_ARG;
        };
        match handle.inner.db().put(key, value) {
            Ok(()) => OK,
            Err(e) => status_of(&e),
        }
    })
}

/// Delete `key`. Deleting an absent key is not an error.
///
/// # Safety
/// `key` must be valid for `key_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_delete(
    db: *mut RegolithDb,
    key: *const u8,
    key_len: usize,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.db().delete(key) {
            Ok(()) => OK,
            Err(e) => status_of(&e),
        }
    })
}

/// Delete every entry in the store (`corekv.Dropable`).
///
/// # Safety
/// `db` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_drop_all(db: *mut RegolithDb) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        match handle.inner.db().drop_all() {
            Ok(()) => OK,
            Err(e) => status_of(&e),
        }
    })
}

/// Create an iterator over a snapshot of the store taken now.
///
/// # Safety
/// `opts` must be null (meaning "defaults") or point at a valid
/// [`RegolithIterOptions`] whose byte pointers are valid for the
/// duration of the call. `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_iter(
    db: *mut RegolithDb,
    opts: *const RegolithIterOptions,
    out: *mut *mut RegolithIter,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        if out.is_null() {
            set_error("null out-param");
            return INVALID_ARG;
        }
        let bounds = match unsafe { crate::iter::bounds_from(opts) } {
            Ok(bounds) => bounds,
            Err(status) => return status,
        };
        let iter = RegolithIter::over_snapshot(handle.inner.db().snapshot(), bounds);
        unsafe { *out = Box::into_raw(Box::new(iter)) };
        OK
    })
}

/// Begin a transaction. `readonly` is enforced by this layer: regolith
/// has no read-only transaction mode, so the flag is carried on the
/// handle and writes are rejected with `REGOLITH_ERR_READ_ONLY_TXN`.
///
/// # Safety
/// `db` must be live; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_db_txn(
    db: *mut RegolithDb,
    readonly: u8,
    out: *mut *mut RegolithTxn,
) -> i32 {
    guard(|| {
        let handle = db_ref!(db);
        if out.is_null() {
            set_error("null out-param");
            return INVALID_ARG;
        }
        let owned = handle
            .inner
            .begin_transaction_owned(IsolationLevel::SnapshotIsolation);
        let txn = RegolithTxn {
            inner: Arc::new(TxnInner::new(owned, readonly != 0)),
        };
        unsafe { *out = Box::into_raw(Box::new(txn)) };
        OK
    })
}
