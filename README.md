# go-regolith

Go bindings for [regolith](https://github.com/sourcenetwork/regolith), an
embedded key-value engine written in Rust.

The engine is reached through the hand-written C ABI in `ffi/`, a small Rust
staticlib crate that wraps the `regolith` crate and is linked into your binary
with cgo. The Go API on top of it is a plain, framework-free key-value store:
`Get`/`Set`/`Has`/`Delete`, ordered iteration, and optimistic transactions with
snapshot isolation.

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
detail string the FFI layer records.

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
- regolith's own engine `Options` are not exposed across the FFI yet, so a store
  is always opened with the engine defaults and `Open` takes no options.

## License

Dual licensed under Apache-2.0 or MIT, at your option.
