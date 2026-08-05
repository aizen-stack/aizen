//! Concurrent lanes — one independent worker per `(bot, chat)` conversation.
//!
//! # Why
//!
//! The daemon used to run ONE turn at a time for the whole process. Hosting several bots was
//! therefore cosmetic: a single `cargo test` in one chat froze every other bot and every other chat
//! behind it, for minutes. This module gives each conversation its own queue and its own task, so
//! two people talking to two bots — or the same person in two chats — make progress at once.
//!
//! # What must NOT become concurrent
//!
//! Three things are deliberately still serialized, because parallelising them would corrupt state
//! rather than speed anything up:
//!
//! 1. **One chat at a time.** Each `(route, chat)` has a single-consumer queue, so a conversation's
//!    messages are answered in order and its history is never written by two turns at once.
//!
//! 2. **One writer per workspace root.** [`RootGate`] serializes turns that share a directory. This
//!    is not a nicety: `WorkspaceWriterLease` tracks held leases in a PROCESS-GLOBAL map
//!    (`workspace_txn::HELD`) and treats a second acquisition of the same worktree as *reentrant*,
//!    handing it an empty lease. That is correct for a parent turn and its sub-agent (one writer as
//!    far as any other process can tell), but between two unrelated lanes it would silently remove
//!    the mutual exclusion and let both edit the same tree. The gate is what keeps that from being
//!    reachable — lanes on DIFFERENT roots still run fully in parallel.
//!
//! 3. **Memory learning.** The passive learner mutates one global store; lanes take a short lock
//!    around it rather than racing to write the same files.
//!
//! A global [`Semaphore`] additionally caps how many turns run at once, because each one can spawn a
//! compiler or a test suite — unbounded concurrency here spends the machine, not just the CPU.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinHandle;

use crate::hostbot::platform::Platform;

/// Default cap on turns running at once. Four is a compromise: enough that a slow chat doesn't block
/// three others, few enough that four concurrent `cargo build`s don't take the machine down. Override
/// with `AIZEN_SERVE_MAX_CONCURRENT`.
const DEFAULT_MAX_CONCURRENT: usize = 4;

/// How many messages a lane may have waiting before the daemon applies backpressure.
const LANE_QUEUE_DEPTH: usize = 32;

/// Read the concurrency cap from the environment, clamped to at least 1.
pub fn max_concurrent() -> usize {
    std::env::var("AIZEN_SERVE_MAX_CONCURRENT")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT)
}

/// One `tokio::sync::Mutex` per canonical workspace root, created on demand.
///
/// Turns that share a directory take the same mutex and therefore run one at a time; turns in
/// different directories never contend. See the module note on `HELD` for why this is required
/// rather than merely nice.
#[derive(Default)]
pub struct RootGate {
    gates: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl RootGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mutex guarding `root`. Canonicalized by the caller (`lane_cwd`) so two spellings of one
    /// directory map to one gate — the whole point would be lost if `./p` and `/abs/p` differed.
    pub fn gate_for(&self, root: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .entry(root.to_path_buf())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// How many distinct roots have been seen (used by tests to prove the gate keys on the path).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.gates.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Live counts a health probe reads: a daemon is "busy" while ANY lane is mid-turn, and the long
/// busy deadline must be judged against the OLDEST running turn, not the most recent one.
#[derive(Default)]
pub struct LaneStats {
    busy: AtomicUsize,
    lanes: AtomicUsize,
}

impl LaneStats {
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }
    fn enter_turn(&self) {
        self.busy.fetch_add(1, Ordering::SeqCst);
    }
    fn leave_turn(&self) {
        // `fetch_update` rather than `fetch_sub`: an underflow here would wrap to usize::MAX and
        // pin the daemon at "busy" forever, which a liveness probe would eventually restart.
        let _ = self
            .busy
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
    }
    fn add_lane(&self) {
        self.lanes.fetch_add(1, Ordering::SeqCst);
    }
    fn drop_lane(&self) {
        let _ = self
            .lanes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_sub(1))
            });
    }
}

/// RAII: increments the busy count for a turn and always decrements, including on panic.
struct BusyGuard(Arc<LaneStats>);
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.leave_turn();
    }
}

/// What every lane shares: the concurrency cap, the per-root gate, the learning lock, and the stats
/// a health probe reads.
pub struct LaneShared {
    pub sem: Arc<Semaphore>,
    pub roots: RootGate,
    pub stats: Arc<LaneStats>,
    /// Serializes the passive memory learner — one global store, many lanes.
    pub learn: tokio::sync::Mutex<()>,
}

impl LaneShared {
    pub fn new() -> Self {
        Self {
            sem: Arc::new(Semaphore::new(max_concurrent())),
            roots: RootGate::new(),
            stats: Arc::new(LaneStats::default()),
            learn: tokio::sync::Mutex::new(()),
        }
    }
}

impl Default for LaneShared {
    fn default() -> Self {
        Self::new()
    }
}

/// A running lane: the queue its messages go on, and the task draining it.
struct Lane {
    tx: mpsc::Sender<String>,
    task: JoinHandle<()>,
}

/// Every live conversation, keyed by `(route, chat)`.
///
/// `dispatch` is the only entry point: it finds or starts the right lane and hands the message over
/// WITHOUT waiting for the turn, which is what lets the daemon's router keep accepting messages
/// while long turns run.
pub struct LaneRegistry<P: Platform> {
    lanes: Mutex<HashMap<(String, P::Chat), Lane>>,
    pub shared: Arc<LaneShared>,
    /// Set once, immediately after construction — lane tasks need a handle to the registry (for
    /// `/rmbot`, which stops OTHER lanes). Weak so the registry can actually be dropped.
    self_ref: Mutex<Weak<LaneRegistry<P>>>,
}

impl<P: Platform> LaneRegistry<P> {
    pub fn new() -> Arc<Self> {
        let me = Arc::new(Self {
            lanes: Mutex::new(HashMap::new()),
            shared: Arc::new(LaneShared::new()),
            self_ref: Mutex::new(Weak::new()),
        });
        *me.self_ref.lock().unwrap() = Arc::downgrade(&me);
        me
    }

    fn weak(&self) -> Weak<LaneRegistry<P>> {
        self.self_ref
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Number of live lanes.
    pub fn len(&self) -> usize {
        self.lanes.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Stop every lane belonging to `route` (a removed bot) and forget them.
    pub fn stop_route(&self, route: &str) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<(String, P::Chat)> =
            lanes.keys().filter(|(r, _)| r == route).cloned().collect();
        for k in keys {
            if let Some(lane) = lanes.remove(&k) {
                lane.task.abort();
                self.shared.stats.drop_lane();
            }
        }
    }

    /// Stop every lane (daemon shutdown).
    pub fn stop_all(&self) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        for (_, lane) in lanes.drain() {
            lane.task.abort();
            self.shared.stats.drop_lane();
        }
    }

    /// Hand `text` to the `(route, chat)` lane, starting it if this is the conversation's first
    /// message. Returns without waiting for the turn.
    ///
    /// A full queue drops the message rather than blocking the router: blocking would stall EVERY
    /// bot to punish one flooding chat, which is the failure this whole module exists to remove.
    pub async fn dispatch(&self, ctx: LaneSpawn<P>, text: String) {
        let key = (ctx.route.clone(), ctx.chat);
        // Fast path: an existing, still-open lane.
        {
            let lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lane) = lanes.get(&key) {
                if !lane.tx.is_closed() {
                    let tx = lane.tx.clone();
                    drop(lanes);
                    if tx.try_send(text).is_err() {
                        eprintln!("[lane {}:{}] queue full — message dropped", key.0, key.1);
                    }
                    return;
                }
            }
        }
        // Start a lane. Re-check under the lock: two messages can arrive back to back.
        let (tx, rx) = mpsc::channel::<String>(LANE_QUEUE_DEPTH);
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lane) = lanes.get(&key) {
            if !lane.tx.is_closed() {
                let tx = lane.tx.clone();
                drop(lanes);
                let _ = tx.try_send(text);
                return;
            }
        }
        let task = (ctx.spawn)(rx, self.weak());
        lanes.insert(
            key,
            Lane {
                tx: tx.clone(),
                task,
            },
        );
        self.shared.stats.add_lane();
        drop(lanes);
        let _ = tx.try_send(text);
    }
}

/// Everything needed to START a lane, supplied by the daemon (which owns the platform, the endpoint,
/// and the turn body). Boxed as a closure so this module stays free of the daemon's dependencies.
pub struct LaneSpawn<P: Platform> {
    pub route: String,
    pub chat: P::Chat,
    #[allow(clippy::type_complexity)]
    pub spawn: Box<
        dyn FnOnce(mpsc::Receiver<String>, Weak<LaneRegistry<P>>) -> JoinHandle<()>
            + Send
            + 'static,
    >,
}

/// Run one turn under BOTH guards: a concurrency permit and this root's gate.
///
/// Order matters only for throughput, not safety — the permit is taken first so the number of turns
/// holding machine resources is bounded even while some of them queue on a shared directory.
pub async fn with_turn_guards<T>(
    shared: &LaneShared,
    root: &std::path::Path,
    f: impl std::future::Future<Output = T>,
) -> T {
    let _permit = shared
        .sem
        .clone()
        .acquire_owned()
        .await
        .expect("lane semaphore is never closed");
    let gate = shared.roots.gate_for(root);
    let _root = gate.lock().await;
    let _busy = BusyGuard(shared.stats.clone());
    shared.stats.enter_turn();
    f.await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    fn shared_with_cap(n: usize) -> LaneShared {
        LaneShared {
            sem: Arc::new(Semaphore::new(n)),
            roots: RootGate::new(),
            stats: Arc::new(LaneStats::default()),
            learn: tokio::sync::Mutex::new(()),
        }
    }

    #[test]
    fn the_same_root_maps_to_one_gate_and_a_different_root_to_another() {
        let g = RootGate::new();
        let a = std::path::Path::new("/srv/projA");
        let b = std::path::Path::new("/srv/projB");
        assert!(
            Arc::ptr_eq(&g.gate_for(a), &g.gate_for(a)),
            "same root, same gate"
        );
        assert!(
            !Arc::ptr_eq(&g.gate_for(a), &g.gate_for(b)),
            "different roots, different gates"
        );
        assert_eq!(g.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_lanes_on_different_roots_run_at_the_same_time() {
        // The point of the whole module: a long turn in one project must not block another project.
        // Proven with a barrier, not a sleep — both turns must be inside their bodies simultaneously
        // or the barrier never releases and the test times out.
        let shared = Arc::new(shared_with_cap(4));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let a = std::path::PathBuf::from("/srv/projA");
        let b = std::path::PathBuf::from("/srv/projB");

        let (s1, s2) = (shared.clone(), shared.clone());
        let (b1, b2) = (barrier.clone(), barrier.clone());
        let t1 = tokio::spawn(async move {
            with_turn_guards(&s1, &a, async move {
                b1.wait().await;
            })
            .await;
        });
        let t2 = tokio::spawn(async move {
            with_turn_guards(&s2, &b, async move {
                b2.wait().await;
            })
            .await;
        });

        let both = tokio::time::timeout(Duration::from_secs(5), async {
            t1.await.unwrap();
            t2.await.unwrap();
        })
        .await;
        assert!(
            both.is_ok(),
            "turns on different roots must overlap; they serialized instead"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_lanes_on_the_same_root_never_overlap() {
        // This is the data-loss guard. `WorkspaceWriterLease` keeps its held-lease map process-wide
        // and would treat the second lane's acquisition as reentrant — handing it an EMPTY lease and
        // letting both write the same tree. The gate is what makes that unreachable.
        let shared = Arc::new(shared_with_cap(4));
        let root = std::path::PathBuf::from("/srv/shared-project");
        let inside = Arc::new(AtomicUsize::new(0));
        let overlapped = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for _ in 0..4 {
            let (s, r) = (shared.clone(), root.clone());
            let (inside, overlapped) = (inside.clone(), overlapped.clone());
            handles.push(tokio::spawn(async move {
                with_turn_guards(&s, &r, async move {
                    if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlapped.store(true, Ordering::SeqCst);
                    }
                    // Yield across an await point: if the gate were missing, another task would be
                    // scheduled here and the counter would show it.
                    tokio::task::yield_now().await;
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "two turns shared one workspace root — the writer lease would not have protected them"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_semaphore_caps_how_many_turns_run_at_once() {
        // Unbounded concurrency here means N simultaneous compilers. The cap is a resource guard.
        let shared = Arc::new(shared_with_cap(2));
        let peak = Arc::new(AtomicUsize::new(0));
        let live = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..6 {
            let s = shared.clone();
            let root = std::path::PathBuf::from(format!("/srv/p{i}")); // distinct roots: only the cap binds
            let (peak, live) = (peak.clone(), live.clone());
            handles.push(tokio::spawn(async move {
                with_turn_guards(&s, &root, async move {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "cap of 2 exceeded: peak {}",
            peak.load(Ordering::SeqCst)
        );
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "with 6 tasks and a cap of 2, at least 2 should have overlapped"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_busy_count_returns_to_zero_after_every_turn() {
        // A leaked busy count would pin the health probe at "busy" and hide a genuinely wedged
        // daemon behind the long deadline.
        let shared = Arc::new(shared_with_cap(2));
        assert_eq!(shared.stats.busy(), 0);
        with_turn_guards(&shared, std::path::Path::new("/srv/p"), async {
            // observed from inside via the shared counter
        })
        .await;
        assert_eq!(shared.stats.busy(), 0, "guard decremented on the way out");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_panicking_turn_still_releases_its_busy_slot() {
        let shared = Arc::new(shared_with_cap(2));
        let s = shared.clone();
        let h = tokio::spawn(async move {
            with_turn_guards(&s, std::path::Path::new("/srv/p"), async {
                panic!("turn blew up");
            })
            .await;
        });
        assert!(h.await.is_err(), "the panic propagated to the join handle");
        assert_eq!(
            shared.stats.busy(),
            0,
            "the RAII guard released the slot despite the panic"
        );
    }

    #[test]
    fn the_concurrency_cap_is_env_overridable_and_never_zero() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AIZEN_SERVE_MAX_CONCURRENT");
        assert_eq!(max_concurrent(), DEFAULT_MAX_CONCURRENT);
        std::env::set_var("AIZEN_SERVE_MAX_CONCURRENT", "9");
        assert_eq!(max_concurrent(), 9);
        // A zero or garbage value must not disable the daemon entirely.
        std::env::set_var("AIZEN_SERVE_MAX_CONCURRENT", "0");
        assert_eq!(max_concurrent(), DEFAULT_MAX_CONCURRENT);
        std::env::set_var("AIZEN_SERVE_MAX_CONCURRENT", "not-a-number");
        assert_eq!(max_concurrent(), DEFAULT_MAX_CONCURRENT);
        std::env::remove_var("AIZEN_SERVE_MAX_CONCURRENT");
    }

    #[test]
    fn lane_stats_never_underflow() {
        // A stray decrement must not wrap to usize::MAX and read as "permanently busy".
        let s = LaneStats::default();
        s.leave_turn();
        assert_eq!(s.busy(), 0);
        s.drop_lane();
        assert_eq!(s.lanes.load(Ordering::SeqCst), 0);
    }
}
