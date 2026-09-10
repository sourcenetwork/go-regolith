# Builds the Rust FFI staticlib that the cgo package in this directory links
# against.  `go build ./...` requires `make ffi` first.
#
# Output: ffi/target/release/libregolith_ffi.a

.PHONY: ffi ffi-debug test-ffi test clean

ffi:
	cargo build --release --manifest-path ffi/Cargo.toml

ffi-debug:
	cargo build --manifest-path ffi/Cargo.toml

test-ffi:
	cargo test --manifest-path ffi/Cargo.toml

test: ffi
	go test ./...

clean:
	cargo clean --manifest-path ffi/Cargo.toml
