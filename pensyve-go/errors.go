package pensyve

import (
	"errors"
	"net"
	"net/url"
)

// Sentinel errors returned by the client for well-known HTTP status codes.
var (
	ErrNotFound     = errors.New("pensyve: not found")
	ErrUnauthorized = errors.New("pensyve: unauthorized")
	ErrRateLimited  = errors.New("pensyve: rate limited")
)

// IsRetryable reports whether the error represents a condition that may
// succeed on a subsequent attempt (i.e., HTTP 5xx responses or transient
// network errors).
func IsRetryable(err error) bool {
	if err == nil {
		return false
	}

	// HTTP 5xx errors are retryable
	var pe *PensyveError
	if errors.As(err, &pe) {
		return pe.Status >= 500
	}

	// Network errors are retryable (DNS, connection refused, timeout, etc.)
	// Check for common transient network error types
	var netErr net.Error
	if errors.As(err, &netErr) {
		return true
	}

	// url.Error wraps transport errors
	var urlErr *url.Error
	if errors.As(err, &urlErr) {
		return true
	}

	return false
}

// IsNotFound reports whether the error wraps ErrNotFound (HTTP 404).
func IsNotFound(err error) bool { return errors.Is(err, ErrNotFound) }

// IsUnauthorized reports whether the error wraps ErrUnauthorized (HTTP 401).
func IsUnauthorized(err error) bool { return errors.Is(err, ErrUnauthorized) }

// IsRateLimited reports whether the error wraps ErrRateLimited (HTTP 429).
func IsRateLimited(err error) bool { return errors.Is(err, ErrRateLimited) }
