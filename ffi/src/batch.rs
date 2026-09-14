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

/// Largest write-ahead-log record the engine will accept for one batch,
/// mirroring its own limit (regolith 0.1.6's private `MAX_RECORD_LEN`,
/// `src/engine/wal.rs:120`, itself `1 << 30`). Since 0.1.6 `Db::write`
/// refuses a record past it through `check_write_len` (`src/lib.rs:999`);
/// through 0.1.4 it accepted one, and `WalReplayIter::next_entry_inner`
/// (`src/engine/wal_replay.rs:218`) then discarded the whole record after a
/// crash. Checking here as well rejects the batch while it is still being
/// decoded, before any op reaches the engine, so every op lands or none
/// does.
///
/// What this bounds is the record's payload, the length the record header
/// stores and replay checks, not the 5-byte header and 4-byte checksum
/// framed around it (`record_len`, `wal.rs:501-505`). Summed over a batch,
/// that payload is
///
/// ```text
/// 4 + sum(set: 25 + key + value, delete: 21 + key)
/// ```
///
/// with each term derived below; `decode` charges exactly that, op by op.
pub(crate) const MAX_BATCH_RECORD_LEN: u64 = 1 << 30;

/// Bytes the engine's batch record payload opens with, ahead of the ops
/// themselves: a little-endian op count (`batch_ops_payload_len`,
/// `wal.rs:642-647`).
const BATCH_COUNT_LEN: u64 = 4;

/// Engine-side bytes one set op adds to the batch record beyond its key
/// and value bytes: `encode_batch_header`'s 1-byte record type plus
/// 4-byte length (`wal.rs:649-652`, 5), the 4-byte column-family id every
/// stored key is prefixed with (`prefix_key`, `column_family.rs:122-127`,
/// 4), and `put_payload_len`'s own 4-byte key length, 4-byte value length
/// and 8-byte sequence number (`wal.rs:613-615`): 5 + 4 + 4 + 4 + 8.
const SET_RECORD_OVERHEAD: u64 = 5 + 4 + 4 + 4 + 8;

/// As [`SET_RECORD_OVERHEAD`], for a delete: the same batch-entry header
/// and column-family prefix, plus `delete_payload_len`'s 4-byte key
/// length and 8-byte sequence number (`wal.rs:617-619`), and no value:
/// 5 + 4 + 4 + 8.
const DELETE_RECORD_OVERHEAD: u64 = 5 + 4 + 4 + 8;

/// Longest frame that cannot produce an oversized record whatever it
/// holds, so `decode` can skip its sizing pass below this and build the
/// batch in one pass.
///
/// A set costs the record 8 bytes more than the frame (25 + key + value
/// against a frame's 1 + 8 + key + 8 + value) and a delete 12 more (21 +
/// key against 1 + 8 + key), so a delete is the worse of the two, and the
/// smallest one a frame can hold is 9 bytes (an empty key). A frame of
/// `n` bytes therefore holds at most `n / 9` ops, and
///
/// ```text
/// record_len <= 4 + n + 12 * (n / 9) = 4 + n * 7 / 3
/// ```
///
/// which stays within [`MAX_BATCH_RECORD_LEN`] exactly while
/// `n <= (MAX_BATCH_RECORD_LEN - 4) * 3 / 7`. Every batch that is refused
/// is above this, by construction: a record past 1 GiB needs more than
/// `(1 << 30) / 21` deletes, which is more than this many frame bytes.
const MAX_FRAME_WITHOUT_SIZING: u64 = (MAX_BATCH_RECORD_LEN - BATCH_COUNT_LEN) * 3 / 7;

/// Engine-side record bytes one set op with a `key_len`-byte key and a
/// `value_len`-byte value adds, on top of [`BATCH_COUNT_LEN`]: see
/// [`SET_RECORD_OVERHEAD`]. `decode` charges every set through this
/// function and nowhere else, so a boundary test calling it the same way
/// pins what `decode` actually does, not a separate copy of the formula.
fn set_record_len(key_len: u64, value_len: u64) -> u64 {
    SET_RECORD_OVERHEAD + key_len + value_len
}

/// As [`set_record_len`], for a delete with a `key_len`-byte key: see
/// [`DELETE_RECORD_OVERHEAD`].
fn delete_record_len(key_len: u64) -> u64 {
    DELETE_RECORD_OVERHEAD + key_len
}

/// Running total of the engine's own batch record bytes. `decode` and the
/// boundary tests below both start one with [`RecordLen::new`], so the two
/// can never start from different numbers.
struct RecordLen(u64);

impl RecordLen {
    /// A running total at [`BATCH_COUNT_LEN`], the record's own starting
    /// point ahead of any op.
    fn new() -> Self {
        Self(BATCH_COUNT_LEN)
    }

    /// Add one op's engine-side record bytes, rejecting before the total
    /// would cross [`MAX_BATCH_RECORD_LEN`]. `index` names the op in the
    /// error. Called before the op reaches `batch.put`/`batch.delete`, so
    /// a refused op, and everything after it, never reaches the engine.
    fn charge(&mut self, add: u64, index: usize) -> Result<(), Failure> {
        let next = self.0.saturating_add(add);
        if next > MAX_BATCH_RECORD_LEN {
            return Err(Failure::invalid_arg(format!(
                "write batch op {index} would push this write batch past its \
                 {MAX_BATCH_RECORD_LEN}-byte limit; split it into smaller batches"
            )));
        }
        self.0 = next;
        Ok(())
    }

    /// The running total so far. `decode` never needs to read it back
    /// (only `charge`'s own rejection matters there); the boundary tests
    /// below use it to check they landed exactly on the limit.
    #[cfg(test)]
    fn get(&self) -> u64 {
        self.0
    }
}

/// One op's tag and length-prefixed fields, borrowed from the frame with
/// no copy.
enum RawOp<'a> {
    Set(&'a [u8], &'a [u8]),
    Delete(&'a [u8]),
}

/// Walk a frame's ops in order, calling `on_op` with each one's index and
/// borrowed fields. `walk` itself never touches a [`WriteBatch`] and never
/// copies a key or value, only the slices `frame` already holds, so
/// [`decode`] can size a frame with one call and build the batch with
/// another, off the same parsing code rather than two copies of it.
fn walk<'a>(
    frame: &'a [u8],
    mut on_op: impl FnMut(usize, RawOp<'a>) -> Result<(), Failure>,
) -> Result<(), Failure> {
    let mut rest = frame;
    let mut index = 0usize;
    while let Some((&tag, tail)) = rest.split_first() {
        rest = tail;
        match tag {
            OP_SET => {
                let key = field(&mut rest, index, "key")?;
                let value = field(&mut rest, index, "value")?;
                on_op(index, RawOp::Set(key, value))?;
            }
            OP_DELETE => {
                let key = field(&mut rest, index, "key")?;
                on_op(index, RawOp::Delete(key))?;
            }
            other => {
                return Err(Failure::invalid_arg(format!(
                    "write batch op {index} has unknown tag {other}"
                )));
            }
        }
        index += 1;
    }
    Ok(())
}

/// Decode a batch frame into a [`WriteBatch`], rejecting a malformed frame,
/// or one whose engine-side record would exceed [`MAX_BATCH_RECORD_LEN`],
/// before any op reaches the engine.
///
/// A frame over [`MAX_FRAME_WITHOUT_SIZING`] gets a sizing pass first,
/// reading only tags and lengths and charging the engine's record bytes
/// through [`RecordLen`], so an oversized frame is rejected without ever
/// allocating a [`WriteBatch`] or copying a key or value into one: a batch
/// refused halfway through building would hold gigabytes of ops the engine
/// never sees. Every other frame, which is every batch that cannot reach
/// the limit, skips straight to the single build pass and pays nothing.
///
/// The build pass's memory is bounded by the frame the caller built
/// (regolith's own `WriteBatch` copies on `put`/`delete`).
pub(crate) fn decode(frame: &[u8]) -> Result<WriteBatch, Failure> {
    if frame.len() as u64 > MAX_FRAME_WITHOUT_SIZING {
        let mut record_len = RecordLen::new();
        walk(frame, |index, op| {
            let add = match op {
                RawOp::Set(key, value) => set_record_len(key.len() as u64, value.len() as u64),
                RawOp::Delete(key) => delete_record_len(key.len() as u64),
            };
            record_len.charge(add, index)
        })?;
    }

    let mut batch = WriteBatch::new();
    walk(frame, |_, op| {
        match op {
            RawOp::Set(key, value) => batch.put(key, value),
            RawOp::Delete(key) => batch.delete(key),
        }
        Ok(())
    })?;
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
    use std::collections::BTreeMap;
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

    // -----------------------------------------------------------------
    // Boundary tests: each drives `set_record_len`/`delete_record_len`
    // and `RecordLen`, the exact pieces `decode` charges every op
    // through, rather than a formula reimplemented in the test. The
    // "lands exactly" target in each is worked out from the raw
    // overhead constants directly (not through those functions), so a
    // wrong constant or a dropped term in either function shows up as
    // the running total missing MAX_BATCH_RECORD_LEN by the difference,
    // not as a self-cancelling tautology. No key or value bytes are
    // ever allocated: every length below is a plain `u64`.
    // -----------------------------------------------------------------

    #[test]
    fn boundary_batch_of_sets_lands_exactly_and_rejects_one_byte_over() {
        // Sixteen 5-byte-keyed sets: fifteen full 64 MiB values, and a
        // sixteenth tuned so the engine's record lands exactly on
        // MAX_BATCH_RECORD_LEN. The same shape measured end to end
        // against regolith 0.1.4: a record of exactly 1 << 30 bytes
        // survives a crash and reopen, one byte more is lost. Since 0.1.6
        // the engine refuses that one-byte-over record instead.
        let key_len = 5u64;
        let full_value_len = 64 * 1024 * 1024u64;
        let sets = 16u64;
        let last_at_limit = MAX_BATCH_RECORD_LEN
            - BATCH_COUNT_LEN
            - (sets - 1) * (SET_RECORD_OVERHEAD + key_len + full_value_len)
            - (SET_RECORD_OVERHEAD + key_len);
        assert_eq!(last_at_limit, 67_108_380);

        let mut record_len = RecordLen::new();
        for i in 0..sets - 1 {
            record_len
                .charge(set_record_len(key_len, full_value_len), i as usize)
                .expect("below the limit is accepted");
        }
        record_len
            .charge(set_record_len(key_len, last_at_limit), (sets - 1) as usize)
            .expect("landing exactly on the limit is accepted");
        assert_eq!(record_len.get(), MAX_BATCH_RECORD_LEN);

        // The same batch with its last value one byte larger crosses the
        // limit on that op and is rejected, with nothing charged past it.
        let mut record_len = RecordLen::new();
        for i in 0..sets - 1 {
            record_len
                .charge(set_record_len(key_len, full_value_len), i as usize)
                .unwrap();
        }
        let err = record_len
            .charge(
                set_record_len(key_len, last_at_limit + 1),
                (sets - 1) as usize,
            )
            .expect_err("one byte past the limit is rejected");
        assert_eq!(err.status, INVALID_ARG);
        let detail = err.detail.expect("invalid_arg carries a detail");
        assert!(detail.contains("op 15"), "{detail:?} missing op 15");
        assert!(
            detail.contains(&MAX_BATCH_RECORD_LEN.to_string()),
            "{detail:?} missing the byte limit"
        );
        assert!(detail.contains("split it"), "{detail:?} missing what to do");
    }

    #[test]
    fn boundary_batch_of_deletes_lands_exactly_and_rejects_one_byte_over() {
        // Every op a delete, so the boundary exercises `delete_record_len`
        // (not `set_record_len`): a dropped `key_len` term there, or an
        // overhead missing the column-family prefix, would show up only
        // here.
        let filler_key_len = 1_000u64;
        let filler_deletes = 4u64;
        let last_key_len_at_limit = MAX_BATCH_RECORD_LEN
            - BATCH_COUNT_LEN
            - filler_deletes * (DELETE_RECORD_OVERHEAD + filler_key_len)
            - DELETE_RECORD_OVERHEAD;
        assert_eq!(last_key_len_at_limit, 1_073_737_715);

        let mut record_len = RecordLen::new();
        for i in 0..filler_deletes {
            record_len
                .charge(delete_record_len(filler_key_len), i as usize)
                .expect("filler delete is well under the limit");
        }
        record_len
            .charge(
                delete_record_len(last_key_len_at_limit),
                filler_deletes as usize,
            )
            .expect("landing exactly on the limit is accepted");
        assert_eq!(record_len.get(), MAX_BATCH_RECORD_LEN);

        let mut record_len = RecordLen::new();
        for i in 0..filler_deletes {
            record_len
                .charge(delete_record_len(filler_key_len), i as usize)
                .unwrap();
        }
        let err = record_len
            .charge(
                delete_record_len(last_key_len_at_limit + 1),
                filler_deletes as usize,
            )
            .expect_err("one byte past the limit is rejected");
        assert_eq!(err.status, INVALID_ARG);
        let detail = err.detail.expect("invalid_arg carries a detail");
        assert!(
            detail.contains(&format!("op {filler_deletes}")),
            "{detail:?} missing op {filler_deletes}"
        );
        assert!(
            detail.contains(&MAX_BATCH_RECORD_LEN.to_string()),
            "{detail:?} missing the byte limit"
        );
    }

    #[test]
    fn boundary_mixed_batch_lands_exactly_and_rejects_one_byte_over() {
        // Two filler sets and two filler deletes, then a final set tuned
        // to land exactly on the limit: both formulas contribute to the
        // running total, as a real mixed batch would exercise both.
        let filler_set_key_len = 10u64;
        let filler_set_value_len = 2_000u64;
        let filler_delete_key_len = 500u64;
        let last_key_len = 8u64;

        let filler_total = 2 * (SET_RECORD_OVERHEAD + filler_set_key_len + filler_set_value_len)
            + 2 * (DELETE_RECORD_OVERHEAD + filler_delete_key_len);
        let last_value_len_at_limit = MAX_BATCH_RECORD_LEN
            - BATCH_COUNT_LEN
            - filler_total
            - (SET_RECORD_OVERHEAD + last_key_len);
        assert_eq!(last_value_len_at_limit, 1_073_736_675);

        let charge_filler = |record_len: &mut RecordLen| {
            record_len
                .charge(set_record_len(filler_set_key_len, filler_set_value_len), 0)
                .unwrap();
            record_len
                .charge(set_record_len(filler_set_key_len, filler_set_value_len), 1)
                .unwrap();
            record_len
                .charge(delete_record_len(filler_delete_key_len), 2)
                .unwrap();
            record_len
                .charge(delete_record_len(filler_delete_key_len), 3)
                .unwrap();
        };

        let mut record_len = RecordLen::new();
        charge_filler(&mut record_len);
        record_len
            .charge(set_record_len(last_key_len, last_value_len_at_limit), 4)
            .expect("landing exactly on the limit is accepted");
        assert_eq!(record_len.get(), MAX_BATCH_RECORD_LEN);

        let mut record_len = RecordLen::new();
        charge_filler(&mut record_len);
        let err = record_len
            .charge(set_record_len(last_key_len, last_value_len_at_limit + 1), 4)
            .expect_err("one byte past the limit is rejected");
        assert_eq!(err.status, INVALID_ARG);
        let detail = err.detail.expect("invalid_arg carries a detail");
        assert!(detail.contains("op 4"), "{detail:?} missing op 4");
        assert!(
            detail.contains(&MAX_BATCH_RECORD_LEN.to_string()),
            "{detail:?} missing the byte limit"
        );
    }

    #[test]
    fn a_frame_under_the_sizing_guard_cannot_reach_the_record_limit() {
        // The guard is what lets `decode` skip its sizing pass, so it has
        // to be safe in the worst case, not the common one: a frame packed
        // end to end with the cheapest op the format has, a 9-byte
        // empty-key delete, each of which costs the engine 21 bytes.
        let worst_case_record = |frame_len: u64| {
            let deletes = frame_len / (1 + 8);
            BATCH_COUNT_LEN + deletes * delete_record_len(0)
        };

        assert!(
            worst_case_record(MAX_FRAME_WITHOUT_SIZING) <= MAX_BATCH_RECORD_LEN,
            "a frame of {MAX_FRAME_WITHOUT_SIZING} bytes can reach {}, past the limit",
            worst_case_record(MAX_FRAME_WITHOUT_SIZING)
        );

        // And the other side: the smallest frame that can produce an
        // oversized record is above the guard, so no refused batch ever
        // skips the sizing pass.
        let deletes_to_pass_the_limit =
            (MAX_BATCH_RECORD_LEN - BATCH_COUNT_LEN) / delete_record_len(0) + 1;
        let smallest_oversized_frame = deletes_to_pass_the_limit * (1 + 8);
        assert!(
            BATCH_COUNT_LEN + deletes_to_pass_the_limit * delete_record_len(0)
                > MAX_BATCH_RECORD_LEN
        );
        assert!(
            smallest_oversized_frame > MAX_FRAME_WITHOUT_SIZING,
            "{smallest_oversized_frame} would skip the sizing pass"
        );
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
        let dir = TempDir::new().expect("tempdir");
        let db = open(&dir);
        let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

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

            // Content, not just length: apply the round through the real
            // FFI write path and check every key it touched against a plain
            // model, so a decoder that mangles or truncates a value (for
            // example, silently capping it) fails here even though every
            // op still counts correctly.
            let (status, detail) = write(db, &f);
            assert_eq!(status, OK, "round {round} write failed: {detail:?}");
            for (is_set, key, value) in &ops {
                if *is_set {
                    model.insert(key.clone(), value.clone());
                } else {
                    model.remove(key);
                }
            }
            for (_, key, _) in &ops {
                assert_eq!(
                    get(db, key).ok(),
                    model.get(key).cloned(),
                    "round {round} key {key:?}"
                );
            }

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

        close(db);
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
