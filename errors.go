package regolith

// #include <stdlib.h>
// #include "regolith_ffi.h"
import "C"

import (
	"errors"
	"fmt"
)

// The errors returned by this package.  They are returned directly, or wrapped
// with the detail string from `regolith_last_error_message` where the FFI layer
// has one, so they are all usable with [errors.Is].
var (
	// ErrNotFound is returned when a key has no entry.
	ErrNotFound = errors.New("key not found")

	// ErrClosed is returned by every call on a closed [DB], and by every call on
	// a [Txn] or [Iter] derived from one.
	ErrClosed = errors.New("db closed")

	// ErrConflict is returned by [Txn.Commit] when commit-time validation lost a
	// race with another writer.  The transaction is resolved either way and the
	// work should be retried from a new one.
	ErrConflict = errors.New("transaction conflict")

	// ErrReadOnly is returned by [Txn.Set] and [Txn.Delete] on a read-only
	// transaction.
	ErrReadOnly = errors.New("read-only transaction")

	// ErrDiscarded is returned by every call on a transaction that has already
	// been committed or discarded.
	ErrDiscarded = errors.New("transaction already resolved")

	// ErrInvalidArgument is returned when the FFI layer rejects an argument,
	// which for this package means a bug in it or a handle it believes to be
	// live.
	ErrInvalidArgument = errors.New("invalid argument")

	// ErrPanic is returned when a Rust panic was caught at the FFI boundary.  No
	// unwind crosses it, but the handle that produced it should be treated as
	// unusable.
	ErrPanic = errors.New("panic at the regolith ffi boundary")

	// ErrUnexpected is returned for any other engine failure; the wrapped detail
	// is the only description of it.
	ErrUnexpected = errors.New("regolith error")
)

// statusToErr maps a regolith status code onto the matching package error.
//
// It must be called immediately after the failing FFI call and on the same
// goroutine: the detail string for the codes that carry one lives in a
// thread-local slot on the Rust side, and any further call on that thread
// overwrites it.  Go may migrate a goroutine between OS threads at almost any
// point, but not between two statements containing no function calls other than
// these, which is why the read happens here and not later.
func statusToErr(status C.int32_t) error {
	switch status {
	case C.REGOLITH_OK:
		return nil
	case C.REGOLITH_ERR_NOT_FOUND:
		return ErrNotFound
	case C.REGOLITH_ERR_DB_CLOSED:
		return ErrClosed
	case C.REGOLITH_ERR_TXN_CONFLICT:
		return ErrConflict
	case C.REGOLITH_ERR_READ_ONLY_TXN:
		return ErrReadOnly
	case C.REGOLITH_ERR_DISCARDED:
		return ErrDiscarded
	case C.REGOLITH_ERR_INVALID_ARG:
		return detailedErr(ErrInvalidArgument)
	case C.REGOLITH_ERR_PANIC:
		return detailedErr(ErrPanic)
	default:
		return detailedErr(ErrUnexpected)
	}
}

// detailedErr wraps the given error with the last error detail recorded by the
// Rust side, if there is one.
func detailedErr(err error) error {
	message := C.regolith_last_error_message()
	if message == nil {
		return err
	}
	defer C.regolith_free_string(message)

	return fmt.Errorf("%w: %s", err, C.GoString(message))
}
