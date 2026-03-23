package pensyve

import (
	"math"
	"math/rand"
	"time"
)

// RetryConfig controls exponential-backoff retry behaviour for the client.
type RetryConfig struct {
	// MaxRetries is the maximum number of retry attempts after the first failure.
	MaxRetries int
	// BaseDelay is the starting delay before the first retry.
	BaseDelay time.Duration
	// MaxDelay caps the computed delay regardless of the attempt number.
	MaxDelay time.Duration
	// JitterFraction adds randomness to each delay. A value of 0.5 means the
	// actual delay is between 50% and 100% of the exponential base.
	JitterFraction float64
}

// DefaultRetryConfig returns a RetryConfig with sensible production defaults:
// 3 retries, 500 ms base delay, 30 s ceiling, 50% jitter.
func DefaultRetryConfig() *RetryConfig {
	return &RetryConfig{
		MaxRetries:     3,
		BaseDelay:      500 * time.Millisecond,
		MaxDelay:       30 * time.Second,
		JitterFraction: 0.5,
	}
}

// delay returns the wait duration for the given attempt index (0-based).
func (rc *RetryConfig) delay(attempt int) time.Duration {
	base := float64(rc.BaseDelay) * math.Pow(2, float64(attempt))
	// jitter: scale between (1 - JitterFraction) and 1.0 of the exponential value
	jitter := (1 - rc.JitterFraction) + rand.Float64()*rc.JitterFraction
	d := time.Duration(base * jitter)
	if d > rc.MaxDelay {
		return rc.MaxDelay
	}
	return d
}
