# go-regolith

Go bindings for [regolith](https://github.com/sourcenetwork/regolith), an
embedded key-value engine written in Rust.

The engine is reached through the hand-written C ABI in `ffi/`, a small Rust
staticlib crate that wraps the `regolith` crate and is linked into your binary
with cgo. The Go API on top of it is a plain, framework-free key-value store:
`Get`/`Set`/`Has`/`Delete`, ordered iteration, atomic batch writes, and
optimistic transactions with snapshot isolation.

## Build requirement (read this first)

**This package cannot be built by `go get` alone.** It is cgo over a Rust
staticlib, so a Rust toolchain is required and the staticlib must be built
before any Go build that includes this package:

```sh
git clone https://github.com/sourcenetwork/go-regolith
cd go-regolith
make ffi          # cargo build --release --manifest-path ffi/Cargo.toml
go build ./...
```

Without `make ffi`, the link fails with a missing
`ffi/target/release/libregolith_ffi.a`.

The consequences are worth stating plainly, because this is the main rough edge
of the library today:

- `go get github.com/sourcenetwork/go-regolith` will download the source but
  will **not** produce a working package: the staticlib is a build artefact and
  is not in the module.
- Anything that depends on this module, transitively included, needs the same
  `make ffi` step run against this module's checkout, and therefore needs a Rust
  toolchain on the build machine and in CI.
- Cross-compilation is whatever cargo and cgo can agree on for the target; there
  are no prebuilt artefacts.

On macOS the linker prints a `ld: warning: ... has malformed LC_DYSYMTAB`
warning coming from the Rust staticlib. It is harmless.

## Usage

```go
package main

import (
	"fmt"
	"log"

	"github.com/sourcenetwork/go-regolith"
)

func main() {
	db, err := regolith.Open("/tmp/my-store")
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	if err := db.Set([]byte("hello"), []byte("world")); err != nil {
		log.Fatal(err)
	}

	value, err := db.Get([]byte("hello"))
	if err != nil {
		log.Fatal(err)
	}
	fmt.Printf("%s\n", value)

	// A transaction: optimistic, with snapshot isolation.  A commit that lost a
	// validation race returns regolith.ErrConflict and should be retried from a
	// new transaction.
	txn, err := db.NewTxn(false)
	if err != nil {
		log.Fatal(err)
	}
	defer txn.Discard() // a no-op after a successful commit

	if err := txn.Set([]byte("a"), []byte("1")); err != nil {
		log.Fatal(err)
	}
	if err := txn.Commit(); err != nil {
		log.Fatal(err)
	}

	// A batch: many sets and deletes applied atomically in one call, with the
	// store's durability.  Faster than a transaction for a grouped write, but
	// nothing is validated against concurrent writers.
	batch := regolith.NewWriteBatch(0)
	batch.Set([]byte("b"), []byte("2"))
	batch.Delete([]byte("a"))
	if err := db.Write(batch); err != nil {
		log.Fatal(err)
	}

	// Ordered iteration, over a snapshot taken when the iterator is created.
	it, err := db.NewIter(regolith.IterOptions{Prefix: []byte("h")})
	if err != nil {
		log.Fatal(err)
	}
	defer it.Close()

	for {
		hasNext, err := it.Next()
		if err != nil {
			log.Fatal(err)
		}
		if !hasNext {
			break
		}
		value, err := it.Value()
		if err != nil {
			log.Fatal(err)
		}
		fmt.Printf("%s = %s\n", it.Key(), value)
	}
}
```

## Engine options

`Open` uses regolith's defaults. `OpenWith` takes an `Options`, whose zero value
means the same thing:

```go
db, err := regolith.OpenWith("/tmp/my-store", regolith.Options{
	// Size the transaction buffer's inline walk to the workload's transactions.
	TransactionKeysInline: regolith.Uint64(8),
	// 0 is a real setting here, not "unset": no compaction worker, compaction
	// on the calling thread.
	MaxBackgroundCompactions: regolith.Uint64(0),
	Durability:               regolith.DurabilityImmediate,
	// Validate every key a transaction read, not only the ones it wrote, so
	// write skew aborts instead of committing.  Applies to every transaction the
	// store begins: corekv's `NewTxn(readonly bool)` leaves no room for a
	// per-transaction level.
	Isolation: regolith.IsolationSerializable,
})
```

Only the fields actually set are applied, which is why the numeric ones are
pointers: for several of these settings `0` is a different choice rather than an
absence of one, so it cannot double as "leave it alone". An invalid value is
rejected with an error naming the field, never clamped, and regolith validates
before touching the filesystem, so a rejected open creates nothing.

## Batch writes

`WriteBatch` collects sets and deletes with `Set` and `Delete`, and `DB.Write`
applies them in one call: every op lands or none does, as one write-ahead log
record, with the same durability as `Set`. There is no conflict detection and
no snapshot - a batch is not a transaction - and ops on the same key apply in
the order they were added, so the last one wins. Call `Reset` to reuse a
batch, and watch `Size` to bound how much memory it holds.

That one write-ahead log record is capped by the engine at 1073741824 bytes
(1 GiB), and `DB.Write` rejects a batch that would cross it with
`ErrInvalidArgument` before writing anything, rather than writing and later
losing it on a crash and reopen. The record is not `Size`: it runs `Size`
bytes plus 8 for every set, 12 for every delete, plus 4, so split a batch
into smaller ones well before `Size` alone reaches 1 GiB. See
`WriteBatch.Size` for the exact rule.

## Handle ordering

The FFI layer owns real Rust handles, and they have to be released in order:

1. An `Iter` created from a `Txn` must be closed before that transaction is
   committed or discarded.
2. Every `Txn` and `Iter` must be resolved before `DB.Close`.

`Close`, `Commit` and `Discard` are each safe to call more than once; a second
call is a no-op (`Commit` after a resolution returns `ErrDiscarded`).

## Errors

All errors are sentinel values usable with `errors.Is`: `ErrNotFound`,
`ErrClosed`, `ErrConflict`, `ErrReadOnly`, `ErrDiscarded`, plus
`ErrInvalidArgument`, `ErrPanic` and `ErrUnexpected`, which are wrapped with the
detail the FFI layer hands back with the status.

## Make targets

| Target      | What it does                                      |
| ----------- | ------------------------------------------------- |
| `ffi`       | Build the release staticlib (required before Go)  |
| `ffi-debug` | Build the debug staticlib                         |
| `test-ffi`  | Run the Rust ABI tests                            |
| `test`      | `make ffi`, then `go test ./...`                  |
| `clean`     | `cargo clean`                                     |

## Current limitations

- The build requirement above.
- Only a small subset of regolith's engine `Options` crosses the FFI so far:
  `WriteBufferSize`, `BlockCacheSize`, `MaxBackgroundCompactions`,
  `TransactionKeysInline`, `Compression`, `Durability` and `Isolation`. The rest
  of that type
  is mostly trait-object hooks (compaction filters, merge operators, event
  listeners, a pluggable `Env`) with no C representation.

## License

Dual licensed under Apache-2.0 or MIT, at your option.
