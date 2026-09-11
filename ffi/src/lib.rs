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
//! * A batch of writes crosses as one packed frame, decoded and validated
//!   in full before it reaches the engine; see `batch.rs` and
//!   `regolith_db_write`.
//! * Output bytes are allocated here; the caller copies them and then
//!   calls [`regolith_free_buf`].
//! * Every status-returning entry point takes a trailing `err` out-param.
//!   On failure it receives an owned [`RegolithError`] when the code alone
//!   does not say what went wrong ([`INVALID_ARG`], [`PANIC`], [`OTHER`]),
//!   null otherwise; read it with [`regolith_error_message`], free it with
//!   [`regolith_error_free`]. Nothing is keyed on the calling thread.
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

use std::any::Any;
use std::borrow::Cow;
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use regolith::DbSlice;

mod batch;
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
/// Anything else; the detail says what.
pub const OTHER: i32 = 8;

// ---------------------------------------------------------------------
// The error detail that travels with a non-OK status.
// ---------------------------------------------------------------------

/// A non-OK status on its way to the caller, with the detail the code
/// alone does not convey. Only [`INVALID_ARG`], [`PANIC`] and [`OTHER`]
/// carry one; every other code is its own message.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) status: i32,
    pub(crate) detail: Option<Cow<'static, str>>,
}

impl Failure {
    /// A status whose code is the whole message.
    pub(crate) const fn code(status: i32) -> Self {
        Self {
            status,
            detail: None,
        }
    }

    /// [`INVALID_ARG`] with the reason.
    pub(crate) fn invalid_arg(detail: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: INVALID_ARG,
            detail: Some(detail.into()),
        }
    }

    /// [`OTHER`] with the reason.
    pub(crate) fn other(detail: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status: OTHER,
            detail: Some(detail.into()),
        }
    }
}

/// Map a regolith engine error onto a status, keeping the text only
/// where the code does not already say what happened.
impl From<regolith::Error> for Failure {
    fn from(err: regolith::Error) -> Self {
        match err {
            regolith::Error::Closed => Self::code(DB_CLOSED),
            // The engine's `Display` already prefixes "invalid argument: ",
            // and so does the Go sentinel this becomes; keep the reason only.
            regolith::Error::InvalidArgument(reason) => Self::invalid_arg(reason),
            other => Self::other(other.to_string()),
        }
    }
}

/// Map a regolith transaction error onto a status.
impl From<regolith::TransactionError> for Failure {
    fn from(err: regolith::TransactionError) -> Self {
        match err {
            // Both are "someone else got there first, roll back and retry",
            // which is exactly `corekv.ErrTxnConflict`. The code is the whole
            // message: nothing reads a conflict's text, and formatting it
            // would Debug-print the key on every lost race.
            regolith::TransactionError::Conflict { .. } | regolith::TransactionError::Busy(_) => {
                Self::code(TXN_CONFLICT)
            }
            regolith::TransactionError::UnsupportedRangeDelete
            | regolith::TransactionError::NoSavepoint => Self::invalid_arg(err.to_string()),
            regolith::TransactionError::Io(_) => Self::other(err.to_string()),
        }
    }
}

/// The text of a caught panic payload, for the [`PANIC`] detail.
fn panic_detail(payload: Box<dyn Any + Send>) -> Cow<'static, str> {
    // `&'static str` from `panic!("literal")`, `String` from a formatted
    // one; anything else has no text to give.
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        Cow::Borrowed(*s)
    } else {
        match payload.downcast::<String>() {
            Ok(s) => Cow::Owned(*s),
            Err(_) => Cow::Borrowed("unknown panic"),
        }
    }
}

/// Opaque error detail, handed to the caller through an entry point's
/// `err` out-param. The caller owns it from that moment until
/// `regolith_error_free`; the message is immutable, so it may be read
/// from any thread, but it must be freed exactly once.
pub struct RegolithError {
    message: CString,
}

impl RegolithError {
    /// Box `detail` for the caller. Interior nul bytes would truncate the
    /// message, so they are stripped rather than losing it entirely.
    fn into_raw(detail: Cow<'static, str>) -> *mut RegolithError {
        let mut bytes = detail.into_owned().into_bytes();
        bytes.retain(|b| *b != 0);
        // Cannot fail: the nul bytes were just removed. `unwrap_or_default`
        // keeps the impossible case a silent empty message, not a panic.
        let message = CString::new(bytes).unwrap_or_default();
        Box::into_raw(Box::new(RegolithError { message }))
    }
}

/// The detail's message, borrowed from the object and valid until
/// [`regolith_error_free`]. Null for a null object.
///
/// # Safety
/// `err` must be null or an object from an entry point's `err` out-param
/// that has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_error_message(err: *const RegolithError) -> *const c_char {
    match unsafe { err.as_ref() } {
        Some(err) => err.message.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Release a detail object. Null is a no-op.
///
/// # Safety
/// `err` must be null or an object not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_error_free(err: *mut RegolithError) {
    if err.is_null() {
        return;
    }
    drop(unsafe { Box::from_raw(err) });
}

/// Run `body` with panics caught and hand its outcome to the caller: the
/// status is returned, and the detail, if there is one, is boxed into
/// `*err`. Every `extern "C"` entry point goes through this, so no unwind
/// can reach the caller's frame and there is exactly one place that turns
/// a [`Failure`] into what C sees.
///
/// `*err` is written exactly once when `err` is non-null, before the
/// return: null unless the failure carries detail, so the caller need
/// not initialise the slot. A null `err` discards the detail.
pub(crate) fn guard(
    err: *mut *mut RegolithError,
    body: impl FnOnce() -> Result<(), Failure>,
) -> i32 {
    // `AssertUnwindSafe`: a caught panic leaves the handles reachable but
    // possibly mid-update. We return a distinct status and the caller's
    // contract is to abandon the handle, so no torn state is observed.
    let (status, detail) = match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => (OK, None),
        Ok(Err(Failure { status, detail })) => (status, detail),
        Err(payload) => (PANIC, Some(panic_detail(payload))),
    };
    if !err.is_null() {
        // SAFETY: non-null was just checked; validity for a write is the
        // entry point's `# Safety` contract on its `err` argument.
        unsafe { *err = detail.map_or(std::ptr::null_mut(), RegolithError::into_raw) };
    }
    status
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
pub(crate) unsafe fn out_bytes(
    bytes: Vec<u8>,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> Result<(), Failure> {
    if out_ptr.is_null() || out_len.is_null() {
        return Err(Failure::invalid_arg("null out-param"));
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
    Ok(())
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
) -> Result<(), Failure> {
    if out_ptr.is_null() || out_len.is_null() || out_handle.is_null() {
        // `slice` drops here, so a rejected call releases the pin it was
        // handed rather than stranding it.
        return Err(Failure::invalid_arg("null out-param"));
    }
    if slice.is_empty() {
        unsafe {
            *out_ptr = std::ptr::null();
            *out_len = 0;
            *out_handle = std::ptr::null_mut();
        }
        return Ok(());
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
    Ok(())
}

/// Write a boolean out-param as 0/1.
///
/// # Safety
/// `out` must be null or a valid writable pointer.
pub(crate) unsafe fn out_bool(value: bool, out: *mut u8) -> Result<(), Failure> {
    if out.is_null() {
        return Err(Failure::invalid_arg("null out-param"));
    }
    unsafe { *out = u8::from(value) };
    Ok(())
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
    let _ = guard(std::ptr::null_mut(), || {
        drop(unsafe { Box::from_raw(handle) });
        Ok(())
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
    use std::ptr;

    use super::*;

    /// Take the detail an entry point handed back, exactly as a C caller
    /// would: read the message, then free the object. `None` when the
    /// call left the out-param null.
    pub(crate) fn take_error(err: *mut RegolithError) -> Option<String> {
        if err.is_null() {
            return None;
        }
        let message = unsafe { std::ffi::CStr::from_ptr(regolith_error_message(err)) }
            .to_string_lossy()
            .into_owned();
        unsafe { regolith_error_free(err) };
        Some(message)
    }

    #[test]
    fn guard_writes_null_on_success() {
        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = guard(&raw mut err, || Ok(()));
        assert_eq!(status, OK);
        assert!(err.is_null());
    }

    #[test]
    fn guard_writes_null_for_a_bare_code() {
        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = guard(&raw mut err, || Err(Failure::code(NOT_FOUND)));
        assert_eq!(status, NOT_FOUND);
        assert!(err.is_null());
    }

    #[test]
    fn guard_hands_back_invalid_argument_detail() {
        let mut err: *mut RegolithError = ptr::null_mut();
        let status = guard(&raw mut err, || Err(Failure::invalid_arg("null key")));
        assert_eq!(status, INVALID_ARG);
        assert_eq!(take_error(err), Some("null key".to_string()));
    }

    #[test]
    fn guard_hands_back_a_panic_with_its_message() {
        let mut err: *mut RegolithError = ptr::null_mut();
        let status = guard(&raw mut err, || panic!("boom"));
        assert_eq!(status, PANIC);
        assert_eq!(take_error(err), Some("boom".to_string()));

        let mut err: *mut RegolithError = ptr::null_mut();
        let status = guard(&raw mut err, || panic!("boom {}", 7));
        assert_eq!(status, PANIC);
        assert_eq!(take_error(err), Some("boom 7".to_string()));

        let mut err: *mut RegolithError = ptr::null_mut();
        let status = guard(&raw mut err, || std::panic::panic_any(7u8));
        assert_eq!(status, PANIC);
        assert_eq!(take_error(err), Some("unknown panic".to_string()));
    }

    #[test]
    fn guard_discards_detail_for_a_null_out_param() {
        let status = guard(ptr::null_mut(), || Err(Failure::invalid_arg("x")));
        assert_eq!(status, INVALID_ARG);
    }

    #[test]
    fn detail_message_round_trips_with_interior_nuls_stripped() {
        for (input, want) in [
            ("a\0b", "ab"),
            ("", ""),
            ("cl\u{e9} \u{1F600}", "cl\u{e9} \u{1F600}"),
            ("\0\0", ""),
        ] {
            let raw = RegolithError::into_raw(Cow::Owned(input.to_string()));
            let message = unsafe { std::ffi::CStr::from_ptr(regolith_error_message(raw)) }
                .to_str()
                .unwrap();
            assert_eq!(message, want, "input {input:?}");
            unsafe { regolith_error_free(raw) };
        }
    }

    #[test]
    fn error_message_and_free_accept_null() {
        assert!(unsafe { regolith_error_message(ptr::null()) }.is_null());
        unsafe { regolith_error_free(ptr::null_mut()) };
    }
}

#[cfg(test)]
mod abi_tests;
