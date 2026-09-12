package regolith

// #include <stdlib.h>
// #include "regolith_ffi.h"
import "C"

import (
	"errors"
	"fmt"
)

// The errors returned by this package.  They are returned directly, or wrapped
// with the detail the FFI layer hands back alongside the status where the code
// alone does not say what went wrong, so they are all usable with [errors.Is].
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

// statusToErr maps a status code onto the matching package error.
//
// detail is what the same call wrote to its error out-param: nil unless the
// code is one of the three that carry a message, in which case the message is
// wrapped into the error and the object is released here.  It arrives with the
// status rather than through a second call, so there is nothing to read
// promptly and nothing that another goroutine on the same thread could have
// overwritten.
func statusToErr(status C.int32_t, detail *C.RegolithError) error {
	var message string
	if detail != nil {
		message = C.GoString(C.regolith_error_message(detail))
		C.regolith_error_free(detail)
	}

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
		return withDetail(ErrInvalidArgument, message)
	case C.REGOLITH_ERR_PANIC:
		return withDetail(ErrPanic, message)
	default:
		return withDetail(ErrUnexpected, message)
	}
}

// withDetail wraps err with the detail message, or returns it bare when there
// is none.
func withDetail(err error, message string) error {
	if message == "" {
		return err
	}

	return fmt.Errorf("%w: %s", err, message)
}
