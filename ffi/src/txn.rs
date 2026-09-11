//! Transaction entry points.
//!
//! Two wrinkles shape this module:
//!
//! 1. `OwnedTransaction::commit`/`rollback` consume the transaction, but
//!    the caller holds it through a heap handle it does not own. So the
//!    transaction lives in an `Option` that the resolving call takes.
//! 2. regolith has no read-only transaction mode. `readonly` is carried
//!    here and `Set`/`Delete` are rejected with [`READ_ONLY_TXN`].

use std::sync::{Arc, RwLock};

use regolith::OwnedTransaction;

use crate::iter::{RegolithIter, RegolithIterOptions};
use crate::{
    DISCARDED, Failure, NOT_FOUND, READ_ONLY_TXN, RegolithError, RegolithValue, guard, in_bytes,
    out_bool, out_borrowed,
};

/// The shared body of a transaction handle.
///
/// Shared because iterators created from a transaction hold an `Arc` of
/// it: they need to rebuild their scan stream on `Seek`/`Reset`, which
/// requires the transaction back.
pub(crate) struct TxnInner {
    /// `None` once committed or discarded. An `RwLock` rather than a
    /// `Mutex` because regolith's transaction reads and writes both take
    /// `&self`, so they can run concurrently; only resolving needs
    /// exclusivity.
    txn: RwLock<Option<OwnedTransaction>>,
    readonly: bool,
}

impl TxnInner {
    pub(crate) fn new(txn: OwnedTransaction, readonly: bool) -> Self {
        Self {
            txn: RwLock::new(Some(txn)),
            readonly,
        }
    }

    /// Run `body` against the live transaction, or fail with
    /// [`DISCARDED`] if it has already been resolved.
    pub(crate) fn with<R>(&self, body: impl FnOnce(&OwnedTransaction) -> R) -> Result<R, Failure> {
        let Ok(guard) = self.txn.read() else {
            return Err(Failure::other(
                "transaction unusable after a panic; discard it and begin a new one",
            ));
        };
        match guard.as_ref() {
            Some(txn) => Ok(body(txn)),
            None => Err(Failure::code(DISCARDED)),
        }
    }

    fn take(&self) -> Result<OwnedTransaction, Failure> {
        let Ok(mut guard) = self.txn.write() else {
            return Err(Failure::other(
                "transaction unusable after a panic; discard it and begin a new one",
            ));
        };
        guard.take().ok_or_else(|| Failure::code(DISCARDED))
    }
}

/// Opaque transaction handle.
pub struct RegolithTxn {
    pub(crate) inner: Arc<TxnInner>,
}

macro_rules! txn_ref {
    ($ptr:expr) => {
        match unsafe { $ptr.as_ref() } {
            Some(txn) => txn,
            None => return Err(Failure::invalid_arg("null txn handle")),
        }
    };
}

/// Read `key` through the transaction, seeing its own uncommitted writes
/// and lending the caller the bytes rather than copying them.
///
/// `get_slice` for the same reason as [`crate::regolith_db_get_borrowed`].
/// The slice a transaction hands back is independent of the transaction:
/// it holds a reference count on the block, arena or buffered write that
/// owns the bytes, so it outlives the read lock taken here.
///
/// Returns [`NOT_FOUND`] with no handle produced when absent.
///
/// # Safety
/// `key` must be valid for `key_len` bytes. On [`OK`](crate::OK),
/// `(*val, *val_len)` is borrowed until `regolith_release_value` is
/// called on `*handle`, which the caller must always do. `err` must be
/// null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_get_borrowed(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    val: *mut *const u8,
    val_len: *mut usize,
    value_handle: *mut *mut RegolithValue,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        let Some(slice) = handle.inner.with(|t| t.get_slice(key))?? else {
            return Err(Failure::code(NOT_FOUND));
        };
        unsafe { out_borrowed(slice, val, val_len, value_handle) }
    })
}

/// Test for the presence of `key` through the transaction.
///
/// regolith's `Transaction` has no `has`, so this is `get_slice` with the
/// value discarded.
///
/// # Safety
/// As [`regolith_txn_get_borrowed`]; `found` must be writable; `err`
/// must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_has(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    found: *mut u8,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        let found_value = handle
            .inner
            .with(|t| t.get_slice(key).map(|v| v.is_some()))??;
        unsafe { out_bool(found_value, found) }
    })
}

/// Buffer a write in the transaction.
///
/// # Safety
/// `key` and `value` must be valid for their lengths; neither is
/// retained past the call. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_set(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        if handle.inner.readonly {
            return Err(Failure::code(READ_ONLY_TXN));
        }
        let (Some(key), Some(value)) = (unsafe { in_bytes(key, key_len) }, unsafe {
            in_bytes(value, value_len)
        }) else {
            return Err(Failure::invalid_arg("null key or value"));
        };
        handle.inner.with(|t| t.put(key, value))??;
        Ok(())
    })
}

/// Buffer a delete in the transaction.
///
/// # Safety
/// `key` must be valid for `key_len` bytes. `err` must be null or
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_delete(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        if handle.inner.readonly {
            return Err(Failure::code(READ_ONLY_TXN));
        }
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            return Err(Failure::invalid_arg("null key"));
        };
        handle.inner.with(|t| t.delete(key))??;
        Ok(())
    })
}

/// Create an iterator over the transaction: its buffered writes merged
/// over the snapshot it began at.
///
/// The set of buffered writes is captured when the iterator is created,
/// so writes made afterwards are not visible to it. Close the iterator
/// before committing or discarding the transaction.
///
/// # Safety
/// `opts` must be null or valid for the duration of the call; `out` must
/// be writable. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_iter(
    txn: *mut RegolithTxn,
    opts: *const RegolithIterOptions,
    out: *mut *mut RegolithIter,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        if out.is_null() {
            return Err(Failure::invalid_arg("null out-param"));
        }
        let bounds = unsafe { crate::iter::bounds_from(opts) };
        let iter = RegolithIter::over_txn(Arc::clone(&handle.inner), bounds)?;
        unsafe { *out = Box::into_raw(Box::new(iter)) };
        Ok(())
    })
}

/// Validate and apply the transaction. Returns [`crate::TXN_CONFLICT`] if
/// another writer touched a validated key first, in which case the
/// transaction is resolved and the caller should retry from a new one.
/// A conflict carries no detail; the code is the message.
///
/// The handle stays allocated afterwards; release it with
/// [`regolith_txn_free`].
///
/// # Safety
/// `txn` must be a live handle. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_commit(
    txn: *mut RegolithTxn,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        handle.inner.take()?.commit()?;
        Ok(())
    })
}

/// Discard the transaction, dropping its buffered writes. Idempotent:
/// discarding an already-resolved transaction is [`OK`](crate::OK).
///
/// The handle stays allocated afterwards; release it with
/// [`regolith_txn_free`].
///
/// # Safety
/// `txn` must be a live handle. `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_discard(
    txn: *mut RegolithTxn,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        let handle = txn_ref!(txn);
        match handle.inner.take() {
            Ok(owned) => {
                owned.rollback();
                Ok(())
            }
            // Already committed or discarded: nothing to do.
            Err(failure) if failure.status == DISCARDED => Ok(()),
            Err(failure) => Err(failure),
        }
    })
}

/// Free the transaction handle, discarding the transaction first if it is
/// still unresolved. Must be called exactly once per handle, after every
/// iterator derived from it has been closed.
///
/// # Safety
/// `txn` must be a handle from `regolith_db_txn` that has not been freed.
/// `err` must be null or writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_free(
    txn: *mut RegolithTxn,
    err: *mut *mut RegolithError,
) -> i32 {
    guard(err, || {
        if txn.is_null() {
            return Err(Failure::invalid_arg("null txn handle"));
        }
        let handle = unsafe { Box::from_raw(txn) };
        if let Ok(owned) = handle.inner.take() {
            owned.rollback();
        }
        Ok(())
    })
}
