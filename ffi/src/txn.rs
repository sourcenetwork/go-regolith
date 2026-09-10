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
    DISCARDED, INVALID_ARG, NOT_FOUND, OK, OTHER, READ_ONLY_TXN, guard, in_bytes, out_bool,
    out_bytes, set_error, txn_status_of,
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
    pub(crate) fn with<R>(&self, body: impl FnOnce(&OwnedTransaction) -> R) -> Result<R, i32> {
        let Ok(guard) = self.txn.read() else {
            set_error("transaction lock poisoned");
            return Err(OTHER);
        };
        match guard.as_ref() {
            Some(txn) => Ok(body(txn)),
            None => Err(DISCARDED),
        }
    }

    fn take(&self) -> Result<OwnedTransaction, i32> {
        let Ok(mut guard) = self.txn.write() else {
            set_error("transaction lock poisoned");
            return Err(OTHER);
        };
        guard.take().ok_or(DISCARDED)
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
            None => {
                set_error("null txn handle");
                return INVALID_ARG;
            }
        }
    };
}

/// Read `key` through the transaction, seeing its own uncommitted writes.
///
/// # Safety
/// `key` must be valid for `key_len` bytes. On [`OK`] the caller owns
/// `(*val, *val_len)` and releases it with `regolith_free_buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_get(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    val: *mut *mut u8,
    val_len: *mut usize,
) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.with(|t| t.get(key)) {
            Err(status) => status,
            Ok(Ok(Some(value))) => unsafe { out_bytes(value, val, val_len) },
            Ok(Ok(None)) => NOT_FOUND,
            Ok(Err(e)) => txn_status_of(&e),
        }
    })
}

/// Test for the presence of `key` through the transaction.
///
/// regolith's `Transaction` has no `has`, so this is `get_slice` with the
/// value discarded - still cheaper than `get`, which copies.
///
/// # Safety
/// As [`regolith_txn_get`]; `found` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_has(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    found: *mut u8,
) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.with(|t| t.get_slice(key).map(|v| v.is_some())) {
            Err(status) => status,
            Ok(Ok(found_value)) => unsafe { out_bool(found_value, found) },
            Ok(Err(e)) => txn_status_of(&e),
        }
    })
}

/// Buffer a write in the transaction.
///
/// # Safety
/// `key` and `value` must be valid for their lengths; neither is
/// retained past the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_set(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        if handle.inner.readonly {
            return READ_ONLY_TXN;
        }
        let (Some(key), Some(value)) =
            (unsafe { in_bytes(key, key_len) }, unsafe {
                in_bytes(value, value_len)
            })
        else {
            set_error("null key or value");
            return INVALID_ARG;
        };
        match handle.inner.with(|t| t.put(key, value)) {
            Err(status) => status,
            Ok(Ok(())) => OK,
            Ok(Err(e)) => txn_status_of(&e),
        }
    })
}

/// Buffer a delete in the transaction.
///
/// # Safety
/// `key` must be valid for `key_len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_delete(
    txn: *mut RegolithTxn,
    key: *const u8,
    key_len: usize,
) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        if handle.inner.readonly {
            return READ_ONLY_TXN;
        }
        let Some(key) = (unsafe { in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match handle.inner.with(|t| t.delete(key)) {
            Err(status) => status,
            Ok(Ok(())) => OK,
            Ok(Err(e)) => txn_status_of(&e),
        }
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
/// be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_iter(
    txn: *mut RegolithTxn,
    opts: *const RegolithIterOptions,
    out: *mut *mut RegolithIter,
) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        if out.is_null() {
            set_error("null out-param");
            return INVALID_ARG;
        }
        let bounds = match unsafe { crate::iter::bounds_from(opts) } {
            Ok(bounds) => bounds,
            Err(status) => return status,
        };
        match RegolithIter::over_txn(Arc::clone(&handle.inner), bounds) {
            Ok(iter) => {
                unsafe { *out = Box::into_raw(Box::new(iter)) };
                OK
            }
            Err(status) => status,
        }
    })
}

/// Validate and apply the transaction. Returns [`crate::TXN_CONFLICT`] if
/// another writer touched a validated key first, in which case the
/// transaction is resolved and the caller should retry from a new one.
///
/// The handle stays allocated afterwards; release it with
/// [`regolith_txn_free`].
///
/// # Safety
/// `txn` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_commit(txn: *mut RegolithTxn) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        match handle.inner.take() {
            Err(status) => status,
            Ok(owned) => match owned.commit() {
                Ok(()) => OK,
                Err(e) => txn_status_of(&e),
            },
        }
    })
}

/// Discard the transaction, dropping its buffered writes. Idempotent:
/// discarding an already-resolved transaction is [`OK`].
///
/// The handle stays allocated afterwards; release it with
/// [`regolith_txn_free`].
///
/// # Safety
/// `txn` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_discard(txn: *mut RegolithTxn) -> i32 {
    guard(|| {
        let handle = txn_ref!(txn);
        match handle.inner.take() {
            Ok(owned) => {
                owned.rollback();
                OK
            }
            // Already committed or discarded: nothing to do.
            Err(DISCARDED) => OK,
            Err(status) => status,
        }
    })
}

/// Free the transaction handle, discarding the transaction first if it is
/// still unresolved. Must be called exactly once per handle, after every
/// iterator derived from it has been closed.
///
/// # Safety
/// `txn` must be a handle from `regolith_db_txn` that has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_txn_free(txn: *mut RegolithTxn) -> i32 {
    guard(|| {
        if txn.is_null() {
            set_error("null txn handle");
            return INVALID_ARG;
        }
        let handle = unsafe { Box::from_raw(txn) };
        if let Ok(owned) = handle.inner.take() {
            owned.rollback();
        }
        OK
    })
}
