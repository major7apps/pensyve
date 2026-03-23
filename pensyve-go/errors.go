package pensyve

import "errors"

// Sentinel errors returned by the client for well-known HTTP status codes.
var (
	ErrNotFound     = errors.New("pensyve: not found")
	ErrUnauthorized = errors.New("pensyve: unauthorized")
	ErrRateLimited  = errors.New("pensyve: rate limited")
)

// IsRetryable reports whether the error represents a condition that may
// succeed on a subsequent attempt (i.e., HTTP 5xx responses).
func IsRetryable(err error) bool {
	var pe *PensyveError
	if errors.As(err, &pe) {
		return pe.Status >= 500
	}
	return false
}

// IsNotFound reports whether the error wraps ErrNotFound (HTTP 404).
func IsNotFound(err error) bool { return errors.Is(err, ErrNotFound) }

// IsUnauthorized reports whether the error wraps ErrUnauthorized (HTTP 401).
func IsUnauthorized(err error) bool { return errors.Is(err, ErrUnauthorized) }

// IsRateLimited reports whether the error wraps ErrRateLimited (HTTP 429).
func IsRateLimited(err error) bool { return errors.Is(err, ErrRateLimited) }
