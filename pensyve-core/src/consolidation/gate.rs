//! Per-namespace serialization for consolidation runs.
//!
//! The promotion pass is not safe to run twice over one namespace at once.
//! It reads the namespace's rows, derives its idempotency guard from that
//! snapshot, and only then begins writing — so two runs that both snapshot
//! before either writes each conclude that nothing has been promoted yet and
//! both mint the same `(about_entity, content)` row. That is the duplicate-row
//! class #219 closed for sequential runs, reopened for overlapping ones
//! (#226).
//!
//! Four call sites start a run and none of them coordinated: the periodic
//! sweep in the gateway's `main.rs`, the fire-and-forget `episode_end` spawns
//! in the gateway's `rest.rs` and the MCP tool server, and the on-demand
//! `/consolidate` endpoint. Rather than ask each of them to remember a lock,
//! [`ConsolidationEngine::run`] takes the lock itself, so the serialization
//! cannot be bypassed — including by call sites added later.
//!
//! [`ConsolidationEngine::run`]: super::ConsolidationEngine::run
//!
//! ## Why the lock is per namespace
//!
//! The hazard is two runs over the *same* row set. Runs on different
//! namespaces touch disjoint rows and are free to proceed in parallel; a
//! single global lock would serialize unrelated tenants behind each other and
//! buy nothing, so the lock is keyed on `namespace_id`.
//!
//! ## Why a blocking lock is safe here
//!
//! [`ConsolidationEngine::run`] is a synchronous function: the guarded region
//! contains no `.await`, and therefore no suspension point at which a holder
//! could yield while still holding the lock. A thread that holds this lock is
//! always a thread that is actively running to completion, so it never needs
//! to be re-scheduled in order to release. That is what makes waiting on it
//! deadlock-free no matter how many waiters pile up or which Tokio runtime
//! flavor they sit on — the usual "never block an async worker on a lock"
//! hazard needs a holder that is itself waiting to be polled, and this one
//! cannot be. The engine is likewise not re-entrant: `run` never calls `run`,
//! so a thread cannot deadlock against itself.
//!
//! Callers that must not block a runtime worker for the *duration* of a run
//! should dispatch through `tokio::task::spawn_blocking`, which is what every
//! gateway call site now does — that is a property of the run itself being CPU-
//! and IO-bound, not of this lock.
//!
//! ## Scope
//!
//! This is a process-local lock. It does not coordinate two processes sharing
//! one database file (a gateway and a CLI invocation, say); that would need a
//! guard in the storage layer. #226 is about overlapping in-process spawns,
//! which is what this closes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// One namespace's run lock. The payload is `()` — the lock exists purely for
/// mutual exclusion, so there is no invariant a panicking holder could leave
/// broken and poisoning carries no information worth honoring.
type NamespaceLock = Arc<Mutex<()>>;

/// Live locks, keyed by namespace. Entries are created on first use and
/// dropped again once nothing holds or awaits them, so the map tracks
/// *in-flight* runs rather than growing once per namespace the process has
/// ever consolidated.
static REGISTRY: OnceLock<Mutex<HashMap<Uuid, NamespaceLock>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<Uuid, NamespaceLock>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the registry lock, recovering rather than propagating poison.
///
/// A panic while the registry is held can only have interrupted a map lookup,
/// which leaves the map itself structurally intact. Propagating the poison
/// would wedge consolidation for every namespace in the process forever, so
/// the guard is recovered instead.
fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<Uuid, NamespaceLock>> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

/// The lock for `namespace_id`, creating it if this is the first run in
/// flight for that namespace. Cloning under the registry lock is what makes
/// [`release`]'s reference-count check sound.
fn acquire_handle(namespace_id: Uuid) -> NamespaceLock {
    Arc::clone(lock_registry().entry(namespace_id).or_default())
}

/// Drop the registry's entry for `namespace_id` if this caller held the last
/// outstanding handle.
///
/// The check runs while the registry is held and handles are only ever cloned
/// while it is held, so a strong count of 1 proves the map owns the sole
/// reference: no one is inside the lock and no one is waiting to be. A handle
/// taken after this point is a fresh lock that nobody else could still be
/// holding, so removal cannot let two runs overlap.
fn release(namespace_id: Uuid) {
    let mut reg = lock_registry();
    if reg
        .get(&namespace_id)
        .is_some_and(|lock| Arc::strong_count(lock) == 1)
    {
        reg.remove(&namespace_id);
    }
}

/// How often a waiting caller re-checks `cancel`. Well inside the ≤500 ms
/// cancellation-response budget of pre-reg §2 I5, and cheap: a run can be
/// capped at `max_duration_secs` (60 s by default), so even a maximal wait is
/// a few thousand wakeups.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Run `f` with exclusive access to `namespace_id`'s consolidation slot,
/// waiting for any run already in flight on that namespace to finish. Runs on
/// other namespaces are unaffected and proceed in parallel.
///
/// Returns `None` without invoking `f` if `cancel` fires while this caller is
/// queued. Waiting is a poll loop rather than a plain blocking acquire for
/// exactly this reason: a run can hold the slot for `max_duration_secs`, so a
/// caller that blocked outright would not observe cancellation until the run
/// ahead of it finished — up to a minute past the I5 budget, and long enough
/// to stall gateway shutdown behind an `episode_end` consolidation.
///
/// An uncontended caller never has its token read at all, so this wrapper is
/// invisible to callers that were not going to wait.
///
/// Acquisition is therefore not FIFO-fair. Neither is `std::sync::Mutex`, and
/// the contended case here is a handful of triggers on one namespace, so
/// starvation is not a practical concern.
///
/// A panic inside `f` propagates once the slot is released; it does not poison
/// the namespace against later runs.
pub fn with_namespace_lock<T>(
    namespace_id: Uuid,
    cancel: &CancellationToken,
    f: impl FnOnce() -> T,
) -> Option<T> {
    let handle = acquire_handle(namespace_id);
    let out = loop {
        // Poison means a previous run panicked mid-consolidation. The storage
        // layer commits one transaction at a time, so the namespace is left at
        // a committed boundary rather than half-written, and the next run
        // re-derives its state from storage anyway. Recovering keeps one
        // panicking run from disabling consolidation for the namespace for the
        // life of the process.
        match handle.try_lock() {
            Ok(_held) => break Some(f()),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let _held = poisoned.into_inner();
                break Some(f());
            }
            // Cancellation is consulted only here, on the edge of an actual
            // wait. An uncontended caller is handed the slot without the gate
            // ever reading the token, which keeps this wrapper transparent:
            // a pre-cancelled run still reports its own engine-entry
            // breadcrumb rather than a queueing one it never experienced.
            Err(std::sync::TryLockError::WouldBlock) => {
                if cancel.is_cancelled() {
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    };
    // Drop this caller's handle before the count check, so the last one out
    // sees the map's own reference alone.
    drop(handle);
    release(namespace_id);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    /// A token that is never signalled — the common case for these tests.
    fn live() -> CancellationToken {
        CancellationToken::new()
    }

    /// Occupancy must never exceed one for a single namespace. Each thread
    /// sleeps inside the lock, so without exclusion the overlap is not a
    /// matter of timing luck — every thread would be inside at once.
    #[test]
    fn same_namespace_runs_are_serialized() {
        let ns = Uuid::new_v4();
        let occupancy = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        thread::scope(|scope| {
            for _ in 0..8 {
                let occupancy = occupancy.clone();
                let peak = peak.clone();
                scope.spawn(move || {
                    with_namespace_lock(ns, &live(), || {
                        let inside = occupancy.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(inside, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        occupancy.fetch_sub(1, Ordering::SeqCst);
                    })
                    .expect("an uncancelled caller must acquire the slot");
                });
            }
        });

        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "two runs were inside one namespace's lock at once"
        );
    }

    /// Distinct namespaces must not serialize behind each other. Both threads
    /// rendezvous *inside* the lock: if the gate were global, neither could
    /// arrive while the other was held and both would time out.
    #[test]
    fn different_namespaces_run_concurrently() {
        let (ns_a, ns_b) = (Uuid::new_v4(), Uuid::new_v4());
        let arrived = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));

        let both_arrived = |gate: &Arc<(Mutex<usize>, std::sync::Condvar)>| {
            let (count, cv) = &**gate;
            let mut n = count.lock().unwrap();
            *n += 1;
            cv.notify_all();
            // Bounded so a regression fails with this assertion rather than
            // hanging the whole test binary until the harness gives up.
            let deadline = Duration::from_secs(10);
            while *n < 2 {
                let (next, timeout) = cv.wait_timeout(n, deadline).unwrap();
                n = next;
                if timeout.timed_out() {
                    break;
                }
            }
            *n >= 2
        };

        let results: Vec<bool> = thread::scope(|scope| {
            let handles: Vec<_> = [ns_a, ns_b]
                .into_iter()
                .map(|ns| {
                    let arrived = arrived.clone();
                    scope.spawn(move || {
                        with_namespace_lock(ns, &live(), || both_arrived(&arrived))
                            .expect("an uncancelled caller must acquire the slot")
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert!(
            results.iter().all(|&ok| ok),
            "runs on different namespaces were serialized against each other"
        );
    }

    /// A panicking run must release the namespace rather than wedge it.
    #[test]
    fn panic_does_not_poison_the_namespace() {
        let ns = Uuid::new_v4();

        let panicked = thread::spawn(move || {
            with_namespace_lock(ns, &live(), || panic!("consolidation blew up"));
        })
        .join();
        assert!(panicked.is_err(), "the panic should have propagated");

        // The namespace is still usable, and on a plain `Mutex` this is
        // exactly the call that would have returned `PoisonError` instead.
        assert_eq!(with_namespace_lock(ns, &live(), || 7), Some(7));
    }

    /// A caller signalled while queued behind a long run must give up rather
    /// than wait the run out — the ≤500 ms I5 cancellation budget has to hold
    /// under contention, not just on an idle namespace.
    #[test]
    fn cancellation_while_queued_gives_up_without_running() {
        let ns = Uuid::new_v4();
        let holder_may_exit = Arc::new(AtomicUsize::new(0));
        let cancel = CancellationToken::new();
        let waiter_ran = Arc::new(AtomicUsize::new(0));

        thread::scope(|scope| {
            // Occupy the namespace until told to let go.
            let holder_flag = holder_may_exit.clone();
            let holder = scope.spawn(move || {
                with_namespace_lock(ns, &live(), || {
                    while holder_flag.load(Ordering::SeqCst) == 0 {
                        thread::sleep(Duration::from_millis(5));
                    }
                })
            });

            // Let the holder take the slot before the waiter queues behind it.
            thread::sleep(Duration::from_millis(50));

            let waiter_cancel = cancel.clone();
            let ran = waiter_ran.clone();
            let waiter = scope.spawn(move || {
                with_namespace_lock(ns, &waiter_cancel, || {
                    ran.fetch_add(1, Ordering::SeqCst);
                })
            });

            thread::sleep(Duration::from_millis(50));
            let t0 = std::time::Instant::now();
            cancel.cancel();

            let outcome = waiter.join().unwrap();
            let responded_in = t0.elapsed();

            assert!(
                outcome.is_none(),
                "a cancelled waiter must not acquire the slot"
            );
            assert_eq!(
                waiter_ran.load(Ordering::SeqCst),
                0,
                "a cancelled waiter must not run the closure"
            );
            assert!(
                responded_in < Duration::from_millis(500),
                "cancel took {responded_in:?}, over the I5 budget"
            );

            holder_may_exit.store(1, Ordering::SeqCst);
            holder.join().unwrap().expect("holder acquired the slot");
        });
    }

    /// Idle namespaces must not accumulate entries for the life of the
    /// process — the registry tracks runs in flight, not runs ever seen.
    #[test]
    fn registry_does_not_retain_idle_namespaces() {
        let ns = Uuid::new_v4();
        with_namespace_lock(ns, &live(), || ()).expect("uncancelled");
        assert!(
            !lock_registry().contains_key(&ns),
            "a finished run left its lock behind"
        );
    }
}
