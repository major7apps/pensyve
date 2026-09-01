//! Per-namespace dispatch for consolidation runs.
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
//! `/consolidate` endpoint. Rather than ask each of them to remember to
//! coordinate, [`ConsolidationEngine::run`] dispatches through here, so the
//! guarantee cannot be bypassed — including by call sites added later.
//!
//! That holds outside this crate only because the module is `pub(crate)`. It
//! was `pub` through #258, which let any dependent claim a namespace here and
//! starve the engine, making a guarantee stated as unbypassable bypassable by
//! anyone who was not a call site (#260). Closing it costs the intra-doc
//! links `run`'s public docs used to point readers here — restricting the
//! module at all costs them, `#[doc(hidden)]` included, since rustdoc
//! generates no page for a hidden item either — so those references are now
//! plain code spans.
//!
//! [`ConsolidationEngine::run`]: super::ConsolidationEngine::run
//!
//! ## Coalescing, not queueing
//!
//! A trigger that arrives while a run is in flight does **not** wait. It marks
//! the namespace pending and returns immediately; the in-flight run picks the
//! flag up when it finishes and runs once more.
//!
//! Waiting was the obvious alternative and it is the wrong shape here. Every
//! caller reaches this code from `spawn_blocking`, so a waiting trigger would
//! hold a blocking-pool thread while doing nothing. That pool is shared with
//! embedding and reranker work, and the `episode_end` and `/consolidate` paths
//! submit one task per request with no admission control — so a burst on a
//! single namespace could exhaust the pool and degrade recall for the entire
//! gateway. Trading a duplicate-row race for a service-wide stall is not a
//! trade worth making. Coalescing bounds the cost at **one occupied thread per
//! namespace**, and a coalesced trigger releases its thread in microseconds.
//!
//! ## Why coalescing does not drop work
//!
//! Skipping a trigger outright *would* drop work: the promotion pass snapshots
//! the namespace at its start, so a run already in flight cannot see episodes
//! written after that snapshot, and a skipped trigger's evidence would sit
//! unconsolidated until some later trigger or the periodic sweep.
//!
//! The pending flag closes that hole for a complete owner: the owner re-runs
//! with a fresh snapshot before releasing. An incomplete owner deliberately
//! declines an immediate re-run; its durable checkpoint and the coalesced
//! caller's typed `CoalescedPending` outcome make resumption explicit, and a
//! later trigger or periodic sweep refreshes the source snapshot.
//!
//! Two consequences worth stating plainly. While triggers keep arriving during
//! each run, the owning caller keeps running — that is a namespace under
//! sustained write traffic genuinely needing sustained consolidation, and it
//! costs one thread. And `max_duration_secs` bounds a *run*, not a caller: an
//! owner that re-runs spends that budget once per run, so a caller which waits
//! for its own return value (the `/consolidate` endpoint) can take longer than
//! one budget under sustained triggering. Incomplete outcomes are the bounded
//! exception: they release the process claim and rely on durable resumption
//! instead of extending the same caller indefinitely.
//!
//! ## Scope
//!
//! This is process-local. It does not coordinate two processes sharing one
//! database file (a gateway and a CLI invocation, say); that would need a
//! guard in the storage layer. #226 is about overlapping in-process triggers,
//! which is what this closes.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use uuid::Uuid;

/// Namespaces with a run in flight. The value is the pending flag: `true`
/// means at least one further trigger arrived while the run was going. A
/// complete owner runs again before release; an incomplete owner releases for
/// later durable resumption.
///
/// Presence *is* the ownership claim, so an entry exists only while a run is
/// actually in flight — the map does not grow with the number of namespaces
/// the process has ever consolidated.
static REGISTRY: OnceLock<Mutex<HashMap<Uuid, bool>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<Uuid, bool>> {
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the registry lock, recovering rather than propagating poison.
///
/// A panic while the registry is held can only have interrupted a map lookup,
/// which leaves the map structurally intact. Propagating the poison would
/// wedge consolidation for every namespace in the process forever, so the
/// guard is recovered instead.
fn lock_registry() -> MutexGuard<'static, HashMap<Uuid, bool>> {
    registry().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Releases the namespace if the owner unwinds.
///
/// This is the only removal on the paths that reach it: a panic in `f`, and a
/// normal exit where `should_rerun` declined before [`finish_or_rerun`] ran.
/// Both leave the entry continuously present, so removing here cannot evict
/// anyone else's claim. Without it a panicking run would leave the namespace
/// permanently claimed, and every later trigger would coalesce forever against
/// an owner that no longer exists.
///
/// It must NOT fire on the path where [`finish_or_rerun`] already released the
/// claim — that removal happens under the registry lock, the lock is dropped
/// before this guard would run, and a fresh owner can claim in the gap.
/// Removing again there would evict *that* owner and let a third run start
/// alongside it, which is the concurrency #226 exists to prevent. `dispatch`
/// disarms the lease on that path.
struct Lease {
    namespace_id: Uuid,
    armed: bool,
}

impl Lease {
    fn new(namespace_id: Uuid) -> Self {
        Self {
            namespace_id,
            armed: true,
        }
    }

    /// Give up responsibility for releasing the namespace, because something
    /// else already has under the registry lock. See the type docs for why
    /// releasing twice is not harmless.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.armed {
            lock_registry().remove(&self.namespace_id);
        }
    }
}

/// Outcome of a [`dispatch`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch<T> {
    /// This caller owned the namespace and ran; carries the closure's result
    /// from its final run.
    Ran(T),
    /// A run was already in flight. Nothing executed here — the namespace was
    /// marked pending. A complete owner will run again; an incomplete owner
    /// returns typed pending state for later durable resumption.
    Coalesced,
}

/// Claim `namespace_id`, or mark it pending if a run already owns it.
///
/// Returns `true` when the caller now owns the namespace.
fn claim(namespace_id: Uuid) -> bool {
    match lock_registry().entry(namespace_id) {
        Entry::Occupied(mut occupied) => {
            *occupied.get_mut() = true;
            false
        }
        Entry::Vacant(vacant) => {
            vacant.insert(false);
            true
        }
    }
}

/// Decide whether the owner runs again, releasing the namespace if not.
///
/// The check and the release happen under one registry lock — see the module
/// docs on why that is what makes coalescing lossless.
fn finish_or_rerun(namespace_id: Uuid) -> bool {
    let mut reg = lock_registry();
    match reg.get_mut(&namespace_id) {
        Some(pending) if *pending => {
            *pending = false;
            true
        }
        _ => {
            reg.remove(&namespace_id);
            false
        }
    }
}

/// Run `f` for `namespace_id`, guaranteeing at most one run per namespace is
/// in flight at a time while leaving different namespaces free to run in
/// parallel.
///
/// If a run already owns the namespace, returns [`Dispatch::Coalesced`]
/// immediately without invoking `f`. Otherwise `f` runs, and a complete owner
/// re-runs while further triggers arrive. `should_rerun` lets an incomplete or
/// failed owner release for later resumption rather than promise an immediate
/// retry.
///
/// A panic in `f` propagates after the namespace is released, so it neither
/// wedges the namespace nor leaves an entry behind.
pub fn dispatch<T>(
    namespace_id: Uuid,
    mut f: impl FnMut() -> T,
    should_rerun: impl Fn(&T) -> bool,
) -> Dispatch<T> {
    if !claim(namespace_id) {
        return Dispatch::Coalesced;
    }
    let mut lease = Lease::new(namespace_id);

    let mut out = f();
    while should_rerun(&out) {
        if !finish_or_rerun(namespace_id) {
            // The claim is already released, atomically, under the registry
            // lock. That lock is dropped before `lease` would run, so a fresh
            // owner may hold this namespace by then — disarm, or we would
            // evict it and let a third run start alongside it (#226).
            lease.disarm();
            reclaim_in_release_window(namespace_id);
            return Dispatch::Ran(out);
        }
        out = f();
    }
    Dispatch::Ran(out)
}

// Thread-local rather than global: the whole test suite shares one registry,
// so a process-wide flag would inject claims into unrelated tests running in
// parallel. `dispatch` is synchronous, so the arming thread is the one that
// reaches the window.
#[cfg(test)]
thread_local! {
    static RECLAIM_IN_RELEASE_WINDOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam for the window between [`finish_or_rerun`]'s release and the
/// lease going out of scope. Compiled out of non-test builds.
///
/// The window is closed by construction in `dispatch`, so no caller-supplied
/// code runs inside it and a black-box test cannot reach it. This lets the
/// test place a fresh claim there deterministically instead of racing for it.
#[cfg(test)]
fn reclaim_in_release_window(namespace_id: Uuid) {
    if RECLAIM_IN_RELEASE_WINDOW.with(std::cell::Cell::get) {
        assert!(
            claim(namespace_id),
            "the namespace must be free once finish_or_rerun has released it"
        );
    }
}

#[cfg(not(test))]
#[inline]
fn reclaim_in_release_window(_namespace_id: Uuid) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;
    use std::time::Duration;

    /// Always honor a pending trigger.
    fn always<T>(_: &T) -> bool {
        true
    }

    /// Never re-run — for tests that only care about the ownership claim.
    fn never<T>(_: &T) -> bool {
        false
    }

    fn is_claimed(namespace_id: Uuid) -> bool {
        lock_registry().contains_key(&namespace_id)
    }

    /// Only one caller may be inside a namespace's run at a time. Each thread
    /// sleeps inside, so without the guarantee the overlap is not a matter of
    /// timing luck — every thread would be inside at once.
    #[test]
    fn same_namespace_never_runs_concurrently() {
        let ns = Uuid::new_v4();
        let occupancy = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        thread::scope(|scope| {
            for _ in 0..8 {
                let occupancy = occupancy.clone();
                let peak = peak.clone();
                scope.spawn(move || {
                    dispatch(
                        ns,
                        || {
                            let inside = occupancy.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(inside, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(20));
                            occupancy.fetch_sub(1, Ordering::SeqCst);
                        },
                        always,
                    )
                });
            }
        });

        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "two runs were inside one namespace at once"
        );
        assert!(!is_claimed(ns), "the namespace was left claimed");
    }

    /// Distinct namespaces must not serialize behind each other. Both threads
    /// rendezvous *inside* their run: if dispatch were global, neither could
    /// arrive while the other was running and both would time out.
    #[test]
    fn different_namespaces_run_concurrently() {
        let (ns_a, ns_b) = (Uuid::new_v4(), Uuid::new_v4());
        let arrived = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));

        let both_arrived = |gate: &Arc<(Mutex<usize>, std::sync::Condvar)>| {
            let (count, cv) = &**gate;
            let mut n = count.lock().unwrap();
            *n += 1;
            cv.notify_all();
            // Bounded so a regression fails with the assertion below rather
            // than hanging the test binary until the harness gives up.
            while *n < 2 {
                let (next, timeout) = cv.wait_timeout(n, Duration::from_secs(10)).unwrap();
                n = next;
                if timeout.timed_out() {
                    break;
                }
            }
            *n >= 2
        };

        let results: Vec<Dispatch<bool>> = thread::scope(|scope| {
            let handles: Vec<_> = [ns_a, ns_b]
                .into_iter()
                .map(|ns| {
                    let arrived = arrived.clone();
                    scope.spawn(move || dispatch(ns, || both_arrived(&arrived), never))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(
            results,
            vec![Dispatch::Ran(true), Dispatch::Ran(true)],
            "runs on different namespaces were serialized against each other"
        );
    }

    /// A trigger that arrives mid-run must be covered by a further run rather
    /// than dropped. This is the property that makes coalescing safe: the
    /// promotion pass snapshots the namespace at its start, so a coalesced
    /// trigger's evidence is only seen by a run that begins after it arrived.
    #[test]
    fn a_coalesced_trigger_causes_another_run() {
        let ns = Uuid::new_v4();
        let runs = Arc::new(AtomicUsize::new(0));
        let coalesced = Arc::new(AtomicUsize::new(0));

        let owner_runs = runs.clone();
        let owner_coalesced = coalesced.clone();
        dispatch(
            ns,
            || {
                // Only the first run invites a competing trigger, so the loop
                // is expected to settle at exactly two runs.
                if owner_runs.fetch_add(1, Ordering::SeqCst) == 0 {
                    let c = owner_coalesced.clone();
                    thread::scope(|scope| {
                        scope.spawn(move || {
                            if dispatch(ns, || (), always) == Dispatch::Coalesced {
                                c.fetch_add(1, Ordering::SeqCst);
                            }
                        });
                    });
                }
            },
            always,
        );

        assert_eq!(
            coalesced.load(Ordering::SeqCst),
            1,
            "the competing trigger should have coalesced"
        );
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "the coalesced trigger should have caused exactly one further run"
        );
        assert!(!is_claimed(ns), "the namespace was left claimed");
    }

    /// Declining a re-run must still release the namespace, pending flag and
    /// all — otherwise an error or a cancellation would strand it.
    #[test]
    fn declining_a_rerun_still_releases_the_namespace() {
        let ns = Uuid::new_v4();

        let outcome = dispatch(
            ns,
            || {
                // A trigger lands mid-run and is coalesced.
                thread::scope(|scope| {
                    scope.spawn(|| dispatch(ns, || (), always));
                });
            },
            never,
        );

        assert_eq!(outcome, Dispatch::Ran(()));
        assert!(!is_claimed(ns), "declining a re-run stranded the namespace");
    }

    /// A panicking run must release the namespace rather than wedge it. Left
    /// claimed, every later trigger would coalesce forever against an owner
    /// that no longer exists.
    #[test]
    fn panic_releases_the_namespace() {
        let ns = Uuid::new_v4();

        let panicked = thread::spawn(move || {
            dispatch(ns, || panic!("consolidation blew up"), always::<()>);
        })
        .join();
        assert!(panicked.is_err(), "the panic should have propagated");

        assert!(
            !is_claimed(ns),
            "a panicking run left the namespace claimed"
        );
        // And the namespace is still usable.
        assert_eq!(dispatch(ns, || 7, never), Dispatch::Ran(7));
        assert!(!is_claimed(ns));
    }

    /// Idle namespaces must not accumulate entries for the life of the
    /// process — the registry tracks runs in flight, not runs ever seen.
    #[test]
    fn registry_does_not_retain_idle_namespaces() {
        let ns = Uuid::new_v4();
        assert_eq!(dispatch(ns, || (), never), Dispatch::Ran(()));
        assert!(!is_claimed(ns), "a finished run left its entry behind");
    }

    /// `finish_or_rerun` releases under the registry lock and then drops it,
    /// so a fresh owner can claim before the outgoing run's lease would run.
    /// The outgoing run must not remove that owner's entry: doing so frees a
    /// namespace someone is actively running in, and the next trigger starts a
    /// second concurrent run — the #226 race this gate exists to close.
    ///
    /// No caller-supplied code executes inside that window, so the test seam
    /// places the competing claim there rather than racing for it.
    #[test]
    fn a_claim_taken_in_the_release_window_is_not_evicted() {
        let ns = Uuid::new_v4();

        RECLAIM_IN_RELEASE_WINDOW.with(|f| f.set(true));
        // `always` sends the run into `finish_or_rerun`; with no trigger
        // pending it releases and returns false, which is the path that used
        // to remove a second time.
        let outcome = dispatch(ns, || (), always);
        RECLAIM_IN_RELEASE_WINDOW.with(|f| f.set(false));

        assert_eq!(outcome, Dispatch::Ran(()));
        assert!(
            is_claimed(ns),
            "the outgoing run evicted a claim it did not own, leaving the \
             namespace free while another run holds it"
        );

        // That simulated owner never runs, so release its claim by hand.
        assert!(!finish_or_rerun(ns));
        assert!(!is_claimed(ns));
    }
}
