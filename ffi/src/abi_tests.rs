//! Tests that drive the C ABI the way cgo does: raw pointers, out-params,
//! integer statuses, explicit frees.
//!
//! This is the gate on the FFI layer: anything these tests do not cover is
//! verified only through the Go tests one directory up. They live inside
//! the crate because it is built as a staticlib only, which no external
//! test target can link.

use std::ptr;

use crate::tests::take_error;
use crate::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------
// Thin Rust-side helpers over the C API. Deliberately not abstractions:
// each one is exactly one FFI call plus the ownership dance the Go side
// will have to do.
// ---------------------------------------------------------------------

fn open(dir: &TempDir) -> *mut RegolithDb {
    let path = dir.path().to_str().unwrap().as_bytes();
    let mut db: *mut RegolithDb = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_db_open(path.as_ptr(), path.len(), &raw mut db, &raw mut err) };
    assert_eq!(status, OK, "open failed: {:?}", take_error(err));
    assert!(!db.is_null());
    db
}

/// Copy an out-param buffer and release it, exactly as `C.GoBytes` plus
/// `regolith_free_buf` will.
fn take_buf(ptr: *mut u8, len: usize) -> Vec<u8> {
    let copied = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
    };
    unsafe { regolith_free_buf(ptr, len) };
    copied
}

fn set(db: *mut RegolithDb, key: &[u8], value: &[u8]) {
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_db_set(
            db,
            key.as_ptr(),
            key.len(),
            value.as_ptr(),
            value.len(),
            &raw mut err,
        )
    };
    assert_eq!(status, OK, "set failed: {:?}", take_error(err));
}

/// Copy a borrowed value and release the handle pinning it, exactly as
/// `C.GoBytes` plus `regolith_release_value` will. The release happens on
/// every path, including the failing ones, because the handle holds a
/// reference count on engine memory.
fn take_borrowed(ptr: *const u8, len: usize, handle: *mut RegolithValue) -> Vec<u8> {
    let copied = if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
    };
    unsafe { regolith_release_value(handle) };
    copied
}

fn get(db: *mut RegolithDb, key: &[u8]) -> Result<Vec<u8>, i32> {
    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_db_get_borrowed(
            db,
            key.as_ptr(),
            key.len(),
            &raw mut val,
            &raw mut len,
            &raw mut handle,
            &raw mut err,
        )
    };
    if status == OK {
        Ok(take_borrowed(val, len, handle))
    } else {
        assert!(
            handle.is_null(),
            "a failed get must produce no handle: {:?}",
            take_error(err)
        );
        Err(status)
    }
}

fn has(db: *mut RegolithDb, key: &[u8]) -> bool {
    let mut found: u8 = 2;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status =
        unsafe { regolith_db_has(db, key.as_ptr(), key.len(), &raw mut found, &raw mut err) };
    assert_eq!(status, OK, "has failed: {:?}", take_error(err));
    found == 1
}

fn txn_get(txn: *mut RegolithTxn, key: &[u8]) -> Result<Vec<u8>, i32> {
    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_txn_get_borrowed(
            txn,
            key.as_ptr(),
            key.len(),
            &raw mut val,
            &raw mut len,
            &raw mut handle,
            &raw mut err,
        )
    };
    if status == OK {
        Ok(take_borrowed(val, len, handle))
    } else {
        assert!(
            handle.is_null(),
            "a failed get must produce no handle: {:?}",
            take_error(err)
        );
        Err(status)
    }
}

fn txn_set(txn: *mut RegolithTxn, key: &[u8], value: &[u8]) -> i32 {
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_txn_set(
            txn,
            key.as_ptr(),
            key.len(),
            value.as_ptr(),
            value.len(),
            &raw mut err,
        )
    };
    // The status alone is what every caller checks; free whatever detail
    // arrived so a rare failure here does not leak it.
    take_error(err);
    status
}

/// Build an options struct. The byte slices must outlive the
/// `regolith_*_iter` call, which is why callers keep them in locals.
fn opts(
    prefix: Option<&[u8]>,
    start: Option<&[u8]>,
    end: Option<&[u8]>,
    reverse: bool,
    keys_only: bool,
) -> RegolithIterOptions {
    fn part(bytes: Option<&[u8]>) -> (*const u8, usize) {
        match bytes {
            Some(bytes) => (bytes.as_ptr(), bytes.len()),
            None => (ptr::null(), 0),
        }
    }
    let (prefix, prefix_len) = part(prefix);
    let (start, start_len) = part(start);
    let (end, end_len) = part(end);
    RegolithIterOptions {
        prefix,
        prefix_len,
        start,
        start_len,
        end,
        end_len,
        reverse: u8::from(reverse),
        keys_only: u8::from(keys_only),
    }
}

fn db_iter(db: *mut RegolithDb, opts: &RegolithIterOptions) -> *mut RegolithIter {
    let mut it: *mut RegolithIter = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_db_iter(db, opts, &raw mut it, &raw mut err) };
    assert_eq!(status, OK, "db_iter failed: {:?}", take_error(err));
    it
}

fn txn_iter(txn: *mut RegolithTxn, opts: &RegolithIterOptions) -> *mut RegolithIter {
    let mut it: *mut RegolithIter = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_txn_iter(txn, opts, &raw mut it, &raw mut err) };
    assert_eq!(status, OK, "txn_iter failed: {:?}", take_error(err));
    it
}

fn iter_next(it: *mut RegolithIter) -> bool {
    let mut valid: u8 = 2;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_iter_next(it, &raw mut valid, &raw mut err) };
    assert_eq!(status, OK, "iter_next failed: {:?}", take_error(err));
    valid == 1
}

fn iter_seek(it: *mut RegolithIter, key: &[u8]) -> bool {
    let mut valid: u8 = 2;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status =
        unsafe { regolith_iter_seek(it, key.as_ptr(), key.len(), &raw mut valid, &raw mut err) };
    assert_eq!(status, OK, "iter_seek failed: {:?}", take_error(err));
    valid == 1
}

fn iter_key(it: *mut RegolithIter) -> Vec<u8> {
    let mut key: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_iter_key(it, &raw mut key, &raw mut len, &raw mut err) };
    assert_eq!(status, OK, "iter_key failed: {:?}", take_error(err));
    take_buf(key, len)
}

fn iter_value(it: *mut RegolithIter) -> Vec<u8> {
    let mut val: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_iter_value(it, &raw mut val, &raw mut len, &raw mut err) };
    assert_eq!(status, OK, "iter_value failed: {:?}", take_error(err));
    take_buf(val, len)
}

/// Walk an iterator to exhaustion, collecting `(key, value)` as strings.
fn drain(it: *mut RegolithIter) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while iter_next(it) {
        let key = String::from_utf8(iter_key(it)).unwrap();
        let value = String::from_utf8(iter_value(it)).unwrap();
        out.push((key, value));
    }
    out
}

fn keys(entries: &[(String, String)]) -> Vec<&str> {
    entries.iter().map(|(k, _)| k.as_str()).collect()
}

/// Seed a store with `a..e` mapped to `va..ve`.
fn seed(db: *mut RegolithDb) {
    for key in ["a", "b", "c", "d", "e"] {
        set(db, key.as_bytes(), format!("v{key}").as_bytes());
    }
}

fn close(db: *mut RegolithDb) {
    let mut err: *mut RegolithError = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_db_close(db, &raw mut err) },
        OK,
        "{:?}",
        take_error(err)
    );
}

// ---------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------

#[test]
fn noop_is_callable() {
    regolith_noop();
}

#[test]
fn null_handles_are_errors_not_crashes() {
    let mut err: *mut RegolithError = ptr::null_mut();
    assert_eq!(
        unsafe {
            regolith_db_get_borrowed(
                ptr::null_mut(),
                b"k".as_ptr(),
                1,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut err,
            )
        },
        INVALID_ARG
    );
    assert_eq!(take_error(err), Some("null db handle".to_string()));

    // Releasing nothing is how a not-found or empty read is released.
    unsafe { regolith_release_value(ptr::null_mut()) };

    // Each null-handle call names the handle in its detail, whether or not
    // the caller asked for one.
    for (name, want) in [
        ("db handle", "null db handle"),
        ("txn handle", "null txn handle"),
        ("iterator handle", "null iterator handle"),
    ] {
        let mut err: *mut RegolithError = ptr::null_mut();
        let status = match name {
            "db handle" => unsafe { regolith_db_close(ptr::null_mut(), &raw mut err) },
            "txn handle" => unsafe { regolith_txn_commit(ptr::null_mut(), &raw mut err) },
            "iterator handle" => unsafe { regolith_iter_close(ptr::null_mut(), &raw mut err) },
            _ => unreachable!(),
        };
        assert_eq!(status, INVALID_ARG, "{name}");
        assert_eq!(take_error(err), Some(want.to_string()), "{name}");
    }

    // A null `err` is the caller declining the detail; the status is
    // unaffected.
    assert_eq!(
        unsafe { regolith_db_close(ptr::null_mut(), ptr::null_mut()) },
        INVALID_ARG
    );
    assert_eq!(
        unsafe { regolith_txn_commit(ptr::null_mut(), ptr::null_mut()) },
        INVALID_ARG
    );
    assert_eq!(
        unsafe { regolith_iter_close(ptr::null_mut(), ptr::null_mut()) },
        INVALID_ARG
    );
    assert_eq!(
        unsafe { regolith_iter_next(ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) },
        INVALID_ARG
    );
}

#[test]
fn error_out_param_is_null_on_success_and_bare_codes() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);

    // `err` starts pointing at garbage so a `guard` that skips the null
    // write on success, or that wrongly attaches detail to a bare code,
    // shows up as a non-null `err` instead of being masked by a
    // coincidentally-null starting value.
    let mut err: *mut RegolithError = ptr::dangling_mut();
    let status = unsafe { regolith_db_set(db, b"k".as_ptr(), 1, b"v".as_ptr(), 1, &raw mut err) };
    assert_eq!(status, OK);
    assert!(err.is_null());

    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::dangling_mut();
    let status = unsafe {
        regolith_db_get_borrowed(
            db,
            b"absent".as_ptr(),
            6,
            &raw mut val,
            &raw mut len,
            &raw mut handle,
            &raw mut err,
        )
    };
    assert_eq!(status, NOT_FOUND);
    assert!(err.is_null());

    let readonly = begin(db, true);
    let mut err: *mut RegolithError = ptr::dangling_mut();
    let status =
        unsafe { regolith_txn_set(readonly, b"k".as_ptr(), 1, b"v".as_ptr(), 1, &raw mut err) };
    assert_eq!(status, READ_ONLY_TXN);
    assert!(err.is_null());
    assert_eq!(unsafe { regolith_txn_free(readonly, ptr::null_mut()) }, OK);

    let discarded = begin(db, false);
    assert_eq!(
        unsafe { regolith_txn_discard(discarded, ptr::null_mut()) },
        OK
    );
    let mut err: *mut RegolithError = ptr::dangling_mut();
    let status =
        unsafe { regolith_txn_set(discarded, b"k".as_ptr(), 1, b"v".as_ptr(), 1, &raw mut err) };
    assert_eq!(status, DISCARDED);
    assert!(err.is_null());
    assert_eq!(unsafe { regolith_txn_free(discarded, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn an_engine_refusal_carries_its_detail() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);

    // One byte over regolith's default max_key_size (8 MiB).
    let key = vec![0u8; 8 * 1024 * 1024 + 1];
    let mut err: *mut RegolithError = ptr::null_mut();
    let status =
        unsafe { regolith_db_set(db, key.as_ptr(), key.len(), ptr::null(), 0, &raw mut err) };
    assert_eq!(status, INVALID_ARG);
    let message = take_error(err).expect("a detail message");
    assert!(
        message.contains("key length 8388609"),
        "message does not name the key length: {message}"
    );
    assert!(
        !message.starts_with("invalid argument: "),
        "the engine's own prefix leaked through: {message}"
    );

    close(db);
}

#[test]
fn open_and_close() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    close(db);

    // Re-opening the same directory works and sees nothing.
    let db = open(&dir);
    assert_eq!(get(db, b"missing"), Err(NOT_FOUND));
    close(db);
}

#[test]
fn set_get_has_delete() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);

    set(db, b"k", b"v");
    assert_eq!(get(db, b"k").unwrap(), b"v");
    assert!(has(db, b"k"));

    // Overwrite.
    set(db, b"k", b"v2");
    assert_eq!(get(db, b"k").unwrap(), b"v2");

    assert_eq!(
        unsafe { regolith_db_delete(db, b"k".as_ptr(), 1, ptr::null_mut()) },
        OK
    );
    assert_eq!(get(db, b"k"), Err(NOT_FOUND));
    assert!(!has(db, b"k"));

    // Deleting an absent key is not an error.
    assert_eq!(
        unsafe { regolith_db_delete(db, b"k".as_ptr(), 1, ptr::null_mut()) },
        OK
    );

    close(db);
}

#[test]
fn get_missing_returns_not_found() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);
    assert_eq!(get(db, b"zzz"), Err(NOT_FOUND));
    assert!(!has(db, b"zzz"));
    close(db);
}

#[test]
fn drop_all_empties_the_store() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);
    assert!(has(db, b"c"));

    assert_eq!(unsafe { regolith_db_drop_all(db, ptr::null_mut()) }, OK);

    for key in ["a", "b", "c", "d", "e"] {
        assert_eq!(get(db, key.as_bytes()), Err(NOT_FOUND), "{key} survived");
    }
    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);
    assert!(drain(it).is_empty());
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Still usable afterwards.
    set(db, b"fresh", b"v");
    assert_eq!(get(db, b"fresh").unwrap(), b"v");
    close(db);
}

// ---------------------------------------------------------------------
// Borrowed point reads
//
// `regolith_db_get_borrowed` and `regolith_txn_get_borrowed` hand out a
// pointer into memory the engine still owns, plus the handle holding the
// reference count that keeps it there. The helpers above already exercise
// the happy path everywhere; these tests pin down the edges of the
// ownership contract, which is what a leak or a use-after-free would be
// hiding in.
// ---------------------------------------------------------------------

/// One raw `regolith_db_get_borrowed` call, nothing released. Returns
/// everything the caller can observe so a test can assert on it.
fn get_raw(db: *mut RegolithDb, key: &[u8]) -> (i32, *const u8, usize, *mut RegolithValue) {
    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();
    let status = unsafe {
        regolith_db_get_borrowed(
            db,
            key.as_ptr(),
            key.len(),
            &raw mut val,
            &raw mut len,
            &raw mut handle,
            ptr::null_mut(),
        )
    };
    (status, val, len, handle)
}

#[test]
fn borrowed_get_lends_the_value_and_takes_it_back() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"k", b"hello");

    let (status, val, len, handle) = get_raw(db, b"k");
    assert_eq!(status, OK);
    assert!(
        !handle.is_null(),
        "a non-empty value must come with a handle"
    );
    assert_eq!(len, 5);
    // The bytes are readable while the handle is held. This is the whole
    // point: no copy was made on the way out.
    assert_eq!(unsafe { std::slice::from_raw_parts(val, len) }, b"hello");
    unsafe { regolith_release_value(handle) };

    close(db);
}

#[test]
fn borrowed_get_miss_produces_no_handle() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let (status, val, len, handle) = get_raw(db, b"absent");
    assert_eq!(status, NOT_FOUND);
    // Nothing was written, so the caller's zeroed locals are untouched
    // and its unconditional release is a no-op.
    assert!(handle.is_null());
    assert!(val.is_null());
    assert_eq!(len, 0);
    unsafe { regolith_release_value(handle) };

    close(db);
}

#[test]
fn borrowed_get_of_an_empty_value_pins_nothing() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"k", b"");

    // Present, but zero length: regolith's empty slice has no owner, so
    // there is nothing to pin and no handle to hand back. The Go binding
    // turns this into a nil slice.
    let (status, val, len, handle) = get_raw(db, b"k");
    assert_eq!(status, OK);
    assert_eq!(len, 0);
    assert!(val.is_null());
    assert!(handle.is_null());
    unsafe { regolith_release_value(handle) };

    assert!(has(db, b"k"), "an empty value is still an entry");
    assert!(get(db, b"k").unwrap().is_empty());

    close(db);
}

#[test]
fn borrowed_get_reads_bytes_owned_by_an_sstable_block() {
    let dir = TempDir::new().unwrap();

    // The FFI has no flush, and the default write buffer is 64 MiB, so
    // the store is built with the engine directly to get the values out
    // of the memtable arena and into an L0 file. Everything read back
    // below is therefore a slice over a decoded SSTable block rather
    // than over an arena chunk, which is the owner the production read
    // path hits most and the one `into_vec` used to copy.
    {
        let db = regolith::Db::open(dir.path(), regolith::Options::default()).unwrap();
        for i in 0..64u32 {
            db.put(
                format!("key{i:04}").as_bytes(),
                &vec![b'a' + (i % 26) as u8; 300],
            )
            .unwrap();
        }
        db.flush().unwrap();
        db.close().unwrap();
    }

    let db = open(&dir);
    for i in 0..64u32 {
        let expected = vec![b'a' + (i % 26) as u8; 300];
        assert_eq!(
            get(db, format!("key{i:04}").as_bytes()).unwrap(),
            expected,
            "key{i:04} came back wrong"
        );
    }

    // A value overwritten after the flush is served from the arena again,
    // so both owners are covered in one store.
    set(db, b"key0000", b"fresh");
    assert_eq!(get(db, b"key0000").unwrap(), b"fresh");

    close(db);
}

#[test]
fn db_is_usable_after_values_are_released() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"k", b"v");

    // Take and release the same value repeatedly: releasing a handle
    // must not disturb what it pointed at, and must leave the store and
    // its block cache fit to serve the next read.
    for _ in 0..1_000 {
        let (status, val, len, handle) = get_raw(db, b"k");
        assert_eq!(status, OK);
        assert_eq!(unsafe { std::slice::from_raw_parts(val, len) }, b"v");
        unsafe { regolith_release_value(handle) };
    }

    set(db, b"k", b"v2");
    assert_eq!(get(db, b"k").unwrap(), b"v2");
    assert_eq!(unsafe { regolith_db_drop_all(db, ptr::null_mut()) }, OK);
    assert_eq!(get(db, b"k"), Err(NOT_FOUND));

    close(db);
}

#[test]
fn borrowed_get_rejects_null_out_params() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"k", b"v");

    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();

    // Each out-param is rejected on its own, and the slice the engine
    // handed over is dropped rather than stranded.
    for (v, l, h) in [
        (ptr::null_mut(), &raw mut len, &raw mut handle),
        (&raw mut val, ptr::null_mut(), &raw mut handle),
        (&raw mut val, &raw mut len, ptr::null_mut()),
    ] {
        assert_eq!(
            unsafe { regolith_db_get_borrowed(db, b"k".as_ptr(), 1, v, l, h, ptr::null_mut()) },
            INVALID_ARG
        );
    }
    assert!(
        handle.is_null(),
        "a rejected call must not hand out a handle"
    );

    // A null key pointer with a non-zero length is still an argument
    // error, and the store is unharmed by any of it.
    assert_eq!(
        unsafe {
            regolith_db_get_borrowed(
                db,
                ptr::null(),
                1,
                &raw mut val,
                &raw mut len,
                &raw mut handle,
                ptr::null_mut(),
            )
        },
        INVALID_ARG
    );
    assert_eq!(get(db, b"k").unwrap(), b"v");

    close(db);
}

#[test]
fn txn_borrowed_get_covers_buffered_empty_and_absent() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"stored", b"v");
    set(db, b"blank", b"");

    let txn = begin(db, false);

    // Committed, buffered, empty and absent, all through the borrowed
    // path.
    assert_eq!(txn_get(txn, b"stored").unwrap(), b"v");
    assert_eq!(txn_set(txn, b"buffered", b"vb"), OK);
    assert_eq!(txn_get(txn, b"buffered").unwrap(), b"vb");
    assert!(txn_get(txn, b"blank").unwrap().is_empty());
    assert_eq!(txn_get(txn, b"absent"), Err(NOT_FOUND));

    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut handle: *mut RegolithValue = ptr::null_mut();
    assert_eq!(
        unsafe {
            regolith_txn_get_borrowed(
                txn,
                b"stored".as_ptr(),
                6,
                ptr::null_mut(),
                &raw mut len,
                &raw mut handle,
                ptr::null_mut(),
            )
        },
        INVALID_ARG
    );
    assert!(handle.is_null());

    // A value borrowed through a transaction is independent of it: the
    // copy was taken before the transaction resolved, and releasing the
    // handle afterwards is still correct.
    assert_eq!(
        unsafe {
            regolith_txn_get_borrowed(
                txn,
                b"stored".as_ptr(),
                6,
                &raw mut val,
                &raw mut len,
                &raw mut handle,
                ptr::null_mut(),
            )
        },
        OK
    );
    let copied = unsafe { std::slice::from_raw_parts(val, len) }.to_vec();
    assert_eq!(unsafe { regolith_txn_discard(txn, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);
    unsafe { regolith_release_value(handle) };
    assert_eq!(copied, b"v");

    close(db);
}

// ---------------------------------------------------------------------
// Iteration over the store
// ---------------------------------------------------------------------

#[test]
fn forward_scan() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);
    let entries = drain(it);
    assert_eq!(keys(&entries), ["a", "b", "c", "d", "e"]);
    assert_eq!(entries[2].1, "vc");
    // Exhausted iterators stay exhausted.
    assert!(!iter_next(it));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn reverse_scan() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, true, false);
    let it = db_iter(db, &options);
    let entries = drain(it);
    assert_eq!(keys(&entries), ["e", "d", "c", "b", "a"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn prefix_scan_includes_the_bare_prefix_and_nothing_outside_it() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    for key in ["j9", "k", "k1", "k2", "l0"] {
        set(db, key.as_bytes(), format!("v{key}").as_bytes());
    }

    let prefix = b"k";
    let options = opts(Some(prefix), None, None, false, false);
    let it = db_iter(db, &options);
    // The corekv suite (TestIteratorPrefix_DoesNotReturnSelf) expects the
    // key equal to the prefix to be yielded, despite what the doc comment
    // on IterOptions.Prefix says.
    assert_eq!(keys(&drain(it)), ["k", "k1", "k2"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(Some(prefix), None, None, true, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["k2", "k1", "k"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // A prefix overrides start/end rather than intersecting with them.
    let options = opts(Some(prefix), Some(b"a"), Some(b"zzz"), false, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["k", "k1", "k2"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn start_and_end_bounds_with_end_exclusive() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let (start, end) = (b"b", b"d");

    let options = opts(None, Some(start), Some(end), false, false);
    let it = db_iter(db, &options);
    // "d" is the exclusive end, so it must not appear.
    assert_eq!(keys(&drain(it)), ["b", "c"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(None, Some(start), Some(end), true, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["c", "b"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Start alone, and end alone.
    let options = opts(None, Some(b"c"), None, false, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["c", "d", "e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(None, None, Some(b"c"), false, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["a", "b"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn keys_only_suppresses_values() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, true);
    let it = db_iter(db, &options);
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"a");
    assert!(iter_value(it).is_empty());
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn seek_forward_and_reverse() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);
    // Exact hit.
    assert!(iter_seek(it, b"c"));
    assert_eq!(iter_key(it), b"c");
    // Miss lands on the next key forward.
    assert!(iter_seek(it, b"bb"));
    assert_eq!(iter_key(it), b"c");
    // Seek then continue.
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"d");
    // Past the end.
    assert!(!iter_seek(it, b"zzz"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(None, None, None, true, false);
    let it = db_iter(db, &options);
    // Reverse seek lands on the greatest key <= the target.
    assert!(iter_seek(it, b"cc"));
    assert_eq!(iter_key(it), b"c");
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"b");
    // Before the beginning.
    assert!(!iter_seek(it, b"0"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn seek_is_clamped_to_the_configured_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let (start, end) = (b"b", b"d");

    let options = opts(None, Some(start), Some(end), false, false);
    let it = db_iter(db, &options);
    // Below `start` clamps up to `start`, it does not yield "a".
    assert!(iter_seek(it, b"a"));
    assert_eq!(iter_key(it), b"b");
    // At or above the exclusive `end` is out of range.
    assert!(!iter_seek(it, b"d"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(None, Some(start), Some(end), true, false);
    let it = db_iter(db, &options);
    // At or above `end` clamps down to the top of the range, not to "d".
    assert!(iter_seek(it, b"zzz"));
    assert_eq!(iter_key(it), b"c");
    // Below `start` is out of range.
    assert!(!iter_seek(it, b"a"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn reset_allows_re_iteration() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, Some(b"b"), Some(b"e"), false, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["b", "c", "d"]);

    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(keys(&drain(it)), ["b", "c", "d"]);

    // Reset mid-walk, and after a seek.
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"b");
    assert!(iter_seek(it, b"d"));
    assert_eq!(iter_key(it), b"d");
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"b");
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // And in reverse.
    let options = opts(None, Some(b"b"), Some(b"e"), true, false);
    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["d", "c", "b"]);
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(keys(&drain(it)), ["d", "c", "b"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn store_iterator_sees_a_snapshot_from_its_creation() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);
    set(db, b"bb", b"vbb");
    assert_eq!(keys(&drain(it)), ["a", "b", "c", "d", "e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let it = db_iter(db, &options);
    assert_eq!(keys(&drain(it)), ["a", "b", "bb", "c", "d", "e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

// ---------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------

fn begin(db: *mut RegolithDb, readonly: bool) -> *mut RegolithTxn {
    let mut txn: *mut RegolithTxn = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_db_txn(db, u8::from(readonly), &raw mut txn, &raw mut err) };
    assert_eq!(status, OK, "begin failed: {:?}", take_error(err));
    assert!(!txn.is_null());
    txn
}

#[test]
fn txn_commit_is_visible() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, false);
    assert_eq!(txn_set(txn, b"f", b"vf"), OK);
    assert_eq!(
        unsafe { regolith_txn_delete(txn, b"a".as_ptr(), 1, ptr::null_mut()) },
        OK
    );

    // The transaction sees its own writes; the store does not, yet.
    assert_eq!(txn_get(txn, b"f").unwrap(), b"vf");
    assert_eq!(txn_get(txn, b"a"), Err(NOT_FOUND));
    assert_eq!(get(db, b"f"), Err(NOT_FOUND));
    assert_eq!(get(db, b"a").unwrap(), b"va");

    assert_eq!(unsafe { regolith_txn_commit(txn, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);

    assert_eq!(get(db, b"f").unwrap(), b"vf");
    assert_eq!(get(db, b"a"), Err(NOT_FOUND));

    close(db);
}

#[test]
fn txn_discard_is_invisible() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, false);
    assert_eq!(txn_set(txn, b"f", b"vf"), OK);
    assert_eq!(unsafe { regolith_txn_discard(txn, ptr::null_mut()) }, OK);
    // Discard is idempotent.
    assert_eq!(unsafe { regolith_txn_discard(txn, ptr::null_mut()) }, OK);
    // Further use reports the transaction as resolved.
    assert_eq!(txn_set(txn, b"g", b"vg"), DISCARDED);
    assert_eq!(txn_get(txn, b"a"), Err(DISCARDED));
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);

    assert_eq!(get(db, b"f"), Err(NOT_FOUND));

    // Freeing without resolving also discards.
    let txn = begin(db, false);
    assert_eq!(txn_set(txn, b"h", b"vh"), OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);
    assert_eq!(get(db, b"h"), Err(NOT_FOUND));

    close(db);
}

#[test]
fn txn_commit_after_resolution_reports_discarded() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);

    let txn = begin(db, false);
    assert_eq!(unsafe { regolith_txn_commit(txn, ptr::null_mut()) }, OK);
    assert_eq!(
        unsafe { regolith_txn_commit(txn, ptr::null_mut()) },
        DISCARDED
    );
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn txn_conflict_produces_the_conflict_code() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    set(db, b"k", b"v0");

    // Both begin before either commits, and both write the same key, so
    // snapshot-isolation validation must reject the second.
    let first = begin(db, false);
    let second = begin(db, false);
    assert_eq!(txn_set(first, b"k", b"v1"), OK);
    assert_eq!(txn_set(second, b"k", b"v2"), OK);

    assert_eq!(unsafe { regolith_txn_commit(first, ptr::null_mut()) }, OK);
    // `err` starts pointing at garbage so a `guard` that skips the null
    // write on a bare code would be caught rather than masked by a
    // coincidentally-null starting value.
    let mut err: *mut RegolithError = ptr::dangling_mut();
    let status = unsafe { regolith_txn_commit(second, &raw mut err) };
    assert_eq!(
        status,
        TXN_CONFLICT,
        "expected a conflict, got {status} ({:?})",
        take_error(err)
    );
    // C-6: nothing reads a conflict's text, so it carries no detail.
    assert!(err.is_null());

    assert_eq!(unsafe { regolith_txn_free(first, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(second, ptr::null_mut()) }, OK);
    assert_eq!(get(db, b"k").unwrap(), b"v1");

    close(db);
}

#[test]
fn readonly_txn_rejects_writes() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, true);
    assert_eq!(txn_set(txn, b"f", b"vf"), READ_ONLY_TXN);
    assert_eq!(
        unsafe { regolith_txn_delete(txn, b"a".as_ptr(), 1, ptr::null_mut()) },
        READ_ONLY_TXN
    );
    // Reads still work.
    assert_eq!(txn_get(txn, b"a").unwrap(), b"va");
    let mut found: u8 = 2;
    assert_eq!(
        unsafe { regolith_txn_has(txn, b"a".as_ptr(), 1, &raw mut found, ptr::null_mut()) },
        OK
    );
    assert_eq!(found, 1);

    assert_eq!(unsafe { regolith_txn_commit(txn, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);
    assert_eq!(get(db, b"f"), Err(NOT_FOUND));

    close(db);
}

// ---------------------------------------------------------------------
// Iteration over a transaction
// ---------------------------------------------------------------------

#[test]
fn txn_iterator_merges_buffered_writes() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, false);
    // An insert, an overwrite and a delete, all uncommitted.
    assert_eq!(txn_set(txn, b"bb", b"vbb"), OK);
    assert_eq!(txn_set(txn, b"c", b"vc2"), OK);
    assert_eq!(
        unsafe { regolith_txn_delete(txn, b"d".as_ptr(), 1, ptr::null_mut()) },
        OK
    );

    let options = opts(None, None, None, false, false);
    let it = txn_iter(txn, &options);
    let entries = drain(it);
    assert_eq!(keys(&entries), ["a", "b", "bb", "c", "e"]);
    assert_eq!(entries[3].1, "vc2");

    // Reset re-walks the same merged view.
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(keys(&drain(it)), ["a", "b", "bb", "c", "e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Reverse.
    let options = opts(None, None, None, true, false);
    let it = txn_iter(txn, &options);
    assert_eq!(keys(&drain(it)), ["e", "c", "bb", "b", "a"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Bounds, with `end` exclusive.
    let options = opts(None, Some(b"b"), Some(b"c"), false, false);
    let it = txn_iter(txn, &options);
    assert_eq!(keys(&drain(it)), ["b", "bb"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Prefix.
    let options = opts(Some(b"b"), None, None, false, false);
    let it = txn_iter(txn, &options);
    assert_eq!(keys(&drain(it)), ["b", "bb"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // Seek, forward and reverse, including the range clamp.
    let options = opts(None, Some(b"b"), Some(b"e"), false, false);
    let it = txn_iter(txn, &options);
    assert!(iter_seek(it, b"ba"));
    assert_eq!(iter_key(it), b"bb");
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"c");
    assert!(iter_seek(it, b"a"));
    assert_eq!(iter_key(it), b"b", "seek below start must clamp to start");
    assert!(!iter_seek(it, b"e"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    let options = opts(None, Some(b"b"), Some(b"e"), true, false);
    let it = txn_iter(txn, &options);
    assert!(iter_seek(it, b"bb"));
    assert_eq!(iter_key(it), b"bb", "reverse seek must include the target");
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"b");
    assert!(iter_seek(it, b"zzz"));
    assert_eq!(iter_key(it), b"c", "seek above end must clamp to the range");
    assert!(!iter_seek(it, b"a"));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    assert_eq!(unsafe { regolith_txn_discard(txn, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);
    close(db);
}

#[test]
fn txn_iterator_on_a_resolved_txn_is_rejected() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, false);
    assert_eq!(unsafe { regolith_txn_discard(txn, ptr::null_mut()) }, OK);

    let options = opts(None, None, None, false, false);
    let mut it: *mut RegolithIter = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_txn_iter(txn, &raw const options, &raw mut it, ptr::null_mut()) },
        DISCARDED
    );
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn null_iter_options_means_full_forward_iteration() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let mut it: *mut RegolithIter = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_db_iter(db, ptr::null(), &raw mut it, ptr::null_mut()) },
        OK
    );
    assert_eq!(keys(&drain(it)), ["a", "b", "c", "d", "e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

// ---------------------------------------------------------------------
// Batched iteration
//
// `regolith_iter_next_batch` frames keys only and retains one value
// reference per entry, so these tests check the framing, the positioning
// rules it shares with `regolith_iter_next`, and that values read back by
// index line up with the keys they were framed beside.
// ---------------------------------------------------------------------

/// Take one batch, decode the `[u32 key_len][key bytes]` frame and check
/// it against `*out_count`.
fn next_batch(it: *mut RegolithIter, max_entries: usize) -> Vec<String> {
    let mut out: *mut u8 = ptr::null_mut();
    let mut len: usize = 0;
    let mut count: usize = usize::MAX;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_iter_next_batch(
            it,
            max_entries,
            &raw mut out,
            &raw mut len,
            &raw mut count,
            &raw mut err,
        )
    };
    assert_eq!(status, OK, "next_batch failed: {:?}", take_error(err));
    assert!(count <= max_entries, "a batch must not exceed max_entries");
    let frame = take_buf(out, len);

    let mut keys = Vec::new();
    let mut at = 0usize;
    while at < frame.len() {
        let key_len = u32::from_le_bytes(frame[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        keys.push(String::from_utf8(frame[at..at + key_len].to_vec()).unwrap());
        at += key_len;
    }
    assert_eq!(keys.len(), count, "framed keys must match *out_count");
    keys
}

/// Borrow value `idx` of the current batch. Nothing is freed: the
/// pointer addresses bytes the iterator holds a reference on.
fn batch_value(it: *mut RegolithIter, idx: usize) -> Result<Vec<u8>, i32> {
    let mut val: *const u8 = ptr::null();
    let mut len: usize = 0;
    let mut err: *mut RegolithError = ptr::null_mut();
    let status =
        unsafe { regolith_iter_batch_value(it, idx, &raw mut val, &raw mut len, &raw mut err) };
    if status != OK {
        take_error(err);
        return Err(status);
    }
    Ok(if len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(val, len) }.to_vec()
    })
}

/// Walk to exhaustion in batches of `max_entries`, reading every value.
fn drain_batched(it: *mut RegolithIter, max_entries: usize) -> Vec<(String, String)> {
    let mut out = Vec::new();
    loop {
        let batch = next_batch(it, max_entries);
        // Exhaustion is `*out_count == 0`, never a short batch.
        if batch.is_empty() {
            return out;
        }
        for (idx, key) in batch.iter().enumerate() {
            let value = String::from_utf8(batch_value(it, idx).unwrap()).unwrap();
            out.push((key.clone(), value));
        }
    }
}

#[test]
fn batch_smaller_than_the_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);

    // First batch positions rather than advancing, so `a` is included.
    assert_eq!(next_batch(it, 2), ["a", "b"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"va");
    assert_eq!(batch_value(it, 1).unwrap(), b"vb");
    assert_eq!(next_batch(it, 2), ["c", "d"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"vc");
    assert_eq!(next_batch(it, 2), ["e"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"ve");
    assert!(next_batch(it, 2).is_empty());
    assert!(next_batch(it, 2).is_empty());

    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    close(db);
}

#[test]
fn batch_on_an_exact_multiple_of_the_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);

    // A batch exactly the size of the range fills, and the next one is
    // the empty batch that reports exhaustion.
    let it = db_iter(db, &options);
    assert_eq!(next_batch(it, 5), ["a", "b", "c", "d", "e"]);
    assert_eq!(batch_value(it, 4).unwrap(), b"ve");
    assert!(next_batch(it, 5).is_empty());
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // And so does a range that is an exact multiple of a smaller batch.
    let options = opts(None, Some(b"a"), Some(b"e"), false, false);
    let it = db_iter(db, &options);
    assert_eq!(next_batch(it, 2), ["a", "b"]);
    assert_eq!(next_batch(it, 2), ["c", "d"]);
    assert!(next_batch(it, 2).is_empty());
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn batch_on_an_empty_range_is_exhausted_immediately() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, Some(b"x"), Some(b"z"), false, false);
    let it = db_iter(db, &options);
    assert!(next_batch(it, 8).is_empty());
    // Nothing was retained, so every index is out of range.
    assert_eq!(batch_value(it, 0), Err(INVALID_ARG));
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn batch_matches_single_stepping_for_reverse_and_prefix() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);
    set(db, b"pre:1", b"v1");
    set(db, b"pre:2", b"v2");

    for max_entries in [1usize, 2, 256] {
        let options = opts(None, None, None, true, false);
        let it = db_iter(db, &options);
        assert_eq!(
            drain_batched(it, max_entries),
            vec![
                ("pre:2".to_string(), "v2".to_string()),
                ("pre:1".to_string(), "v1".to_string()),
                ("e".to_string(), "ve".to_string()),
                ("d".to_string(), "vd".to_string()),
                ("c".to_string(), "vc".to_string()),
                ("b".to_string(), "vb".to_string()),
                ("a".to_string(), "va".to_string()),
            ],
            "reverse batch of {max_entries}"
        );
        assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

        let options = opts(Some(b"pre:"), None, None, false, false);
        let it = db_iter(db, &options);
        let entries = drain_batched(it, max_entries);
        assert_eq!(
            keys(&entries),
            ["pre:1", "pre:2"],
            "prefix batch of {max_entries}"
        );
        assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

        // Reverse and prefix together, which is where the clamping and
        // the batch's first-iteration rule interact.
        let options = opts(Some(b"pre:"), None, None, true, false);
        let it = db_iter(db, &options);
        let entries = drain_batched(it, max_entries);
        assert_eq!(
            keys(&entries),
            ["pre:2", "pre:1"],
            "reverse prefix of {max_entries}"
        );
        assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    }

    close(db);
}

#[test]
fn batch_under_keys_only_retains_no_values() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, true);
    let it = db_iter(db, &options);

    assert_eq!(next_batch(it, 3), ["a", "b", "c"]);
    // In-range indices read back empty rather than failing, matching
    // `regolith_iter_value` under `KeysOnly`.
    for idx in 0..3 {
        assert_eq!(batch_value(it, idx), Ok(Vec::new()));
    }
    // Past the batch is still an error, so a caller bug is not hidden by
    // the empty-value rule.
    assert_eq!(batch_value(it, 3), Err(INVALID_ARG));

    assert_eq!(next_batch(it, 3), ["d", "e"]);
    assert_eq!(batch_value(it, 1), Ok(Vec::new()));
    assert_eq!(batch_value(it, 2), Err(INVALID_ARG));

    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    close(db);
}

#[test]
fn batch_after_a_seek_includes_the_sought_entry() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);

    // Exact match.
    assert!(iter_seek(it, b"c"));
    assert_eq!(next_batch(it, 2), ["c", "d"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"vc");
    // Inexact, landing on the next key up.
    assert!(iter_seek(it, b"bb"));
    assert_eq!(next_batch(it, 8), ["c", "d", "e"]);
    // A seek past the end yields an empty batch, not the range again.
    assert!(!iter_seek(it, b"z"));
    assert!(next_batch(it, 8).is_empty());
    // Single stepping keeps its own contract: it advances past the seek.
    assert!(iter_seek(it, b"c"));
    assert!(iter_next(it));
    assert_eq!(iter_key(it), b"d");
    // And a batch taken after that stepping continues from there.
    assert_eq!(next_batch(it, 8), ["e"]);
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    // In reverse a seek lands on the greatest key <= the target, and the
    // batch starts there and walks down.
    let options = opts(None, None, None, true, false);
    let it = db_iter(db, &options);
    assert!(iter_seek(it, b"bb"));
    assert_eq!(next_batch(it, 8), ["b", "a"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"vb");
    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);

    close(db);
}

#[test]
fn reset_re_batches_from_the_start_of_the_range() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let options = opts(None, Some(b"b"), Some(b"e"), false, false);
    let it = db_iter(db, &options);

    assert_eq!(keys(&drain_batched(it, 2)), ["b", "c", "d"]);
    assert!(next_batch(it, 2).is_empty());

    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(keys(&drain_batched(it, 2)), ["b", "c", "d"]);

    // A reset mid-batch, and a reset after a seek, both return to the
    // start of the range.
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(next_batch(it, 2), ["b", "c"]);
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(next_batch(it, 2), ["b", "c"]);
    assert!(iter_seek(it, b"d"));
    assert_eq!(unsafe { regolith_iter_reset(it, ptr::null_mut()) }, OK);
    assert_eq!(next_batch(it, 2), ["b", "c"]);
    // The reset dropped the previous batch's values with it.
    assert_eq!(batch_value(it, 2), Err(INVALID_ARG));

    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    close(db);
}

#[test]
fn batch_values_are_addressable_by_index_and_bounded() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    // Values distinct in length as well as content, so a wrong index or
    // a stale pointer cannot pass.
    for (i, key) in ["k1", "k2", "k3"].iter().enumerate() {
        set(db, key.as_bytes(), "x".repeat(i + 1).as_bytes());
    }
    set(db, b"k4", b"");

    let options = opts(None, None, None, false, false);
    let it = db_iter(db, &options);

    assert_eq!(next_batch(it, 4), ["k1", "k2", "k3", "k4"]);
    assert_eq!(batch_value(it, 0).unwrap(), b"x");
    assert_eq!(batch_value(it, 1).unwrap(), b"xx");
    assert_eq!(batch_value(it, 2).unwrap(), b"xxx");
    // An empty value is a zero-length read, not a failure.
    assert_eq!(batch_value(it, 3), Ok(Vec::new()));
    // Out of range, including absurdly so.
    assert_eq!(batch_value(it, 4), Err(INVALID_ARG));
    assert_eq!(batch_value(it, usize::MAX), Err(INVALID_ARG));

    // Reading out of order, and twice, is fine: the pointers are
    // borrowed from slices the iterator still holds.
    assert_eq!(batch_value(it, 2).unwrap(), b"xxx");
    assert_eq!(batch_value(it, 0).unwrap(), b"x");

    // Null out-params and a zero batch size are argument errors.
    let mut len: usize = 0;
    assert_eq!(
        unsafe { regolith_iter_batch_value(it, 0, ptr::null_mut(), &raw mut len, ptr::null_mut()) },
        INVALID_ARG
    );
    let mut out: *mut u8 = ptr::null_mut();
    let mut count: usize = 0;
    assert_eq!(
        unsafe {
            regolith_iter_next_batch(
                it,
                0,
                &raw mut out,
                &raw mut len,
                &raw mut count,
                ptr::null_mut(),
            )
        },
        INVALID_ARG
    );
    assert_eq!(
        unsafe {
            regolith_iter_next_batch(
                ptr::null_mut(),
                8,
                &raw mut out,
                &raw mut len,
                &raw mut count,
                ptr::null_mut(),
            )
        },
        INVALID_ARG
    );

    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    close(db);
}

#[test]
fn batch_over_a_txn_merges_buffered_writes() {
    let dir = TempDir::new().unwrap();
    let db = open(&dir);
    seed(db);

    let txn = begin(db, false);
    assert_eq!(txn_set(txn, b"bb", b"vbb"), OK);
    assert_eq!(txn_set(txn, b"c", b"updated"), OK);

    let options = opts(None, None, None, false, false);
    let it = txn_iter(txn, &options);
    assert_eq!(
        drain_batched(it, 2),
        vec![
            ("a".to_string(), "va".to_string()),
            ("b".to_string(), "vb".to_string()),
            ("bb".to_string(), "vbb".to_string()),
            ("c".to_string(), "updated".to_string()),
            ("d".to_string(), "vd".to_string()),
            ("e".to_string(), "ve".to_string()),
        ]
    );

    // A seek rebuilds the stream; the batch after it still starts on the
    // entry sought.
    assert!(iter_seek(it, b"bb"));
    assert_eq!(next_batch(it, 2), ["bb", "c"]);
    assert_eq!(batch_value(it, 1).unwrap(), b"updated");

    assert_eq!(unsafe { regolith_iter_close(it, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(txn, ptr::null_mut()) }, OK);
    close(db);
}

// ---------------------------------------------------------------------
// Engine options
//
// The resolution of `RegolithOptions` onto regolith's own `Options` is
// unit-tested in src/options.rs, where the resolved struct can be
// inspected field by field. What these tests add is the other half: that
// the ABI entry point accepts the struct, that a store opened through it
// actually works, and that a rejected value arrives as a status with the
// field named.
// ---------------------------------------------------------------------

/// A `RegolithOptions` with nothing set, as `calloc` or a Go zero value
/// gives it.
fn no_options() -> RegolithOptions {
    RegolithOptions {
        present: 0,
        write_buffer_size: 0,
        block_cache_size: 0,
        max_background_compactions: 0,
        transaction_keys_inline: 0,
        compression: 0,
        durability: 0,
        isolation: 0,
    }
}

/// `regolith_db_open_with_options`, returning either the handle or the
/// status that refused it.
fn open_with(
    dir: &TempDir,
    opts: *const RegolithOptions,
) -> Result<*mut RegolithDb, (i32, Option<String>)> {
    let path = dir.path().to_str().unwrap().as_bytes();
    let mut db: *mut RegolithDb = ptr::null_mut();
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe {
        regolith_db_open_with_options(path.as_ptr(), path.len(), opts, &raw mut db, &raw mut err)
    };
    if status == OK {
        assert!(!db.is_null());
        Ok(db)
    } else {
        assert!(db.is_null());
        Err((status, take_error(err)))
    }
}

/// Open with `opts`, exercise the store through a transaction, and close
/// it. Every option below has to survive this. `label` names the setting
/// under test so a failure says which one broke.
fn assert_store_works(dir: &TempDir, opts: &RegolithOptions, label: &str) {
    let db = open_with(dir, opts).unwrap_or_else(|(status, message)| {
        panic!("open with {label} failed: {status} {message:?}")
    });

    set(db, b"k", b"v");
    assert_eq!(get(db, b"k").unwrap(), b"v");

    let txn = begin(db, false);
    assert_eq!(txn_set(txn, b"t", b"tv"), OK);
    let mut err: *mut RegolithError = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_txn_commit(txn, &raw mut err) },
        OK,
        "{:?}",
        take_error(err)
    );
    let mut err: *mut RegolithError = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_txn_free(txn, &raw mut err) },
        OK,
        "{:?}",
        take_error(err)
    );
    assert_eq!(get(db, b"t").unwrap(), b"tv");

    close(db);
}

#[test]
fn open_with_null_options_is_the_defaults_path() {
    let dir = TempDir::new().unwrap();
    let db = open_with(&dir, ptr::null()).unwrap();
    set(db, b"k", b"v");
    close(db);

    // And the store it produced is the same one `regolith_db_open` would
    // have produced: re-opening it the old way reads the same bytes.
    let db = open(&dir);
    assert_eq!(get(db, b"k").unwrap(), b"v");
    close(db);
}

#[test]
fn open_with_a_zeroed_struct_is_the_defaults_path() {
    let dir = TempDir::new().unwrap();
    let opts = no_options();
    let db = open_with(&dir, &raw const opts).unwrap();
    set(db, b"k", b"v");
    close(db);

    let db = open(&dir);
    assert_eq!(get(db, b"k").unwrap(), b"v");
    close(db);
}

#[test]
fn every_exposed_field_round_trips_into_a_working_store() {
    for (bit, field, value) in [
        (OPT_WRITE_BUFFER_SIZE, "write_buffer_size", 1024 * 1024),
        (OPT_BLOCK_CACHE_SIZE, "block_cache_size", 2 * 1024 * 1024),
        (
            OPT_MAX_BACKGROUND_COMPACTIONS,
            "max_background_compactions",
            2,
        ),
        (OPT_TRANSACTION_KEYS_INLINE, "transaction_keys_inline", 128),
    ] {
        let dir = TempDir::new().unwrap();
        let mut opts = no_options();
        opts.present = bit;
        opts.write_buffer_size = value;
        opts.block_cache_size = value;
        opts.max_background_compactions = value;
        opts.transaction_keys_inline = value;
        assert_store_works(&dir, &opts, field);
    }

    for codec in [COMPRESSION_NONE, COMPRESSION_SNAPPY, COMPRESSION_LZ4] {
        let dir = TempDir::new().unwrap();
        let mut opts = no_options();
        opts.present = OPT_COMPRESSION;
        opts.compression = codec;
        assert_store_works(&dir, &opts, &format!("compression {codec}"));
    }

    for mode in [DURABILITY_IMMEDIATE, DURABILITY_EVENTUAL] {
        let dir = TempDir::new().unwrap();
        let mut opts = no_options();
        opts.present = OPT_DURABILITY;
        opts.durability = mode;
        assert_store_works(&dir, &opts, &format!("durability {mode}"));
    }
}

#[test]
fn zero_background_compactions_is_accepted_and_is_not_unset() {
    // The case a zero-means-default scheme would have made unreachable:
    // no background worker, compaction on the calling thread. It has to
    // open, and it has to survive enough writes to flush.
    let dir = TempDir::new().unwrap();
    let mut opts = no_options();
    opts.present = OPT_MAX_BACKGROUND_COMPACTIONS | OPT_WRITE_BUFFER_SIZE;
    opts.max_background_compactions = 0;
    // Small enough that the writes below rotate the memtable, so the
    // calling-thread compaction path is actually entered.
    opts.write_buffer_size = 64 * 1024;

    let db = open_with(&dir, &raw const opts).expect("open failed");
    let value = vec![b'x'; 4096];
    for i in 0..64u32 {
        set(db, format!("key-{i:04}").as_bytes(), &value);
    }
    assert_eq!(get(db, b"key-0000").unwrap(), value);
    close(db);
}

#[test]
fn a_value_regolith_refuses_is_rejected_with_the_field_named() {
    // `write_buffer_size` must be greater than zero. Setting the bit with
    // a zero value reaches `Options::validate`, which refuses it before
    // any filesystem work - so nothing is created and nothing is clamped.
    let dir = TempDir::new().unwrap();
    let mut opts = no_options();
    opts.present = OPT_WRITE_BUFFER_SIZE;
    opts.write_buffer_size = 0;

    let Err((status, message)) = open_with(&dir, &raw const opts) else {
        panic!("expected the open to fail");
    };
    assert_eq!(status, INVALID_ARG);
    let message = message.expect("a detail message");
    assert!(
        message.contains("write_buffer_size"),
        "message does not name the field: {message}"
    );
}

#[test]
fn an_unknown_enum_discriminant_is_rejected_with_the_field_named() {
    let dir = TempDir::new().unwrap();
    let mut opts = no_options();
    opts.present = OPT_COMPRESSION;
    opts.compression = 99;

    let Err((status, message)) = open_with(&dir, &raw const opts) else {
        panic!("expected the open to fail");
    };
    assert_eq!(status, INVALID_ARG);
    let message = message.expect("a detail message");
    assert!(
        message.contains("compression"),
        "message does not name the field: {message}"
    );
}

/// Two transactions that each read what the other is about to write, with
/// disjoint write sets: the write-skew schedule. Returns the status of
/// the second commit, which is the whole question - the first always
/// commits, so whether the second one does is exactly what the isolation
/// level decides.
fn write_skew(dir: &TempDir, opts: *const RegolithOptions) -> i32 {
    let db = open_with(dir, opts).expect("open failed");
    set(db, b"x", b"0");
    set(db, b"y", b"0");

    let first = begin(db, false);
    let second = begin(db, false);

    // Each reads the key the other writes. Plain reads: nothing here asks
    // for a key "for update", so snapshot isolation validates neither.
    assert_eq!(txn_get(first, b"y").unwrap(), b"0");
    assert_eq!(txn_get(second, b"x").unwrap(), b"0");
    assert_eq!(txn_set(first, b"x", b"1"), OK);
    assert_eq!(txn_set(second, b"y", b"1"), OK);

    let mut err: *mut RegolithError = ptr::null_mut();
    assert_eq!(
        unsafe { regolith_txn_commit(first, &raw mut err) },
        OK,
        "first commit: {:?}",
        take_error(err)
    );
    let mut err: *mut RegolithError = ptr::null_mut();
    let status = unsafe { regolith_txn_commit(second, &raw mut err) };
    // A conflict carries no detail (C-6); free whatever is there either way.
    take_error(err);

    assert_eq!(unsafe { regolith_txn_free(first, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(second, ptr::null_mut()) }, OK);
    close(db);
    status
}

#[test]
fn isolation_unset_is_snapshot_isolation_and_admits_write_skew() {
    // The guarantee that keeps every previously recorded number valid: a
    // store opened without the bit behaves exactly as it did before the
    // field existed.
    let dir = TempDir::new().unwrap();
    assert_eq!(write_skew(&dir, ptr::null()), OK);

    let dir = TempDir::new().unwrap();
    let opts = no_options();
    assert_eq!(write_skew(&dir, &raw const opts), OK);
}

#[test]
fn each_isolation_level_changes_what_write_skew_does() {
    for (level, label, want) in [
        (ISOLATION_READ_COMMITTED, "read committed", OK),
        (ISOLATION_SNAPSHOT, "snapshot", OK),
        (ISOLATION_SERIALIZABLE, "serializable", TXN_CONFLICT),
    ] {
        let dir = TempDir::new().unwrap();
        let mut opts = no_options();
        opts.present = OPT_ISOLATION;
        opts.isolation = level;

        let status = write_skew(&dir, &raw const opts);
        assert_eq!(
            status, want,
            "{label}: second commit was {status}, want {want}"
        );
    }
}

#[test]
fn serializable_still_conflicts_on_a_write_write_overlap() {
    // Serializable only ever adds to the validation set, so everything
    // snapshot isolation rejected it must reject too.
    let dir = TempDir::new().unwrap();
    let mut opts = no_options();
    opts.present = OPT_ISOLATION;
    opts.isolation = ISOLATION_SERIALIZABLE;
    let db = open_with(&dir, &raw const opts).expect("open failed");
    set(db, b"k", b"v0");

    let first = begin(db, false);
    let second = begin(db, false);
    assert_eq!(txn_set(first, b"k", b"v1"), OK);
    assert_eq!(txn_set(second, b"k", b"v2"), OK);
    assert_eq!(unsafe { regolith_txn_commit(first, ptr::null_mut()) }, OK);
    assert_eq!(
        unsafe { regolith_txn_commit(second, ptr::null_mut()) },
        TXN_CONFLICT
    );
    assert_eq!(unsafe { regolith_txn_free(first, ptr::null_mut()) }, OK);
    assert_eq!(unsafe { regolith_txn_free(second, ptr::null_mut()) }, OK);
    assert_eq!(get(db, b"k").unwrap(), b"v1");

    close(db);
}

#[test]
fn every_isolation_level_round_trips_into_a_working_store() {
    for level in [
        ISOLATION_READ_COMMITTED,
        ISOLATION_SNAPSHOT,
        ISOLATION_SERIALIZABLE,
    ] {
        let dir = TempDir::new().unwrap();
        let mut opts = no_options();
        opts.present = OPT_ISOLATION;
        opts.isolation = level;
        assert_store_works(&dir, &opts, &format!("isolation {level}"));
    }
}

#[test]
fn an_unknown_isolation_level_is_rejected_with_the_field_named() {
    let dir = TempDir::new().unwrap();
    let mut opts = no_options();
    opts.present = OPT_ISOLATION;
    opts.isolation = 3;

    let Err((status, message)) = open_with(&dir, &raw const opts) else {
        panic!("expected the open to fail");
    };
    assert_eq!(status, INVALID_ARG);
    let message = message.expect("a detail message");
    assert!(
        message.contains("isolation"),
        "message does not name the field: {message}"
    );
}
