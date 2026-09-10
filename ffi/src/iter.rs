//! Iterator entry points.
//!
//! All of corekv's fussy iteration semantics live here rather than on the
//! Go side, because regolith's cursor already does the hard parts:
//!
//! * `End` is exclusive, `Start` inclusive.
//! * `Prefix` overrides `Start`/`End`; it is rewritten to the range
//!   `[prefix, prefix_end(prefix))`, so the key exactly equal to the
//!   prefix is yielded (which is what the corekv suite expects, despite
//!   the doc comment on `IterOptions.Prefix`).
//! * `Reset` returns the iterator to its construction position, so the
//!   data can be walked again.
//! * `Seek` is clamped to the configured range and, under `Reverse`,
//!   lands on the greatest key <= the target.
//! * `KeysOnly` suppresses the value copy entirely.
//!
//! Positioning model, matching `leveldb/iter.go` and `badger/iter.go`: an
//! iterator starts un-positioned with a pending reset, so the first
//! `next` call positions it on the first in-range entry rather than
//! advancing past it.

use std::sync::Arc;

use regolith::{OwnedSnapshotIter, ScanDirection, Snapshot, TxnScanStream};

use crate::txn::TxnInner;
use crate::{
    INVALID_ARG, NOT_FOUND, OK, guard, in_opt_bytes, out_bool, out_bytes, set_error, status_of,
};

/// Iteration options, laid out for C. A byte range is absent when its
/// pointer is null or its length is zero.
#[repr(C)]
pub struct RegolithIterOptions {
    /// Only yield keys beginning with this. Overrides `start`/`end`.
    pub prefix: *const u8,
    /// Length of `prefix`.
    pub prefix_len: usize,
    /// Inclusive lower bound.
    pub start: *const u8,
    /// Length of `start`.
    pub start_len: usize,
    /// Exclusive upper bound.
    pub end: *const u8,
    /// Length of `end`.
    pub end_len: usize,
    /// Non-zero to walk in descending key order.
    pub reverse: u8,
    /// Non-zero to skip value reads; `regolith_iter_value` then yields an
    /// empty buffer.
    pub keys_only: u8,
}

/// The resolved, owned form of [`RegolithIterOptions`].
pub(crate) struct Bounds {
    start: Option<Vec<u8>>,
    end: Option<Vec<u8>>,
    reverse: bool,
    keys_only: bool,
}

impl Bounds {
    fn direction(&self) -> ScanDirection {
        if self.reverse {
            ScanDirection::Reverse
        } else {
            ScanDirection::Forward
        }
    }

    /// Is `key` inside `[start, end)`?
    fn contains(&self, key: &[u8]) -> bool {
        if let Some(start) = &self.start
            && key < start.as_slice()
        {
            return false;
        }
        if let Some(end) = &self.end
            && key >= end.as_slice()
        {
            return false;
        }
        true
    }
}

/// The smallest key strictly greater than every key prefixed by `prefix`,
/// or `None` when `prefix` is all `0xff` and no such key exists.
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != 0xff {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

/// Read a caller-supplied options struct. A null `opts` means defaults.
///
/// # Safety
/// `opts` must be null or point at a valid struct whose byte pointers are
/// valid for their stated lengths.
pub(crate) unsafe fn bounds_from(opts: *const RegolithIterOptions) -> Result<Bounds, i32> {
    let Some(opts) = (unsafe { opts.as_ref() }) else {
        return Ok(Bounds {
            start: None,
            end: None,
            reverse: false,
            keys_only: false,
        });
    };

    let prefix = unsafe { in_opt_bytes(opts.prefix, opts.prefix_len) };
    let (start, end) = match prefix {
        // Prefix iteration is range iteration over [prefix, prefix_end).
        Some(prefix) => (Some(prefix.to_vec()), prefix_end(prefix)),
        None => (
            unsafe { in_opt_bytes(opts.start, opts.start_len) }.map(<[u8]>::to_vec),
            unsafe { in_opt_bytes(opts.end, opts.end_len) }.map(<[u8]>::to_vec),
        ),
    };

    Ok(Bounds {
        start,
        end,
        reverse: opts.reverse != 0,
        keys_only: opts.keys_only != 0,
    })
}

/// Where the entries come from.
enum Source {
    /// A snapshot of the committed store. A real cursor, so seeking is
    /// cheap and in-place.
    Snapshot(OwnedSnapshotIter),
    /// A transaction's buffered writes merged over its begin snapshot.
    ///
    /// `TxnScanStream` is a pull iterator with no cursor surface - no
    /// `seek`, no `prev`, no rewind - so `Reset`/`Seek` rebuild it with
    /// adjusted bounds instead. That is why the transaction is kept here.
    Txn {
        txn: Arc<TxnInner>,
        stream: Option<TxnScanStream<'static>>,
        /// The entry the iterator is currently positioned on. The stream
        /// hands out owned entries and keeps nothing, so the current one
        /// has to be held.
        current: Option<Entry>,
    },
}

struct Entry {
    key: Vec<u8>,
    /// `None` under `KeysOnly`.
    value: Option<Vec<u8>>,
}

/// Opaque iterator handle.
pub struct RegolithIter {
    bounds: Bounds,
    source: Source,
    /// Set at construction and by `regolith_iter_reset`: the next `next`
    /// call positions at the start of the range rather than advancing.
    reset: bool,
}

impl RegolithIter {
    pub(crate) fn over_snapshot(snapshot: Snapshot, bounds: Bounds) -> Self {
        Self {
            bounds,
            source: Source::Snapshot(snapshot.into_owned_iter()),
            reset: true,
        }
    }

    pub(crate) fn over_txn(txn: Arc<TxnInner>, bounds: Bounds) -> Result<Self, i32> {
        // Fail fast if the transaction is already resolved, rather than
        // handing back a handle that can never yield anything.
        txn.with(|_| ())?;
        Ok(Self {
            bounds,
            source: Source::Txn {
                txn,
                stream: None,
                current: None,
            },
            reset: true,
        })
    }

    /// Position on the first in-range entry.
    fn restart(&mut self) -> Result<bool, i32> {
        if matches!(self.source, Source::Txn { .. }) {
            let (start, end) = (self.bounds.start.clone(), self.bounds.end.clone());
            self.rebuild(start.as_deref(), end.as_deref())?;
            return self.pull();
        }
        let reverse = self.bounds.reverse;
        let start = self.bounds.start.clone();
        let end = self.bounds.end.clone();
        {
            let Source::Snapshot(cursor) = &mut self.source else {
                unreachable!();
            };
            if reverse {
                match &end {
                    // `end` is exclusive, so a reverse walk begins on the
                    // greatest key strictly below it.
                    Some(end) => {
                        cursor.seek_for_prev(end);
                        if cursor.valid() && cursor.key() == Some(end.as_slice()) {
                            cursor.prev();
                        }
                    }
                    None => cursor.seek_to_last(),
                }
            } else {
                match &start {
                    Some(start) => cursor.seek(start),
                    None => cursor.seek_to_first(),
                }
            }
        }
        self.cursor_valid()
    }

    /// Advance one entry in the iteration direction.
    fn step(&mut self) -> Result<bool, i32> {
        if matches!(self.source, Source::Txn { .. }) {
            return self.pull();
        }
        // Stepping an exhausted cursor is not defined, and a cursor that
        // has left the range is already at the end of the iteration.
        if !self.in_range() {
            return Ok(false);
        }
        let reverse = self.bounds.reverse;
        let Source::Snapshot(cursor) = &mut self.source else {
            unreachable!();
        };
        if reverse {
            cursor.prev();
        } else {
            cursor.next();
        }
        self.cursor_valid()
    }

    /// Position on the entry `Seek` should land on, clamped to the range.
    fn seek(&mut self, target: &[u8]) -> Result<bool, i32> {
        self.reset = false;
        if self.bounds.reverse {
            self.seek_reverse(target)
        } else {
            self.seek_forward(target)
        }
    }

    fn seek_forward(&mut self, target: &[u8]) -> Result<bool, i32> {
        // Never yield below `start`, so a target under it becomes `start`.
        let clamped: Vec<u8> = match &self.bounds.start {
            Some(start) if target < start.as_slice() => start.clone(),
            _ => target.to_vec(),
        };
        if matches!(self.source, Source::Txn { .. }) {
            let end = self.bounds.end.clone();
            self.rebuild(Some(&clamped), end.as_deref())?;
            return self.pull();
        }
        {
            let Source::Snapshot(cursor) = &mut self.source else {
                unreachable!();
            };
            cursor.seek(&clamped);
        }
        self.cursor_valid()
    }

    fn seek_reverse(&mut self, target: &[u8]) -> Result<bool, i32> {
        // `end` is exclusive, so a target at or above it is the same as
        // starting the reverse walk from the top of the range.
        let above_end = match &self.bounds.end {
            Some(end) => target >= end.as_slice(),
            None => false,
        };
        if matches!(self.source, Source::Txn { .. }) {
            // A reverse stream is bounded above by an exclusive `end`, so
            // to include `target` itself the bound is the smallest key
            // above it: `target` with a zero byte appended.
            let end = match &self.bounds.end {
                Some(end) if above_end => end.clone(),
                _ => {
                    let mut exclusive = target.to_vec();
                    exclusive.push(0);
                    exclusive
                }
            };
            let start = self.bounds.start.clone();
            self.rebuild(start.as_deref(), Some(&end))?;
            return self.pull();
        }
        if above_end {
            return self.restart();
        }
        {
            let Source::Snapshot(cursor) = &mut self.source else {
                unreachable!();
            };
            cursor.seek_for_prev(target);
        }
        self.cursor_valid()
    }

    /// Rebuild the transaction scan stream over a new range.
    fn rebuild(&mut self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<(), i32> {
        let direction = self.bounds.direction();
        let Source::Txn {
            txn,
            stream,
            current,
        } = &mut self.source
        else {
            unreachable!("rebuild is only for the transaction source");
        };
        // Drop the old stream before building the new one so there is
        // never more than one live at a time.
        *stream = None;
        *current = None;
        let fresh = txn.with(|t| {
            let borrowed = t.scan_stream_in(start, end, direction);
            // SAFETY (risk R2 in PLAN-regolith.md, option (a)): the
            // `'txn` lifetime on `TxnScanStream` is a claim, not a real
            // borrow - the stream holds an `Arc` on the engine and an
            // already-materialised `Vec` of the transaction's buffered
            // writes, exactly as `OwnedSnapshotIter` does for a
            // snapshot. Nothing inside it points at the `Transaction`.
            // Erasing the lifetime is therefore layout-identical and
            // dereferences nothing that can go away.
            //
            // What the transaction *does* still own is the snapshot pin
            // that keeps the sequence this stream reads from alive, so
            // the caller's contract is that every iterator is closed
            // before its transaction is committed, discarded or freed.
            // That is the same ordering badger requires, and the `Arc`
            // above keeps the handle itself alive regardless.
            let erased: TxnScanStream<'static> = unsafe { std::mem::transmute(borrowed) };
            erased
        })?;
        let Source::Txn { stream, .. } = &mut self.source else {
            unreachable!();
        };
        *stream = Some(fresh);
        Ok(())
    }

    /// Pull the next entry from the transaction stream into `current`.
    fn pull(&mut self) -> Result<bool, i32> {
        let keys_only = self.bounds.keys_only;
        let Source::Txn {
            stream, current, ..
        } = &mut self.source
        else {
            unreachable!("pull is only for the transaction source");
        };
        let Some(stream) = stream.as_mut() else {
            *current = None;
            return Ok(false);
        };
        match stream.next() {
            Some((key, value)) => {
                *current = Some(Entry {
                    key,
                    value: if keys_only {
                        None
                    } else {
                        Some(value.as_ref().to_vec())
                    },
                });
                Ok(true)
            }
            None => {
                // A merged stream cannot surface a mid-range read failure
                // through `Iterator`, so ask afterwards.
                let status = stream.status();
                *current = None;
                match status {
                    Ok(()) => Ok(false),
                    Err(e) => Err(status_of(&e)),
                }
            }
        }
    }

    /// Is the snapshot cursor on a key inside the range?
    fn in_range(&self) -> bool {
        match &self.source {
            Source::Snapshot(cursor) => match cursor.key() {
                Some(key) => self.bounds.contains(key),
                None => false,
            },
            Source::Txn { current, .. } => current.is_some(),
        }
    }

    /// Validity of the snapshot cursor after a move, surfacing read errors.
    fn cursor_valid(&self) -> Result<bool, i32> {
        let Source::Snapshot(cursor) = &self.source else {
            unreachable!("cursor_valid is only for the snapshot source");
        };
        if cursor.valid() {
            return Ok(self.in_range());
        }
        // An invalid cursor means either "range ended" or "read failed";
        // `status` is the only thing that tells them apart.
        match cursor.status() {
            Ok(()) => Ok(false),
            Err(e) => Err(status_of(&e)),
        }
    }

    fn current_key(&self) -> Option<&[u8]> {
        if !self.in_range() {
            return None;
        }
        match &self.source {
            Source::Snapshot(cursor) => cursor.key(),
            Source::Txn { current, .. } => current.as_ref().map(|e| e.key.as_slice()),
        }
    }

    fn current_value(&self) -> Option<&[u8]> {
        if self.bounds.keys_only || !self.in_range() {
            return None;
        }
        match &self.source {
            Source::Snapshot(cursor) => cursor.value(),
            Source::Txn { current, .. } => current.as_ref().and_then(|e| e.value.as_deref()),
        }
    }
}

macro_rules! iter_ref {
    ($ptr:expr) => {
        match unsafe { $ptr.as_mut() } {
            Some(it) => it,
            None => {
                set_error("null iterator handle");
                return INVALID_ARG;
            }
        }
    };
}

/// Advance the iterator, writing 0/1 to `valid`.
///
/// The first call after construction or after `regolith_iter_reset`
/// positions at the start of the range instead of advancing.
///
/// # Safety
/// `it` must be a live handle; `valid` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_next(it: *mut RegolithIter, valid: *mut u8) -> i32 {
    guard(|| {
        let iter = iter_ref!(it);
        let moved = if iter.reset {
            iter.reset = false;
            iter.restart()
        } else {
            iter.step()
        };
        match moved {
            Ok(is_valid) => unsafe { out_bool(is_valid, valid) },
            Err(status) => status,
        }
    })
}

/// Seek to `key`, clamped to the configured range, writing 0/1 to
/// `valid`. Under `Reverse` this lands on the greatest key <= `key`.
///
/// # Safety
/// `key` must be valid for `key_len` bytes; `valid` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_seek(
    it: *mut RegolithIter,
    key: *const u8,
    key_len: usize,
    valid: *mut u8,
) -> i32 {
    guard(|| {
        let iter = iter_ref!(it);
        let Some(key) = (unsafe { crate::in_bytes(key, key_len) }) else {
            set_error("null key");
            return INVALID_ARG;
        };
        match iter.seek(key) {
            Ok(is_valid) => unsafe { out_bool(is_valid, valid) },
            Err(status) => status,
        }
    })
}

/// Mark the iterator for re-iteration: the next `regolith_iter_next` call
/// returns to the start of the range.
///
/// # Safety
/// `it` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_reset(it: *mut RegolithIter) -> i32 {
    guard(|| {
        let iter = iter_ref!(it);
        iter.reset = true;
        OK
    })
}

/// Copy out the current key. Returns [`NOT_FOUND`] when the iterator is
/// not on a valid entry.
///
/// # Safety
/// `key`/`key_len` must be writable. On [`OK`] the caller owns the buffer
/// and releases it with `regolith_free_buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_key(
    it: *mut RegolithIter,
    key: *mut *mut u8,
    key_len: *mut usize,
) -> i32 {
    guard(|| {
        let iter = iter_ref!(it);
        match iter.current_key() {
            Some(current) => {
                let copied = current.to_vec();
                unsafe { out_bytes(copied, key, key_len) }
            }
            None => NOT_FOUND,
        }
    })
}

/// Copy out the current value, or an empty buffer under `KeysOnly`.
/// Returns [`NOT_FOUND`] when the iterator is not on a valid entry.
///
/// # Safety
/// As [`regolith_iter_key`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_value(
    it: *mut RegolithIter,
    val: *mut *mut u8,
    val_len: *mut usize,
) -> i32 {
    guard(|| {
        let iter = iter_ref!(it);
        if !iter.in_range() {
            return NOT_FOUND;
        }
        // `KeysOnly` yields an empty buffer rather than an error: the Go
        // side's contract is `nil, nil`.
        let value = iter.current_value().unwrap_or(&[]).to_vec();
        unsafe { out_bytes(value, val, val_len) }
    })
}

/// Close the iterator and free its handle.
///
/// # Safety
/// `it` must be a handle from `regolith_db_iter`/`regolith_txn_iter` that
/// has not already been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn regolith_iter_close(it: *mut RegolithIter) -> i32 {
    guard(|| {
        if it.is_null() {
            set_error("null iterator handle");
            return INVALID_ARG;
        }
        drop(unsafe { Box::from_raw(it) });
        OK
    })
}
