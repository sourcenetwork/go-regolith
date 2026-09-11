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
# removed.  About half the size of the LTO archive (9.7 MB to 4.7 MB on x86_64
# Linux); the caller's linked binary shrinks by only a few percent, because the
# linker already drops most of what strip removes.
#
# Cargo's own `strip` profile setting does nothing here: stripping happens at
# link time and a staticlib is never linked, it is an archive of object files.
# Fat LTO (ffi/Cargo.toml) does most of the shrinking on its own by folding
# the dependencies into one optimised object (28.3 MB to 9.7 MB); what strip
# removes on top is the debug information that arrives with the precompiled
# standard library and compiler-builtins objects, which the profile cannot
# reach.
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
