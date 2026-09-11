//! Decodes the write-batch frame consumed by [`crate::regolith_db_write`].
//!
//! A frame is a sequence of ops, each one of
//!
//! ```text
//! set:    [u8 tag = 1][u64 key_len][key bytes][u64 value_len][value bytes]
//! delete: [u8 tag = 2][u64 key_len][key bytes]
//! ```
//!
//! with little-endian, unsigned 8-byte lengths, repeated until the buffer
//! ends. There is no op count and no header: the decoder walks to the end
//! and rejects anything that does not fit exactly.
//!
//! This module owns decoding that frame and nothing else. The invariant: a
//! frame either decodes completely into a [`WriteBatch`] or produces one
//! [`INVALID_ARG`](crate::INVALID_ARG) naming the first bad op, and in the
//! second case nothing is written, because the caller only ever hands the
//! engine a fully decoded batch.

use regolith::WriteBatch;

use crate::Failure;

/// Frame tag of a set. Must match `opSet` in the Go binding.
pub(crate) const OP_SET: u8 = 1;
/// Frame tag of a delete. Must match `opDelete` in the Go binding.
pub(crate) const OP_DELETE: u8 = 2;

/// Decode a batch frame into a [`WriteBatch`], rejecting a malformed frame
/// before any op reaches the engine.
///
/// The batch holds one copy of every key and value (regolith's own
/// `WriteBatch` copies on `put`/`delete`), so its memory is bounded by
/// the frame the caller built.
pub(crate) fn decode(frame: &[u8]) -> Result<WriteBatch, Failure> {
    let mut batch = WriteBatch::new();
    let mut rest = frame;
    let mut index = 0usize;
    while let Some((&tag, tail)) = rest.split_first() {
        rest = tail;
        match tag {
            OP_SET => {
                let key = field(&mut rest, index, "key")?;
                let value = field(&mut rest, index, "value")?;
                batch.put(key, value);
            }
            OP_DELETE => batch.delete(field(&mut rest, index, "key")?),
            other => {
                return Err(Failure::invalid_arg(format!(
                    "write batch op {index} has unknown tag {other}"
                )));
            }
        }
        index += 1;
    }
    Ok(batch)
}

/// Take one `[u64 len][bytes]` field off the front of `rest`. `what` names
/// the field in the detail of a frame that ends inside it.
fn field<'a>(rest: &mut &'a [u8], index: usize, what: &str) -> Result<&'a [u8], Failure> {
    let Some((len, tail)) = rest.split_first_chunk::<8>() else {
        return Err(Failure::invalid_arg(format!(
            "write batch op {index} is cut off in its {what} length"
        )));
    };
    // Only fallible on a target narrower than 64 bits; there a length this
    // large cannot be in the frame either way.
    let Ok(len) = usize::try_from(u64::from_le_bytes(*len)) else {
        return Err(Failure::invalid_arg(format!(
            "write batch op {index} has a {what} length that does not fit in memory"
        )));
    };
    let Some((bytes, tail)) = tail.split_at_checked(len) else {
        return Err(Failure::invalid_arg(format!(
            "write batch op {index} is cut off inside its {what}"
        )));
    };
    *rest = tail;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use crate::abi_tests::{close, get, has, open, seed};
    use crate::tests::take_error;
    use crate::*;
    use tempfile::TempDir;

    use super::*;

    // ---------------------------------------------------------------------
    // Test frame builder, mirroring the Go packer.
    // ---------------------------------------------------------------------

    enum Op<'a> {
        Set(&'a [u8], &'a [u8]),
        Delete(&'a [u8]),
    }

    fn frame(ops: &[Op]) -> Vec<u8> {
        let mut buf = Vec::new();
        for op in ops {
            match op {
                Op::Set(key, value) => {
                    buf.push(OP_SET);
                    buf.extend_from_slice(&(key.len() as u64).to_le_bytes());
                    buf.extend_from_slice(key);
                    buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
                    buf.extend_from_slice(value);
                }
                Op::Delete(key) => {
                    buf.push(OP_DELETE);
                    buf.extend_from_slice(&(key.len() as u64).to_le_bytes());
                    buf.extend_from_slice(key);
                }
            }
        }
        buf
    }

    // ---------------------------------------------------------------------
    // decode() alone, no engine involved.
    // ---------------------------------------------------------------------

    #[test]
    fn decode_empty_frame_is_an_empty_batch() {
        let batch = decode(&[]).expect("an empty frame decodes");
        assert!(batch.is_empty());
        assert_eq!(batch.len(), 0);
    }

    #[test]
    fn decode_counts_sets_and_deletes() {
        let f = frame(&[Op::Set(b"a", b"1"), Op::Delete(b"b"), Op::Set(b"c", b"")]);
        let batch = decode(&f).expect("a well-formed frame decodes");
        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 3);

        let f = frame(&[Op::Set(b"", b"v")]);
        let batch = decode(&f).expect("an empty key is accepted");
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        for (tag, want) in [
            (0u8, "unknown tag 0"),
            (3, "unknown tag 3"),
            (255, "unknown tag 255"),
        ] {
            let err = decode(&[tag]).expect_err("an unknown tag is rejected");
            assert_eq!(err.status, INVALID_ARG);
            let detail = err.detail.expect("invalid_arg carries a detail");
            assert!(detail.contains(want), "{detail:?} missing {want:?}");
            assert!(detail.contains("op 0"), "{detail:?} missing op 0");
        }

        let mut f = frame(&[Op::Set(b"a", b"1")]);
        f.push(9);
        let err = decode(&f).expect_err("the second op's bad tag is rejected");
        let detail = err.detail.expect("invalid_arg carries a detail");
        assert!(detail.contains("op 1"), "{detail:?} missing op 1");
    }

    #[test]
    fn decode_rejects_truncated_key() {
        let mut f = vec![OP_SET];
        f.extend_from_slice(&5u64.to_le_bytes());
        f.extend_from_slice(b"ab");
        let err = decode(&f).expect_err("a key cut short of its claimed length is rejected");
        assert!(err.detail.unwrap().contains("cut off inside its key"));

        let f = vec![OP_SET, 0, 0, 0];
        let err = decode(&f).expect_err("a key length cut short is rejected");
        assert!(err.detail.unwrap().contains("cut off in its key length"));

        let f = vec![OP_DELETE];
        let err = decode(&f).expect_err("a delete with no key length is rejected");
        assert!(err.detail.unwrap().contains("cut off in its key length"));
    }

    #[test]
    fn decode_rejects_truncated_value() {
        let mut f = vec![OP_SET];
        f.extend_from_slice(&1u64.to_le_bytes());
        f.push(b'a');
        f.extend_from_slice(&4u64.to_le_bytes());
        f.extend_from_slice(b"xy");
        let err = decode(&f).expect_err("a value cut short of its claimed length is rejected");
        assert!(err.detail.unwrap().contains("cut off inside its value"));

        let mut f = vec![OP_SET];
        f.extend_from_slice(&1u64.to_le_bytes());
        f.push(b'a');
        let err = decode(&f).expect_err("a set with no value length is rejected");
        assert!(err.detail.unwrap().contains("cut off in its value length"));
    }

    #[test]
    fn decode_rejects_oversized_lengths() {
        // A frame claiming an enormous length fails at the presence check,
        // proving no allocation was ever attempted from it: this test
        // completing at all is the assertion for that case.
        let mut f = vec![OP_SET];
        f.extend_from_slice(&u64::MAX.to_le_bytes());
        let err = decode(&f).expect_err("a u64::MAX key length is rejected");
        assert!(err.detail.unwrap().contains("cut off inside its key"));

        let mut f = vec![OP_SET];
        f.extend_from_slice(&(1u64 << 40).to_le_bytes());
        let err = decode(&f).expect_err("a huge key length is rejected");
        assert!(err.detail.unwrap().contains("cut off inside its key"));

        let mut f = vec![OP_SET];
        f.extend_from_slice(&1u64.to_le_bytes());
        f.push(b'a');
        f.extend_from_slice(&(1u64 << 40).to_le_bytes());
        let err = decode(&f).expect_err("a huge value length is rejected");
        assert!(err.detail.unwrap().contains("cut off inside its value"));
    }

    /// A small xorshift64* PRNG, deterministic across runs: the same seed
    /// produces the same sequence of ops every time this test runs.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn decode_round_trips_a_generated_frame() {
        let mut rng = Rng(42);
        for round in 0..200 {
            let op_count = 1 + (rng.next() % 64) as usize;
            let ops: Vec<(bool, Vec<u8>, Vec<u8>)> = (0..op_count)
                .map(|_| {
                    let is_set = rng.next().is_multiple_of(2);
                    let key_len = (rng.next() % 33) as usize;
                    let key: Vec<u8> = (0..key_len).map(|_| rng.next() as u8).collect();
                    let value = if is_set {
                        let value_len = (rng.next() % 301) as usize;
                        (0..value_len).map(|_| rng.next() as u8).collect()
                    } else {
                        Vec::new()
                    };
                    (is_set, key, value)
                })
                .collect();
            let framed: Vec<Op> = ops
                .iter()
                .map(|(is_set, key, value)| {
                    if *is_set {
                        Op::Set(key, value)
                    } else {
                        Op::Delete(key)
                    }
                })
                .collect();

            let f = frame(&framed);
            let batch = decode(&f)
                .unwrap_or_else(|e| panic!("round {round} failed to decode: {:?}", e.detail));
            assert_eq!(batch.len(), op_count, "round {round}");

            let mut extra = f.clone();
            extra.push(0);
            assert!(
                decode(&extra).is_err(),
                "round {round}: a trailing byte must be rejected"
            );

            let mut short = f;
            short.pop();
            assert!(
                decode(&short).is_err(),
                "round {round}: a missing final byte must be rejected"
            );
        }
    }

    // ---------------------------------------------------------------------
    // regolith_db_write, driven as cgo does. A closed handle is not
    // exercised here: regolith_db_close frees it, so that check belongs to
    // the Go side (batch_test.go), which owns the closed-handle guard.
    // ---------------------------------------------------------------------

    fn write(db: *mut RegolithDb, frame: &[u8]) -> (i32, Option<String>) {
        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = unsafe { regolith_db_write(db, frame.as_ptr(), frame.len(), &raw mut err) };
        (status, take_error(err))
    }

    #[test]
    fn db_write_round_trips_sets_and_deletes() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);
        seed(db);

        let f = frame(&[
            Op::Set(b"a", b"va2"),
            Op::Delete(b"b"),
            Op::Set(b"f", b"vf"),
            Op::Delete(b"zz"),
        ]);
        let (status, detail) = write(db, &f);
        assert_eq!(status, OK, "write failed: {detail:?}");
        assert!(detail.is_none());

        assert_eq!(get(db, b"a"), Ok(b"va2".to_vec()));
        assert_eq!(get(db, b"b"), Err(NOT_FOUND));
        assert_eq!(get(db, b"f"), Ok(b"vf".to_vec()));
        assert_eq!(get(db, b"c"), Ok(b"vc".to_vec()));
        assert!(!has(db, b"zz"));

        close(db);
    }

    #[test]
    fn db_write_empty_frame_is_ok() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);
        seed(db);

        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = unsafe { regolith_db_write(db, ptr::null(), 0, &raw mut err) };
        assert_eq!(status, OK);
        assert!(err.is_null());

        let empty: &[u8] = b"";
        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = unsafe { regolith_db_write(db, empty.as_ptr(), 0, &raw mut err) };
        assert_eq!(status, OK);
        assert!(err.is_null());

        assert_eq!(get(db, b"a"), Ok(b"va".to_vec()));

        close(db);
    }

    #[test]
    fn db_write_null_arguments_are_invalid_arg() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);
        seed(db);

        let f = frame(&[Op::Set(b"x", b"1")]);
        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status =
            unsafe { regolith_db_write(ptr::null_mut(), f.as_ptr(), f.len(), &raw mut err) };
        assert_eq!(status, INVALID_ARG);
        assert_eq!(take_error(err), Some("null db handle".to_string()));

        let mut err: *mut RegolithError = ptr::dangling_mut();
        let status = unsafe { regolith_db_write(db, ptr::null(), 1, &raw mut err) };
        assert_eq!(status, INVALID_ARG);
        assert_eq!(take_error(err), Some("null write batch".to_string()));

        assert_eq!(get(db, b"a"), Ok(b"va".to_vec()));
        assert!(!has(db, b"x"));

        close(db);
    }

    #[test]
    fn db_write_malformed_frame_writes_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);
        seed(db);

        let mut f = frame(&[Op::Set(b"a", b"1")]);
        f.push(9);
        let (status, detail) = write(db, &f);
        assert_eq!(status, INVALID_ARG);
        let detail = detail.expect("invalid_arg carries a detail");
        assert!(detail.contains("op 1"), "{detail:?} missing op 1");
        assert!(
            detail.contains("unknown tag 9"),
            "{detail:?} missing unknown tag 9"
        );

        assert_eq!(get(db, b"a"), Ok(b"va".to_vec()));

        close(db);
    }

    #[test]
    fn db_write_is_all_or_nothing_when_the_engine_refuses() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);

        let oversized = vec![0u8; (8 << 20) + 1];
        let f = frame(&[Op::Set(b"a", b"1"), Op::Set(&oversized, b"x")]);
        let (status, detail) = write(db, &f);
        assert_eq!(status, INVALID_ARG);
        let detail = detail.expect("invalid_arg carries a detail");
        assert!(detail.contains("key length 8388609"), "{detail:?}");
        assert!(
            !detail.starts_with("invalid argument: "),
            "the engine's own prefix must not be doubled: {detail:?}"
        );

        assert_eq!(get(db, b"a"), Err(NOT_FOUND));

        close(db);
    }

    #[test]
    fn db_write_last_op_on_a_key_wins() {
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);

        let f = frame(&[
            Op::Set(b"a", b"1"),
            Op::Set(b"a", b"2"),
            Op::Delete(b"a"),
            Op::Set(b"a", b"3"),
            Op::Set(b"b", b"1"),
            Op::Delete(b"b"),
        ]);
        let (status, detail) = write(db, &f);
        assert_eq!(status, OK, "write failed: {detail:?}");

        assert_eq!(get(db, b"a"), Ok(b"3".to_vec()));
        assert_eq!(get(db, b"b"), Err(NOT_FOUND));

        close(db);
    }
}
