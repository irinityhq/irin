//! Phase 2 file-inbox-watch sentinel.
//!
//! Default backend on macOS: PollWatcher (1s cadence) — P0-4 demo-path fix.
//! Native (FSEvents/kqueue) is opt-in via a future config.watcher_backend
//! field, deferred until v0.2.
//!
//! APFS clonefile note (Grok G4): on macOS, `cp -c` or Finder copy emits
//! spurious modify events on the source file under native FSEvents.
//! PollWatcher is unaffected; native requires debounce to coalesce. Default
//! to poll until that hardening lands.
//!
//! Lifecycle: `start_watching()` constructs and starts a PollWatcher that
//! pushes Create events into a debounce buffer. After `debounce` elapses,
//! every settled path enters a bounded FIFO queue that `observe()` consumes.

use crate::watch::{
    EscalateError, Escalation, ObserveError, Sentinel, SentinelState, Tier, Urgency,
};
use async_trait::async_trait;
use notify::{Config, Event, EventKind, PollWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub struct FileInboxSentinel {
    name: String,
    tenant: String,
    path: PathBuf,
    patterns: Vec<String>,
    debounce: Duration,
    cooldown: Duration,
    /// path -> first-seen-event-time, used to deduplicate burst events
    /// during a single debounce window.
    pending: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    /// Paths that have settled past the debounce window, in arrival order.
    /// Bounded to 64 entries; `observe()` consumes one path per tick.
    settled: Arc<Mutex<VecDeque<PathBuf>>>,
    /// Most recent watcher-backend error, consumed by `observe()` so the
    /// runner records a sentinel failure instead of a healthy idle tick.
    last_error: Arc<Mutex<Option<String>>>,
    /// Channel for sending deduplicated paths from the sync watcher callback
    /// into the async debounce task on the watch-rt (fixes unbounded OS thread
    /// spawn + blocking sleep violation).
    debounce_tx: Option<UnboundedSender<PathBuf>>,
    /// Receiver held (under lock for one-time take) so the debouncer task can
    /// be spawned on the provided runtime handle (or fallback thread in tests).
    debounce_rx: Arc<Mutex<Option<UnboundedReceiver<PathBuf>>>>,
    /// Structural liveness flag for the debouncer task (liveness regression / grok
    /// finding #1). `false` until `start_watching` spawns the debouncer, then
    /// `true`; a `DebouncerLiveness` Drop-guard flips it back to `false` if that
    /// task ever ends — by clean channel-close OR panic. observe() reads it to
    /// tell a genuinely dead watcher (must fail loudly) from a healthy idle
    /// inbox (benign). NOT time-based — a cadence proxy would be a heartbeat,
    /// forbidden by the no-cadence-as-signal guard.
    alive: Arc<AtomicBool>,
}

/// Drop-guard that flips the debouncer liveness flag to `false` when the
/// debouncer task ends — covering both a clean `rx` close and a panic (Drop
/// runs during unwind). Held inside the spawned task; see `alive` field.
struct DebouncerLiveness(Arc<AtomicBool>);
impl Drop for DebouncerLiveness {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Redact a filesystem path for logging: emit only the file extension and a
/// stable short hash — never the filename or directory layout. Inbox filenames
/// are attacker/tenant-controlled and can carry customer, document, or
/// directory-layout names, and debug/trace breadcrumbs can reach a
/// retained JSON log sink. The hash lets an operator correlate the same file
/// across the event -> settle -> observe -> match trail without exposing names;
/// it is non-cryptographic (DefaultHasher), used only as an opaque label.
fn redact_path(p: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.to_string_lossy().hash(&mut h);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("noext");
    format!("{:016x}.{ext}", h.finish())
}

const MAX_SETTLED_FILES: usize = 64;

fn push_settled(settled: &Mutex<VecDeque<PathBuf>>, path: PathBuf) {
    let mut settled = settled.lock();
    if settled.len() >= MAX_SETTLED_FILES {
        settled.pop_front();
    }
    settled.push_back(path);
}

fn handle_watcher_result(
    result: notify::Result<Event>,
    pending: &Mutex<HashMap<PathBuf, Instant>>,
    tx: &UnboundedSender<PathBuf>,
    last_error: &Mutex<Option<String>>,
    name: &str,
) {
    match result {
        Ok(event) => {
            if matches!(event.kind, EventKind::Create(_)) {
                for path in event.paths {
                    let mut pending = pending.lock();
                    if !pending.contains_key(&path) {
                        pending.insert(path.clone(), Instant::now());
                        drop(pending);
                        match tx.send(path.clone()) {
                            Ok(()) => tracing::trace!(
                                sentinel = %name,
                                file = %redact_path(&path),
                                "file-inbox: Create event enqueued for debounce"
                            ),
                            Err(_) => tracing::debug!(
                                sentinel = %name,
                                file = %redact_path(&path),
                                "file-inbox: Create event dropped — debouncer receiver closed"
                            ),
                        }
                    }
                }
            }
        }
        Err(error) => {
            let message = error.to_string();
            tracing::warn!(
                sentinel = %name,
                error = %message,
                "file-inbox: watcher backend error"
            );
            *last_error.lock() = Some(message);
        }
    }
}

impl FileInboxSentinel {
    pub fn new(
        name: &str,
        tenant: &str,
        path: &Path,
        patterns: Vec<String>,
        debounce: Duration,
    ) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            name: name.into(),
            tenant: tenant.into(),
            path: path.to_path_buf(),
            patterns,
            debounce,
            cooldown: Duration::from_secs(5),
            pending: Arc::new(Mutex::new(HashMap::new())),
            settled: Arc::new(Mutex::new(VecDeque::new())),
            last_error: Arc::new(Mutex::new(None)),
            debounce_tx: Some(tx),
            debounce_rx: Arc::new(Mutex::new(Some(rx))),
            // Not alive until start_watching spawns the debouncer. A sentinel
            // whose watcher never started is therefore reported failed by
            // observe(), not masked as healthy idle.
            alive: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Override the default 5s cooldown — used by the yaml registry loader.
    pub fn with_cooldown(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    /// Boot-time path validation (P0-4 demo fix): fail fast if path missing
    /// or unreadable, so the operator sees a clear error instead of a
    /// silently-quarantining sentinel.
    pub fn validate_path(&self) -> anyhow::Result<()> {
        if !self.path.exists() {
            anyhow::bail!(
                "file-inbox-watch path '{}' missing or unreadable. \
                 Expected docker-compose bind mount './demo/inbox:/var/lib/gateway/inbox:ro'. \
                 Cold-boot Phase 2 demo cannot start without this path.",
                self.path.display()
            );
        }
        std::fs::read_dir(&self.path).map_err(|e| {
            anyhow::anyhow!(
                "file-inbox-watch path '{}' exists but cannot read_dir: {}",
                self.path.display(),
                e
            )
        })?;
        Ok(())
    }

    /// Start a PollWatcher on the configured path. Returns the watcher
    /// handle; the caller MUST hold onto it for the watcher to keep
    /// running (drop = stop). For v0.1 we deliberately leak via
    /// `std::mem::forget` inside the runner spawn-site so the watcher
    /// lives for process lifetime; here the watcher is returned so
    /// tests can decide.
    ///
    /// v0.2 isolation: the debounce is now driven by an async mpsc from the
    /// sync callback into a task on the provided watch-rt handle (or fallback
    /// bounded thread for tests/off-rt). This eliminates the previous
    /// unbounded std::thread::spawn + blocking sleep per burst event.
    pub fn start_watching(&self, handle: Option<&Handle>) -> notify::Result<PollWatcher> {
        let pending = self.pending.clone();
        let last_error = self.last_error.clone();
        let tx = self
            .debounce_tx
            .clone()
            .expect("debounce channel not initialized in new()");
        let cb_name = self.name.clone(); // trace context for the watcher callback

        let cfg = Config::default().with_poll_interval(Duration::from_secs(1));
        let mut watcher = PollWatcher::new(
            move |result| {
                handle_watcher_result(result, &pending, &tx, &last_error, &cb_name);
            },
            cfg,
        )?;
        watcher.watch(&self.path, RecursiveMode::NonRecursive)?;

        // Consume the single-shot debounce receiver. A second start_watching()
        // would find it already taken — that must be a loud Err, not a silent Ok
        // that hands back a running PollWatcher while NO debouncer is armed (which
        // would leave the sentinel permanently reporting Fatal).         // unreachable today (one call site, fresh sentinel), but the API now
        // refuses the restart footgun. Taking it AFTER watcher creation means a
        // PollWatcher `?`-failure above never consumes the receiver.
        let rx = self.debounce_rx.lock().take().ok_or_else(|| {
            notify::Error::generic(
                "start_watching called more than once: debounce receiver already consumed",
            )
        })?;

        let p2 = self.pending.clone();
        let settled = self.settled.clone();
        let d2 = self.debounce;
        let name = self.name.clone(); // trace context for the debouncer task
        let alive = self.alive.clone();
        // Publish liveness, then move the guard — built on THIS stack — into the
        // debouncer. Building it on the spawner stack (not inside the async block
        // / closure) means an unpolled-future drop (rt shutdown/cancel) or a
        // thread::spawn panic after the store still runs Drop -> alive=false,
        // closing the creation-time TOCTOU that would re-open the false-healthy
        // window.
        self.alive.store(true, Ordering::SeqCst);
        let live = DebouncerLiveness(alive);

        if let Some(h) = handle {
            let mut rx = rx;
            h.spawn(async move {
                let _live = live;
                while let Some(p) = rx.recv().await {
                    tokio::time::sleep(d2).await;
                    p2.lock().remove(&p);
                    tracing::trace!(
                        sentinel = %name,
                        file = %redact_path(&p),
                        "file-inbox: settled past debounce -> queue"
                    );
                    push_settled(&settled, p);
                }
            });
        } else {
            // Fallback for tests and any off-rt call sites: one bounded thread per
            // sentinel drives the debounce (prevents the per-event spawn explosion).
            let mut rx = rx;
            std::thread::spawn(move || {
                let _live = live;
                while let Some(p) = rx.blocking_recv() {
                    std::thread::sleep(d2);
                    p2.lock().remove(&p);
                    tracing::trace!(
                        sentinel = %name,
                        file = %redact_path(&p),
                        "file-inbox: settled past debounce -> queue"
                    );
                    push_settled(&settled, p);
                }
            });
        }

        Ok(watcher)
    }
}

#[async_trait]
impl Sentinel for FileInboxSentinel {
    fn name(&self) -> &str {
        &self.name
    }
    fn tenant(&self) -> &str {
        &self.tenant
    }
    fn tier(&self) -> Tier {
        Tier::Polling
    }
    fn cooldown(&self) -> Duration {
        self.cooldown
    }

    async fn observe(&self) -> Result<SentinelState, ObserveError> {
        if let Some(error) = self.last_error.lock().take() {
            return Err(ObserveError::TransientUpstream(error));
        }
        let path = self.settled.lock().pop_front();
        match path {
            None => {
                // A genuinely dead watcher (debouncer task panicked / channel
                // closed / never started) leaves the settled queue empty forever.
                // That is a real failure and must be loud (liveness regression / grok
                // finding #1): report it so the runner record_failure-quarantines
                // THIS sentinel — without it, dead is indistinguishable from idle.
                // Residual: covers debouncer-task death, not notify's internal
                // poll-thread dying while the debouncer parks alive (undetectable
                // without a forbidden cadence proxy).
                if !self.alive.load(Ordering::SeqCst) {
                    tracing::debug!(
                        sentinel = %self.name,
                        tenant = %self.tenant,
                        "file-inbox: debouncer not alive -> Fatal (dead or never started)"
                    );
                    return Err(ObserveError::Fatal(
                        "file-inbox debouncer is not running (watcher dead or never started)"
                            .into(),
                    ));
                }
                // Alive but no pending file = the normal resting state of a
                // poll-driven sentinel, NOT a failure. Returning Err here routed
                // every quiet tick to ObserveErr -> record_failure -> self-
                // quarantine between fires (first-fire regression). Emit a path-less state
                // so `interesting()` returns None -> Uninteresting ->
                // record_success, the same healthy idle path a non-matching file
                // already takes.
                let observed_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                tracing::trace!(
                    sentinel = %self.name,
                    tenant = %self.tenant,
                    "file-inbox: idle tick (debouncer alive) -> Uninteresting"
                );
                Ok(SentinelState {
                    tenant: self.tenant.clone(),
                    sentinel: self.name.clone(),
                    observed_at,
                    payload: serde_json::json!({}),
                })
            }
            Some(p) => {
                let metadata = tokio::fs::metadata(&p)
                    .await
                    .map_err(|e| ObserveError::TransientUpstream(e.to_string()))?;
                let observed_at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                tracing::debug!(
                    sentinel = %self.name,
                    tenant = %self.tenant,
                    file = %redact_path(&p),
                    size = metadata.len(),
                    "file-inbox: observed pending file"
                );
                Ok(SentinelState {
                    tenant: self.tenant.clone(),
                    sentinel: self.name.clone(),
                    observed_at,
                    payload: serde_json::json!({
                        "path": p.to_string_lossy(),
                        "size": metadata.len(),
                    }),
                })
            }
        }
    }

    fn interesting(&self, state: &SentinelState) -> Option<String> {
        let path = state.payload["path"].as_str()?;
        let filename = Path::new(path).file_name()?.to_str()?;
        // Match against globs in self.patterns. Simple suffix match for v0.1
        // (full glob crate is overkill until config supports `**/*.pdf`).
        let matches = self.patterns.iter().any(|pat| {
            if let Some(suffix) = pat.strip_prefix("*.") {
                filename.ends_with(&format!(".{suffix}"))
            } else {
                filename == pat
            }
        });
        if matches {
            tracing::debug!(
                sentinel = %self.name,
                file = %redact_path(Path::new(path)),
                "file-inbox: pattern match -> interesting"
            );
            Some(format!("new file: {filename}"))
        } else {
            tracing::trace!(
                sentinel = %self.name,
                file = %redact_path(Path::new(path)),
                "file-inbox: no pattern match -> uninteresting"
            );
            None
        }
    }

    async fn escalate(
        &self,
        state: SentinelState,
        reason: String,
    ) -> Result<Escalation, EscalateError> {
        // Urgency convention: filenames prefixed `urgent-` or `urgent_`
        // escalate at High, otherwise Medium. Lets the operator drop a
        // priority file by name without a config change.
        let urgency = state
            .payload
            .get("path")
            .and_then(|v| v.as_str())
            .and_then(|s| Path::new(s).file_name())
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("urgent-") || n.starts_with("urgent_"))
            .map(|is_urgent| {
                if is_urgent {
                    Urgency::High
                } else {
                    Urgency::Medium
                }
            })
            .unwrap_or(Urgency::Medium);
        // Redacted file token only — `reason` is "new file: {filename}" and would
        // re-expose the inbox filename in logs.
        let file = state
            .payload
            .get("path")
            .and_then(|v| v.as_str())
            .map(|s| redact_path(Path::new(s)))
            .unwrap_or_else(|| "-".into());
        tracing::debug!(
            sentinel = %self.name,
            tenant = %self.tenant,
            urgency = ?urgency,
            file = %file,
            "file-inbox: escalating"
        );
        Ok(Escalation {
            state,
            reason,
            urgency,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentinel(dir: &Path) -> FileInboxSentinel {
        FileInboxSentinel::new(
            "file-inbox-test",
            "canary",
            dir,
            vec!["*.txt".to_string()],
            Duration::from_millis(0),
        )
    }

    // Regression — first-live-fire first-fire regression. An idle inbox with a LIVE
    // debouncer is the normal resting state of a poll-driven sentinel, NOT a
    // failure. observe() must return an empty Uninteresting state so the runner
    // records success; returning Err here routed every quiet tick to ObserveErr
    // -> record_failure -> self-quarantine between fires.
    #[tokio::test]
    async fn idle_inbox_alive_is_uninteresting_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path());
        s.alive.store(true, Ordering::SeqCst); // simulate a live debouncer
        let state = s.observe().await.expect("idle observe must be Ok, not Err");
        assert!(
            state.payload.get("path").is_none(),
            "idle state carries no path"
        );
        assert_eq!(s.interesting(&state), None, "idle tick is Uninteresting");
    }

    // A dead or never-started
    // debouncer (alive == false) leaves the settled queue empty forever; that is a
    // real failure and observe() MUST report it (Fatal) so the runner
    // record_failure-quarantines the dead sentinel, rather than masking it as
    // healthy idle. This is the structural liveness signal that prevents a
    // silent dead-watcher blind spot.
    #[tokio::test]
    async fn dead_or_unstarted_watcher_idle_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path()); // alive defaults to false (never started)
        let err = s
            .observe()
            .await
            .expect_err("dead/unstarted watcher must fail, not report healthy idle");
        assert!(
            matches!(err, ObserveError::Fatal(_)),
            "dead watcher is Fatal, got {err:?}"
        );
    }

    // Integration — start_watching spawns the debouncer and flips alive true,
    // after which an idle inbox is benign again. Proves the real wiring, not a
    // hand-set flag.
    #[tokio::test]
    async fn started_watcher_sets_alive_and_idle_is_uninteresting() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path());
        assert!(!s.alive.load(Ordering::SeqCst), "not alive before start");
        // Keep the watcher (drop == stop) for the duration of the test.
        let _watcher = s
            .start_watching(Some(&Handle::current()))
            .expect("start_watching ok on a valid dir");
        assert!(s.alive.load(Ordering::SeqCst), "alive after start");
        let state = s.observe().await.expect("idle observe Ok once alive");
        assert_eq!(s.interesting(&state), None);
    }

    #[tokio::test]
    async fn watcher_error_is_reported_once_then_idle() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path());
        s.alive.store(true, Ordering::SeqCst);

        handle_watcher_result(
            Err(notify::Error::generic("permission changed")),
            &s.pending,
            s.debounce_tx.as_ref().unwrap(),
            &s.last_error,
            &s.name,
        );

        let err = s
            .observe()
            .await
            .expect_err("watcher callback error must fail the next tick");
        assert!(
            matches!(err, ObserveError::TransientUpstream(ref message) if message.contains("permission changed")),
            "got {err:?}"
        );

        let state = s
            .observe()
            .await
            .expect("reported watcher error is consumed once");
        assert_eq!(s.interesting(&state), None);
    }

    // log breadcrumbs must not expose inbox filenames or directory
    // layout. redact_path emits only an opaque hash + extension, and is stable
    // for the same path so an operator can still correlate the event trail.
    #[test]
    fn redact_path_hides_name_and_dir_keeps_ext_and_is_stable() {
        let p = Path::new("/var/lib/gateway/inbox/acme-corp-invoice.pdf");
        let r = redact_path(p);
        assert!(!r.contains("acme"), "no customer name: {r}");
        assert!(!r.contains("invoice"), "no document name: {r}");
        assert!(
            !r.contains("inbox") && !r.contains("gateway"),
            "no dir: {r}"
        );
        assert!(r.ends_with(".pdf"), "extension preserved: {r}");
        assert_eq!(r, redact_path(p), "stable for cross-breadcrumb correlation");
        assert_ne!(
            r,
            redact_path(Path::new("/var/lib/gateway/inbox/other.pdf")),
            "different path -> different token"
        );
    }

    // Regression — liveness regression. When the
    // debouncer task actually DIES (all senders dropped -> rx closes -> loop
    // ends), the Drop-guard must flip alive=false and a subsequent idle observe()
    // must report Fatal. Proves the Drop side of the structural signal — the part
    // that, left in the async block, re-opened the false-healthy window.
    #[tokio::test]
    async fn debouncer_death_flips_alive_and_observe_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = sentinel(dir.path());
        let watcher = s
            .start_watching(Some(&Handle::current()))
            .expect("start_watching ok");
        assert!(s.alive.load(Ordering::SeqCst), "alive after start");
        // Close the debounce channel: drop the field sender AND the watcher,
        // which holds the only other sender clone inside its callback.
        s.debounce_tx = None;
        drop(watcher);
        // Let the debouncer observe the closed channel, exit, and run its guard.
        let mut flipped = false;
        for _ in 0..100 {
            if !s.alive.load(Ordering::SeqCst) {
                flipped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(flipped, "alive must flip false once the debouncer dies");
        let err = s
            .observe()
            .await
            .expect_err("dead debouncer -> observe Fatal");
        assert!(matches!(err, ObserveError::Fatal(_)), "got {err:?}");
    }

    // The std::thread fallback path (start_watching(None)) also arms liveness.
    #[tokio::test]
    async fn std_thread_fallback_sets_alive() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path());
        let _watcher = s.start_watching(None).expect("start ok (thread fallback)");
        assert!(s.alive.load(Ordering::SeqCst), "thread path sets alive");
    }

    // a second start_watching() must Err loudly (the single-shot
    // receiver is gone), not silently return Ok with no debouncer armed.
    #[tokio::test]
    async fn start_watching_twice_is_err_not_silent_ok() {
        let dir = tempfile::tempdir().unwrap();
        let s = sentinel(dir.path());
        let _w1 = s
            .start_watching(Some(&Handle::current()))
            .expect("first start ok");
        assert!(s.alive.load(Ordering::SeqCst));
        let second = s.start_watching(Some(&Handle::current()));
        assert!(
            second.is_err(),
            "second start_watching must Err, not silent Ok"
        );
        // The first debouncer is still running, so liveness stays true.
        assert!(s.alive.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn two_files_settled_in_one_window_are_observed_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let s = sentinel(dir.path());
        let _watcher = s
            .start_watching(Some(&Handle::current()))
            .expect("start watcher");
        let tx = s.debounce_tx.as_ref().unwrap();
        tx.send(first.clone()).unwrap();
        tx.send(second.clone()).unwrap();

        for _ in 0..100 {
            if s.settled.lock().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(s.settled.lock().len(), 2, "both files must settle");

        let first_state = s.observe().await.expect("observe first settled file");
        let second_state = s.observe().await.expect("observe second settled file");
        assert_eq!(
            first_state.payload["path"].as_str(),
            Some(first.to_string_lossy().as_ref())
        );
        assert_eq!(
            second_state.payload["path"].as_str(),
            Some(second.to_string_lossy().as_ref())
        );
    }

    // Happy path intact: a pending matching file yields a path-bearing state
    // that interesting() flags for escalation.
    #[tokio::test]
    async fn pending_matching_file_is_interesting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("firstfire.txt");
        std::fs::write(&file, b"x").unwrap();
        let s = sentinel(dir.path());
        s.settled.lock().push_back(file.clone());
        let state = s.observe().await.expect("observe Ok");
        assert_eq!(
            state.payload["path"].as_str(),
            Some(file.to_string_lossy().as_ref())
        );
        assert_eq!(
            s.interesting(&state),
            Some("new file: firstfire.txt".to_string())
        );
    }

    // A pending file that does not match the configured pattern is observed but
    // Uninteresting — same benign outcome as idle, never a failure.
    #[tokio::test]
    async fn pending_nonmatching_file_is_uninteresting() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ignore.bin");
        std::fs::write(&file, b"x").unwrap();
        let s = sentinel(dir.path());
        s.settled.lock().push_back(file);
        let state = s.observe().await.expect("observe Ok");
        assert_eq!(s.interesting(&state), None);
    }
}
