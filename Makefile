# Builds the Rust FFI staticlib that the cgo package in this directory links
# against.  `go build ./...` requires `make ffi` first.
#
# Output: ffi/target/release/libregolith_ffi.a

# macOS `strip` spells "remove debug information" as -S; GNU binutils as
# --strip-debug.  Neither removes the exported symbols the linker needs.
STRIP_DEBUG_FLAG := $(if $(filter Darwin,$(shell uname -s)),-S,--strip-debug)

.PHONY: ffi ffi-debug ffi-dist test-ffi test clean

ffi:
	cargo build --release --manifest-path ffi/Cargo.toml

ffi-debug:
	cargo build --manifest-path ffi/Cargo.toml

# The archive distributed in go-regolith-prebuilt, with debug information
# removed.  Roughly a third smaller, and it shrinks the caller's linked binary
# by about as much, because the linker copies less in.
#
# Cargo's own `strip` profile setting does nothing here: stripping happens at
# link time and a staticlib is never linked, it is an archive of object files.
# LTO is no help either - fat LTO embeds bitcode and makes the archive larger.
# So the only thing that works is stripping the archive after the fact.
#
# `ffi` deliberately stays unstripped so that a Rust-side crash in development
# still symbolicates.
ffi-dist: ffi
	strip $(STRIP_DEBUG_FLAG) ffi/target/release/libregolith_ffi.a
	@ls -l ffi/target/release/libregolith_ffi.a

test-ffi:
	cargo test --manifest-path ffi/Cargo.toml

test: ffi
	go test ./...

clean:
	cargo clean --manifest-path ffi/Cargo.toml
