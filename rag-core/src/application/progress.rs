//! Progress reporting for long operations. The use cases announce the phase they
//! are in (resolve, fetch, list, read, commit) through a `Progress` they are given;
//! the presentation layer decides what to do with it (print a line, keep a
//! heartbeat going) and measures time, which the application cannot (there is no
//! clock but the `Clock` port, which counts whole seconds).
//!
//! `end` returns the milliseconds the phase took as the implementation measured
//! them; the use cases put them into the JSON reports (`PhaseTiming`). The default
//! implementation does nothing and measures nothing, so tests and library users
//! need no progress at all.

use serde::Serialize;

use crate::application::error::AppError;

/// A step of an indexing operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Finding the default branch of the remote.
    Resolve,
    /// Cloning or fetching objects.
    Fetch,
    /// Listing the files of the commit.
    List,
    /// Reading blobs (one batch at a time) and chunking them.
    Read,
    /// Writing the index in one transaction.
    Commit,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Fetch => "fetch",
            Self::List => "list",
            Self::Read => "read",
            Self::Commit => "commit",
        }
    }
}

/// Receives the phases of an operation. Methods take `&self`; implementations keep
/// their state behind interior mutability (the port is `Send + Sync`, so a heartbeat
/// thread can share it).
pub trait Progress: Send + Sync {
    /// A phase starts. `detail` is a short human description such as `list 120
    /// files` or `read batch 2/5`; it is plain text without credentials.
    fn begin(&self, _phase: Phase, _detail: &str) {}

    /// Something worth saying about the running phase once it is known (`120 files`
    /// after the listing, say). Plain text without credentials.
    fn detail(&self, _phase: Phase, _text: &str) {}

    /// The phase ends. Returns how long it took in milliseconds; an implementation
    /// that does not measure returns 0.
    fn end(&self, _phase: Phase) -> u64 {
        0
    }

    /// Whether the wall-clock budget of the whole command (`--deadline`) has run
    /// out. The use cases ask between phases and before each batch of work, and stop
    /// with `AppError::DeadlineExceeded` instead of starting more. The default has no
    /// limit.
    fn deadline_passed(&self) -> bool {
        false
    }
}

/// `Err(AppError::DeadlineExceeded)` when the budget of the command is spent.
pub fn ensure_in_time(progress: &dyn Progress) -> Result<(), AppError> {
    if progress.deadline_passed() {
        Err(AppError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

/// Reports nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProgress;

impl Progress for NoProgress {}

/// The progress used when none is given.
pub static NO_PROGRESS: NoProgress = NoProgress;

/// How long one phase took, as it appears in the JSON reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PhaseTiming {
    pub phase: &'static str,
    pub elapsed_ms: u64,
}

/// The phases of one operation in the order they first began; a phase that runs
/// several times (read batches) is one entry with the summed time.
#[derive(Debug, Clone, Default)]
pub struct Timings(Vec<PhaseTiming>);

impl Timings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `elapsed_ms` to `phase`.
    pub fn add(&mut self, phase: Phase, elapsed_ms: u64) {
        match self.0.iter_mut().find(|t| t.phase == phase.as_str()) {
            Some(timing) => timing.elapsed_ms += elapsed_ms,
            None => self.0.push(PhaseTiming {
                phase: phase.as_str(),
                elapsed_ms,
            }),
        }
    }

    /// Announces `phase`, runs `work`, ends the phase, and records its time.
    pub fn run<T>(
        &mut self,
        progress: &dyn Progress,
        phase: Phase,
        detail: &str,
        work: impl FnOnce() -> T,
    ) -> T {
        progress.begin(phase, detail);
        let value = work();
        let elapsed = progress.end(phase);
        self.add(phase, elapsed);
        value
    }

    pub fn into_vec(self) -> Vec<PhaseTiming> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::{ensure_in_time, NoProgress, Phase, PhaseTiming, Progress, Timings, NO_PROGRESS};
    use crate::application::error::AppError;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
    }

    impl Progress for Recorder {
        fn begin(&self, phase: Phase, detail: &str) {
            self.events
                .lock()
                .unwrap()
                .push(format!("begin {} {detail}", phase.as_str()));
        }

        fn end(&self, phase: Phase) -> u64 {
            self.events
                .lock()
                .unwrap()
                .push(format!("end {}", phase.as_str()));
            7
        }
    }

    #[test]
    fn phases_have_stable_names() {
        let names: Vec<&str> = [
            Phase::Resolve,
            Phase::Fetch,
            Phase::List,
            Phase::Read,
            Phase::Commit,
        ]
        .iter()
        .map(Phase::as_str)
        .collect();
        assert_eq!(names, ["resolve", "fetch", "list", "read", "commit"]);
    }

    #[test]
    fn the_default_progress_does_nothing_and_measures_nothing() {
        NoProgress.begin(Phase::Fetch, "x");
        NoProgress.detail(Phase::Fetch, "y");
        assert_eq!(NoProgress.end(Phase::Fetch), 0);
        assert_eq!(NO_PROGRESS.end(Phase::List), 0);
    }

    #[test]
    fn without_a_deadline_there_is_always_time_left() {
        assert!(!NoProgress.deadline_passed());
        assert_eq!(ensure_in_time(&NoProgress), Ok(()));
    }

    #[test]
    fn a_passed_deadline_stops_the_operation_with_the_deadline_error() {
        struct Late;
        impl Progress for Late {
            fn deadline_passed(&self) -> bool {
                true
            }
        }
        assert_eq!(ensure_in_time(&Late), Err(AppError::DeadlineExceeded));
    }

    #[test]
    fn a_run_announces_the_phase_around_the_work_and_records_its_time() {
        let recorder = Recorder::default();
        let mut timings = Timings::new();
        let value = timings.run(&recorder, Phase::List, "list 3 files", || 42);
        assert_eq!(value, 42);
        assert_eq!(
            *recorder.events.lock().unwrap(),
            ["begin list list 3 files", "end list"]
        );
        assert_eq!(
            timings.into_vec(),
            [PhaseTiming {
                phase: "list",
                elapsed_ms: 7
            }]
        );
    }

    #[test]
    fn a_phase_that_runs_again_is_summed_in_its_first_position() {
        let mut timings = Timings::new();
        timings.add(Phase::Fetch, 5);
        timings.add(Phase::Read, 10);
        timings.add(Phase::Read, 20);
        timings.add(Phase::Commit, 1);
        let entries: Vec<(&str, u64)> = timings
            .into_vec()
            .iter()
            .map(|t| (t.phase, t.elapsed_ms))
            .collect();
        assert_eq!(entries, [("fetch", 5), ("read", 30), ("commit", 1)]);
    }

    #[test]
    fn timings_serialize_as_phase_and_elapsed_ms() {
        let mut timings = Timings::new();
        timings.add(Phase::Resolve, 12);
        assert_eq!(
            serde_json::to_string(&timings.into_vec()).unwrap(),
            r#"[{"phase":"resolve","elapsed_ms":12}]"#
        );
    }
}
