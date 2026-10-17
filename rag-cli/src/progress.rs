//! The progress lines on stderr. Indexing can take minutes; a caller that sees
//! nothing cannot tell a working `rag` from a stuck one. `StderrProgress` receives
//! the phases the use cases announce (`Progress`), prints one line when a phase
//! starts or says something, prints a heartbeat line when a phase has been silent
//! for a while, and measures how long each phase took.
//!
//! Every line is credential-scrubbed and stripped of control characters on the way
//! out (`Scrubbed`): lines quote locations and branch names that come from the
//! caller and from the remote.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rag_core::application::error::Scrubbed;
use rag_core::application::progress::{Phase, Progress};
use rag_infra::deadline::Deadline;

/// How long a phase may be silent before a heartbeat line says it is still running.
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// Where finished lines go. The real sink writes to stderr and ignores errors.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

struct Active {
    phase: Phase,
    detail: String,
    started: Instant,
}

struct State {
    active: Option<Active>,
    last_line: Instant,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    sink: Sink,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while printing must not stop the progress of the other threads.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, state: &mut State, line: &str) {
        (self.sink)(Scrubbed::new(line).as_str());
        state.last_line = Instant::now();
    }
}

/// `Progress` that prints to a sink and keeps a heartbeat going while a phase runs.
pub struct StderrProgress {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    deadline: Deadline,
}

impl StderrProgress {
    pub fn new(sink: Sink, heartbeat: Duration) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                active: None,
                last_line: Instant::now(),
                stop: false,
            }),
            wake: Condvar::new(),
            sink,
        });
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || beat(&shared, heartbeat))
        };
        Self {
            shared,
            worker: Some(worker),
            deadline: Deadline::none(),
        }
    }

    /// Tells the use cases when the overall budget of the command is spent.
    pub fn with_deadline(mut self, deadline: Deadline) -> Self {
        self.deadline = deadline;
        self
    }

    /// Prints a line of its own (scrubbed like every other).
    pub fn note(&self, line: &str) {
        let mut state = self.shared.lock();
        self.shared.emit(&mut state, line);
    }
}

/// The heartbeat thread: wakes every `heartbeat`, and says so when a phase has been
/// running without a line for that long.
fn beat(shared: &Shared, heartbeat: Duration) {
    let mut state = shared.lock();
    while !state.stop {
        let (next, _) = shared
            .wake
            .wait_timeout(state, heartbeat)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state = next;
        if state.stop {
            break;
        }
        if state.last_line.elapsed() < heartbeat {
            continue;
        }
        let Some(active) = &state.active else {
            continue;
        };
        let line = format!(
            "{}: still working, {} s so far ({})",
            active.phase.as_str(),
            active.started.elapsed().as_secs(),
            active.detail
        );
        shared.emit(&mut state, &line);
    }
}

impl Progress for StderrProgress {
    fn begin(&self, phase: Phase, detail: &str) {
        let mut state = self.shared.lock();
        state.active = Some(Active {
            phase,
            detail: detail.to_owned(),
            started: Instant::now(),
        });
        self.shared
            .emit(&mut state, &format!("{}: {detail}", phase.as_str()));
    }

    fn detail(&self, phase: Phase, text: &str) {
        let mut state = self.shared.lock();
        if let Some(active) = &mut state.active {
            active.detail = text.to_owned();
        }
        self.shared
            .emit(&mut state, &format!("{}: {text}", phase.as_str()));
    }

    fn end(&self, _phase: Phase) -> u64 {
        let mut state = self.shared.lock();
        let elapsed = state
            .active
            .take()
            .map_or(0, |active| active.started.elapsed().as_millis());
        u64::try_from(elapsed).unwrap_or(u64::MAX)
    }

    fn deadline_passed(&self) -> bool {
        self.deadline.passed()
    }
}

impl Drop for StderrProgress {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.wake.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rag_core::application::progress::{Phase, Progress};

    use rag_infra::deadline::Deadline;

    use super::{Sink, StderrProgress};

    type Lines = Arc<Mutex<Vec<String>>>;

    fn progress(heartbeat: Duration) -> (StderrProgress, Lines) {
        let lines = Lines::default();
        let sink: Sink = {
            let lines = Arc::clone(&lines);
            Arc::new(move |line: &str| lines.lock().unwrap().push(line.to_owned()))
        };
        (StderrProgress::new(sink, heartbeat), lines)
    }

    fn quiet() -> Duration {
        Duration::from_secs(3600)
    }

    #[test]
    fn a_phase_prints_its_name_and_its_detail() {
        let (progress, lines) = progress(quiet());
        progress.begin(Phase::List, "listing files");
        progress.detail(Phase::List, "120 files");
        progress.begin(Phase::Read, "reading batch 2/5");
        assert_eq!(
            *lines.lock().unwrap(),
            [
                "list: listing files",
                "list: 120 files",
                "read: reading batch 2/5"
            ]
        );
    }

    #[test]
    fn a_phase_reports_how_long_it_ran() {
        let (progress, _lines) = progress(quiet());
        progress.begin(Phase::Fetch, "fetching main");
        std::thread::sleep(Duration::from_millis(30));
        assert!(progress.end(Phase::Fetch) >= 25);
        assert_eq!(progress.end(Phase::Fetch), 0, "nothing runs any more");
    }

    #[test]
    fn credentials_never_reach_the_sink() {
        let (progress, lines) = progress(quiet());
        progress.note("indexing https://user:tok3n@example.com/o/r.git");
        progress.begin(
            Phase::Fetch,
            "fetching https://user:tok3n@example.com/o/r.git",
        );
        for line in lines.lock().unwrap().iter() {
            assert!(!line.contains("tok3n"), "{line}");
        }
        assert_eq!(lines.lock().unwrap().len(), 2);
    }

    #[test]
    fn control_characters_never_reach_the_sink() {
        let (progress, lines) = progress(quiet());
        progress.note("fetching \u{1b}[2Jmain\u{202e}\nnext line");
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\u{1b}') && !lines[0].contains('\u{202e}'));
        assert!(!lines[0].contains('\n'));
    }

    #[test]
    fn a_long_silent_phase_prints_a_heartbeat_with_its_detail() {
        let (progress, lines) = progress(Duration::from_millis(20));
        progress.begin(Phase::Fetch, "fetching main");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("fetch: still working"))
        {
            assert!(Instant::now() < deadline, "no heartbeat: {lines:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
        let heartbeat = lines
            .lock()
            .unwrap()
            .iter()
            .find(|line| line.starts_with("fetch: still working"))
            .cloned()
            .unwrap();
        assert!(heartbeat.contains("fetching main"), "{heartbeat}");
    }

    #[test]
    fn no_heartbeat_is_printed_while_no_phase_runs() {
        let (progress, lines) = progress(Duration::from_millis(10));
        progress.begin(Phase::Fetch, "fetching main");
        progress.end(Phase::Fetch);
        let before = lines.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(lines.lock().unwrap().len(), before);
    }

    #[test]
    fn the_deadline_is_passed_only_once_its_time_is_up() {
        let (progress, _lines) = progress(quiet());
        assert!(!progress.deadline_passed(), "no deadline, no limit");
        let progress = progress.with_deadline(Deadline::after(Duration::from_secs(3600)));
        assert!(!progress.deadline_passed());
        let progress = progress.with_deadline(Deadline::after(Duration::ZERO));
        assert!(progress.deadline_passed());
    }

    #[test]
    fn dropping_stops_the_heartbeat_thread_promptly() {
        let (progress, _lines) = progress(quiet());
        let started = Instant::now();
        drop(progress);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
