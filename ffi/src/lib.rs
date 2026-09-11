//! A flat C ABI over the `regolith` embedded key-value engine, shaped to
//! exactly what `github.com/sourcenetwork/corekv` needs from a store.
//!
//! The contract, in one place (see `include/regolith_ffi.h` for the same
//! thing in C):
//!
//! * Every entry point returns an `i32` status. `0` is OK; see the
//!   `REGOLITH_*` constants for the rest.
//! * Every entry point catches Rust panics and returns [`PANIC`] rather
//!   than unwinding into the caller's frame.
//! * Handles are opaque pointers created here and destroyed by the
//!   matching `_close`/`_free`. A null handle is an error, never a
//!   dereference.
//! * Input bytes cross as `(*const u8, usize)` and are **not retained**
//!   past the call, so the caller may pass Go memory directly.
//! * Output bytes are allocated here; the caller copies them and then
//!   calls [`regolith_free_buf`]. Output strings are freed with
//!   [`regolith_free_string`].
//! * Point reads are the exception: they hand back a **borrowed**
//!   pointer into memory the engine already owns, plus a
//!   [`RegolithValue`] handle holding the reference count that keeps it
//!   there. Nothing is copied on this side. The caller copies and then
//!   calls [`regolith_release_value`] on every path, promptly: see
//!   [`RegolithValue`].
//!
//! `unsafe` is unavoidable in an FFI shim. It is confined to the raw
//! pointer helpers in this module plus one documented lifetime extension
//! in `iter.rs`.

use std::cell::RefCell;
use std::ffi::CString;
use std::panic::{AssertUnwindSafe, catch_unwind};

use regolith::DbSlice;

mod db;
mod iter;
mod options;
mod txn;

pub use db::*;
pub use iter::*;
pub use options::*;
pub use txn::*;

// ---------------------------------------------------------------------
// Status codes. Kept numerically identical to the header.
// ---------------------------------------------------------------------

/// Success.
pub const OK: i32 = 0;
/// No entry for the requested key.
pub const NOT_FOUND: i32 = 1;
/// The database has been closed.
pub const DB_CLOSED: i32 = 2;
/// An optimistic transaction lost a commit-time validation race.
pub const TXN_CONFLICT: i32 = 3;
/// A write was attempted on a read-only transaction.
pub const READ_ONLY_TXN: i32 = 4;
/// The transaction has already been committed or discarded.
pub const DISCARDED: i32 = 5;
/// A null pointer or otherwise unusable argument.
pub const INVALID_ARG: i32 = 6;
/// A Rust panic was caught at the boundary.
pub const PANIC: i32 = 7;
/// Anything else; call `regolith_last_error_message` for detail.
pub const OTHER: i32 = 8;

// ---------------------------------------------------------------------
// Thread-local error detail.
// ---------------------------------------------------------------------

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Record a detail message for the status just returned. Only consulted
/// by the caller for [`OTHER`], [`PANIC`] and [`TXN_CONFLICT`].
pub(crate) fn set_error(message: impl Into<Vec<u8>>) {
    // Interior nul bytes would truncate the message; strip them rather
    // than losing the message entirely.
    let mut bytes: Vec<u8> = message.into();
    bytes.retain(|b| *b != 0);
    let cstring = CString::new(bytes).expect("nul bytes were just removed");
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(cstring));
}

/// Map a regolith engine error onto a status code, recording detail.
pub(crate) fn status_of(err: &regolith::Error) -> i32 {
    set_error(err.to_string());
    match err {
        regolith::Error::Closed => DB_CLOSED,
        regolith::Error::InvalidArgument(_) => INVALID_ARG,
        _ => OTHER,
    }
}

/// Map a regolith transaction error onto a status code, recording detail.
pub(crate) fn txn_status_of(err: &regolith::TransactionError) -> i32 {
    set_error(err.to_string());
    match err {
        // Both are "someone else got there first, roll back and retry",
        // which is exactly `corekv.ErrTxnConflict`.
        regolith::TransactionError::Conflict { .. } | regolith::TransactionError::Busy(_) => {
            TXN_CONFLICT
        }
        regolith::TransactionError::UnsupportedRangeDelete
        | regolith::TransactionError::NoSavepoint => INVALID_ARG,
        regolith::TransactionError::Io(_) => OTHER,
    }
}

/// Run `body` with panics caught. Every `extern "C"` entry point goes
/// through this, so no unwind can reach the caller's frame.
pub(crate) fn guard(body: impl FnOnce() -> i32) -> i32 {
    // `AssertUnwindSafe`: a caught panic leaves the handles reachable but
    // possibly mid-update. We return a distinct status and the caller's
    // contract is to abandon the handle, so no torn state is observed.
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
            let detail = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic".to_string()
            };
            set_error(format!("panic in regolith-ffi: {detail}"));
            PANIC
        }
    }
}

// ---------------------------------------------------------------------
// Raw pointer helpers.
// ---------------------------------------------------------------------

/// View caller-owned input bytes. `len == 0` yields an empty slice
/// regardless of the pointer, which is how a nil Go slice arrives.
///
/// # Safety
/// `ptr` must be valid for `len` bytes for the duration of the call.
pub(crate) unsafe fn in_bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len == 0 {
        Some(&[])
    } else if ptr.is_null() {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

/// Same as [`in_bytes`] but an empty/absent value maps to `None`, for
/// the optional bound arguments where "not provided" is meaningful.
///
/// # Safety
/// As [`in_bytes`].
pub(crate) unsafe fn in_opt_bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if ptr.is_null() || len == 0 {
        None
    } else {
        Some(unsafe { std::slice::from_raw_parts(ptr, len) })
    }
}

/// Hand ownership of `bytes` to the caller via `(out_ptr, out_len)`.
/// The caller must release it with [`regolith_free_buf`].
///
/// # Safety
/// `out_ptr` and `out_len` must be valid writable pointers.
pub(crate) unsafe fn out_bytes(bytes: Vec<u8>, out_ptr: *mut *mut u8, out_len: *mut usize) -> i32 {
    if out_ptr.is_null() || out_len.is_null() {
        set_error("null out-param");
        return INVALID_ARG;
    }
    // `into_boxed_slice` makes capacity == len, which is what
    // `regolith_free_buf` reconstructs.
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed).cast::<u8>();
    unsafe {
        *out_ptr = ptr;
        *out_len = len;
    }
    OK
}

/// Opaque handle keeping a borrowed value's bytes alive.
///
/// It boxes nothing but a [`DbSlice`], which is a refcounted view of
/// bytes the engine already holds: an SSTable data block, a memtable
/// arena chunk, or a heap buffer the engine made. Holding one **pins**
/// that owner - a slice over an SSTable block keeps the block resident
/// even after the block cache evicts it - so a handle is meant to live
/// only as long as it takes the caller to copy the bytes out. One held
/// indefinitely is a leak of engine memory, not just of 32 bytes.
pub struct RegolithValue {
    /// Held, not read: the pointer handed to the caller addresses the
    /// engine's bytes directly, and this is what keeps them valid.
    #[allow(dead_code)]
    slice: DbSlice,
}

/// Lend `slice`'s bytes to the caller through `(out_ptr, out_len)`, with
/// `out_handle` taking the reference count that keeps them alive. The
/// caller releases it with [`regolith_release_value`].
///
/// A zero-length value pins nothing (regolith's empty slice has no
/// owner), so it yields `(null, 0)` and a **null** handle. Releasing a
/// null handle is a no-op, so the caller still needs only one
/// unconditional release.
///
/// # Safety
/// `out_ptr`, `out_len` and `out_handle` must be valid writable
/// pointers.
pub(crate) unsafe fn out_borrowed(
    slice: DbSlice,
    out_ptr: *mut *const u8,
    out_len: *mut usize,
    out_handle: *mut *mut RegolithValue,
) -> i32 {
    if out_ptr.is_null() || out_len.is_null() || out_handle.is_null() {
        set_error("null out-param");
        // `slice` drops here, so a rejected call releases the pin it was
        // handed rather than stranding it.
        return INVALID_ARG;
    }
    if slice.is_empty() {
        unsafe {
            *out_ptr = std::ptr::null();
            *out_len = 0;
            *out_handle = std::ptr::null_mut();
        }
        return OK;
    }
    // Read the view out before boxing: the bytes live in the engine, not
    // in the `DbSlice`, so moving the slice into the box does not move
    // them and the pointer stays valid for as long as the box does.
    let ptr = slice.as_slice().as_ptr();
    let len = slice.len();
    unsafe {
        *out_ptr = ptr;
        *out_len = len;
        *out_handle = Box::into_raw(Box::new(RegolithValue { slice }));
    }
    OK
}

/// Write a boolean out-param as 0/1.
///
/// # Safety
/// `out` must be null or a valid writable pointer.
pub(crate) unsafe fn out_bool(value: bool, out: *mut u8) -> i32 {
    if out.is_null() {
        set_error("null out-param");
        return INVALID_ARG;
    }
    unsafe { *out = u8::from(value) };
    OK
}

// ---------------------------------------------------------------------
// Exported utility functions.
// ---------------------------------------------------------------------

/// A do-nothing call, used by the benchmark to measure raw cgo call cost.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "C" fn regolith_noop() {
    // `black_box` stops the optimiser from deciding this call has no
    // observable effect and eliding it at the call site.
    std::hint::black_box(());
}

/// Return the calling thread's last error detail as a freshly allocated
/// C string, or null if there is none. Free it with
/// [`regolith_free_string`].
#[unsafe(no_mangle)]
pub extern "C" fn regolith_last_error_message() -> *mut std::ffi::c_char {
    // Deliberately not wrapped in `guard`: it returns a pointer, not a
    // status. Nothing here can panic other than allocation failure.
    LAST_ERROR.with(|slot| match slot.borrow().as_ref() {
        Some(message) => message.clone().into_raw(),
        None => std::ptr::null_mut(),
    })
}

/// Free a string returned by [`regolith_last_error_message`].
///
/// # Safety
/// `s` must be null or a pointer previously returned by
/// [`regolith_last_error_message`], and must not be freed twice.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_free_string(s: *mut std::ffi::c_char) {
    if s.is_null() {
        return;
    }
    drop(unsafe { CString::from_raw(s) });
}

/// Release a value handle from a `_get_borrowed` call, dropping the
/// reference count that kept the engine's bytes pinned. The borrowed
/// pointer that came with it is invalid afterwards. A null handle is a
/// no-op, which is what a zero-length or not-found read produces.
///
/// # Safety
/// `handle` must be null or a handle from a `_get_borrowed` call that
/// has not already been released.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_release_value(handle: *mut RegolithValue) {
    if handle.is_null() {
        return;
    }
    // Guarded like everything else: dropping the slice drops an arena
    // refcount, which can return a chunk to the engine's recycling pool.
    // That is engine code, so it gets the same no-unwind treatment as
    // the rest of the boundary. The status is discarded because the C
    // signature has nowhere to put it.
    let _ = guard(|| {
        drop(unsafe { Box::from_raw(handle) });
        OK
    });
}

/// Free a buffer handed out by any `_key`/`_value` call.
///
/// # Safety
/// `(ptr, len)` must be exactly a pair produced by this library and not
/// yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_free_buf(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
}

// ---------------------------------------------------------------------
// Test helpers.
// ---------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    /// Take the calling thread's last error detail, exactly as a C caller
    /// would: read it, then free it.
    pub(crate) fn last_error() -> Option<String> {
        let raw = super::regolith_last_error_message();
        if raw.is_null() {
            return None;
        }
        let message = unsafe { std::ffi::CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { super::regolith_free_string(raw) };
        Some(message)
    }
}

#[cfg(test)]
mod abi_tests;
