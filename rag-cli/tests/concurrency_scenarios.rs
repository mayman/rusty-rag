//! Concurrency, crash and git-integration scenarios against the real `rag` binary
//! and real `git`. Every test has its own `RUSTY_RAG_HOME` in a temp directory.
//!
//! The scenarios are numbered as in the system-design review list. Tests wait for
//! markers (phase lines on stderr, `rag list` statuses) rather than for fixed times.
//! Where a timing window cannot be closed, the test accepts both outcomes only when
//! both are legitimate, and the comment at that test says so.
//!
//! The tests of this file run one at a time (`serial`): each one starts several
//! processes that index in a debug build, and a loaded machine would otherwise turn
//! the 20 second lock wait into a flaky limit.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rag_core::application::ports::{OperationLock, SourceRepository};
use rag_infra::lock::FileOperationLock;
use rag_infra::sqlite::SqliteStore;

use serde_json::Value;
use tempfile::TempDir;

// --- helpers ------------------------------------------------------------------------

static SERIAL: Mutex<()> = Mutex::new(());

/// Test output that shows without --nocapture; the lint set bans the print macros.
macro_rules! note {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

/// Runs the test alone, and prints how long it took when it ends.
struct Scenario {
    name: &'static str,
    started: Instant,
    _turn: MutexGuard<'static, ()>,
}

fn scenario(name: &'static str) -> Scenario {
    let turn = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Scenario {
        name,
        started: Instant::now(),
        _turn: turn,
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        note!("[timing] {}: {:.2?}", self.name, self.started.elapsed());
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn git(dir: &Path, args: &[&str]) {
    let output = git_command(dir, args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        text(&output.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir, args).output().expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        text(&output.stderr)
    );
    text(&output.stdout).trim().to_owned()
}

/// A throwaway upstream repository on disk.
struct Upstream {
    _temp: TempDir,
    path: PathBuf,
}

impl Upstream {
    fn empty() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("project");
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        Self { _temp: temp, path }
    }

    fn write(&self, name: &str, content: &str) {
        self.write_bytes(name, content.as_bytes());
    }

    fn write_bytes(&self, name: &str, content: &[u8]) {
        let file = self.path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    fn commit(&self, message: &str) {
        git(&self.path, &["add", "-A"]);
        git(&self.path, &["commit", "-q", "-m", message]);
    }

    /// Records an entry in the next commit without touching the work tree, so entries
    /// Windows cannot hold (very long paths) still get into the repository.
    fn stage_entry(&self, mode: &str, name: &str, content: &str) {
        let scratch = self.path.join(".blob-scratch");
        std::fs::write(&scratch, content).unwrap();
        let oid = git_stdout(&self.path, &["hash-object", "-w", ".blob-scratch"]);
        std::fs::remove_file(&scratch).unwrap();
        self.stage_cacheinfo(mode, &oid, name);
    }

    fn stage_cacheinfo(&self, mode: &str, oid: &str, name: &str) {
        git(
            &self.path,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("{mode},{oid},{name}"),
            ],
        );
    }

    fn commit_staged(&self, message: &str) {
        git(&self.path, &["commit", "-q", "-m", message]);
    }

    fn head(&self) -> String {
        git_stdout(&self.path, &["rev-parse", "HEAD"])
    }

    fn head_short(&self) -> String {
        self.head()[..7].to_owned()
    }

    fn location(&self) -> &str {
        self.path.to_str().unwrap()
    }

    /// `files` files that all say `generation<generation>` on their first line, with
    /// filler so that indexing them takes a while. Committed.
    fn stamped(files: usize, generation: usize) -> Self {
        let upstream = Self::empty();
        upstream.stamp(files, generation);
        upstream.write("src/marker.rs", "pub fn recoverymarker() {}\n");
        upstream.commit(&format!("generation {generation}"));
        upstream
    }

    /// Rewrites every stamped file for `generation` and commits.
    fn restamp(&self, files: usize, generation: usize) {
        self.stamp(files, generation);
        self.commit(&format!("generation {generation}"));
    }

    fn stamp(&self, files: usize, generation: usize) {
        for n in 0..files {
            let mut body = format!("pub fn stampfn_{n}() {{ /* generation{generation} */ }}\n");
            for i in 0..30 {
                body.push_str(&format!("pub fn filler_{n}_{i}_{generation}() {{}}\n"));
            }
            self.write(&format!("src/module{n}.rs"), &body);
        }
    }
}

/// One `RUSTY_RAG_HOME`.
struct Rag {
    home: TempDir,
}

impl Rag {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("temp dir"),
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.home.path().join("data")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rag"));
        command
            .env("RUSTY_RAG_HOME", self.data_dir())
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    /// Runs `args` to the end: exit code and stdout parsed as exactly one JSON value.
    fn call(&self, args: &[&str]) -> (i32, Value) {
        let output = self.command().args(args).output().expect("rag starts");
        let code = output
            .status
            .code()
            .unwrap_or_else(|| panic!("rag {args:?} was killed: {}", text(&output.stderr)));
        let value = parse_json(&output.stdout, args, &text(&output.stderr));
        (code, value)
    }

    fn json(&self, args: &[&str], code: i32) -> Value {
        let (got, value) = self.call(args);
        assert_eq!(got, code, "rag {args:?}: {value}");
        value
    }

    fn add(&self, upstream: &Upstream) -> Value {
        self.json(&["add", upstream.location()], 0)
    }

    fn name_of(&self, added: &Value) -> String {
        let _ = self;
        added["name"].as_str().unwrap().to_owned()
    }

    fn spawn(&self, args: &[&str]) -> Running {
        Running::spawn(self.command().args(args))
    }

    fn statuses(&self) -> Vec<String> {
        self.json(&["list"], 0)["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["status"].as_str().unwrap().to_owned())
            .collect()
    }

    fn search(&self, query: &str) -> Value {
        self.json(&["search", query, "-k", "20"], 0)
    }

    fn tmp_entries(&self) -> Vec<String> {
        std::fs::read_dir(self.data_dir().join("tmp"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The operation lock of the only registered source, held by this test process.
    fn hold_lock_of_only_source(&self) -> rag_core::application::ports::LockGuard {
        let store = SqliteStore::open_for_read(&self.data_dir()).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 1);
        FileOperationLock::new(self.data_dir().join("locks"))
            .acquire(sources[0].id())
            .expect("the lock is free")
    }
}

fn parse_json(stdout: &[u8], args: &[&str], stderr: &str) -> Value {
    let stdout = text(stdout);
    assert_eq!(
        stdout.trim_end().lines().count(),
        1,
        "rag {args:?}: stdout is one JSON line: {stdout:?}\nstderr: {stderr}"
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("rag {args:?}: stdout is not JSON ({error}): {stdout:?}"))
}

fn hit_paths(response: &Value) -> Vec<String> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["path"].as_str().unwrap().to_owned())
        .collect()
}

/// A `rag` process in the background, with its stderr lines available as they come.
struct Running {
    child: Child,
    lines: Receiver<String>,
    seen: Arc<Mutex<Vec<String>>>,
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<()>,
}

impl Running {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rag starts");
        let mut out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        let stdout = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = out.read_to_end(&mut bytes);
            bytes
        });
        let (sender, lines) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stderr = {
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    seen.lock().unwrap().push(line.clone());
                    let _ = sender.send(line);
                }
            })
        };
        Self {
            child,
            lines,
            seen,
            stdout,
            stderr,
        }
    }

    /// Waits until a stderr line starts with `prefix`; false if the process ended (or
    /// 120 s passed) without printing one.
    fn wait_for_line(&self, prefix: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            match self.lines.recv_timeout(left) {
                Ok(line)
                    if line
                        .strip_prefix("rag: ")
                        .unwrap_or(&line)
                        .starts_with(prefix) =>
                {
                    return true
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// kill -9 (TerminateProcess on Windows). The pipes are left to their threads:
    /// a git child the process orphaned may keep them open for a moment.
    fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Waits for the end: exit code, stdout as one JSON value, all of stderr.
    fn finish(mut self, args: &[&str]) -> (i32, Value, String) {
        let status = self.child.wait().unwrap();
        let stdout = self.stdout.join().unwrap();
        self.stderr.join().unwrap();
        let stderr = self.seen.lock().unwrap().join("\n");
        let code = status.code().expect("rag exited by itself");
        (code, parse_json(&stdout, args, &stderr), stderr)
    }

    fn has_exited(&mut self) -> bool {
        self.child.try_wait().unwrap().is_some()
    }
}

/// What one search process answered.
struct Reply {
    code: i32,
    body: Value,
}

/// Runs `rag search <query>` in a loop until `stop`, counting finished searches in
/// `done`.
fn search_loop(rag: &Rag, query: &str, stop: &AtomicBool, done: &AtomicUsize) -> Vec<Reply> {
    let mut replies = Vec::new();
    loop {
        let (code, body) = rag.call(&["search", query, "-k", "20"]);
        replies.push(Reply { code, body });
        done.fetch_add(1, Ordering::SeqCst);
        if stop.load(Ordering::SeqCst) {
            return replies;
        }
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Which generations a successful search response shows, asserting that every source
/// in it carries one commit that belongs to that generation.
fn generations_in(reply: &Reply, commits: &[(usize, String)]) -> Vec<usize> {
    assert_eq!(reply.code, 0, "{}", reply.body);
    let mut seen = Vec::new();
    for hit in reply.body["results"].as_array().unwrap() {
        let snippet = hit["snippet"].as_str().unwrap();
        let generation = commits
            .iter()
            .map(|(generation, _)| *generation)
            .find(|generation| snippet.contains(&format!("generation{generation}")))
            .unwrap_or_else(|| panic!("no known generation in {snippet:?}"));
        if !seen.contains(&generation) {
            seen.push(generation);
        }
    }
    assert!(
        seen.len() <= 1,
        "one response mixes generations {seen:?}: {}",
        reply.body
    );
    if let Some(generation) = seen.first() {
        let expected = &commits.iter().find(|(g, _)| g == generation).unwrap().1;
        for (slug, info) in reply.body["sources"].as_object().unwrap() {
            assert_eq!(
                info["commit"].as_str().unwrap(),
                expected,
                "{slug}: the commit does not belong to generation {generation}"
            );
        }
    }
    seen
}

const FILES: usize = 300;

// --- 1, 2, 3: readers while a writer works ----------------------------------------------

#[test]
fn search_during_an_update_never_fails_and_never_mixes_commits() {
    let _s = scenario("1 search during update");
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let first = upstream.head_short();
    rag.add(&upstream);
    upstream.restamp(FILES, 2);
    let second = upstream.head_short();
    let commits = vec![(1, first), (2, second)];

    let stop = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let replies = std::thread::scope(|scope| {
        let searcher = scope.spawn(|| search_loop(&rag, "stampfn", &stop, &done));
        wait_until("three searches before the update", || {
            done.load(Ordering::SeqCst) >= 3
        });
        let updated = rag.json(&["update"], 0);
        assert_eq!(updated["sources"][0]["result"], "updated");
        let after = done.load(Ordering::SeqCst);
        wait_until("three searches after the update", || {
            done.load(Ordering::SeqCst) >= after + 3
        });
        stop.store(true, Ordering::SeqCst);
        searcher.join().unwrap()
    });

    let mut per_generation = [0usize; 3];
    for reply in &replies {
        for generation in generations_in(reply, &commits) {
            per_generation[generation] += 1;
        }
    }
    note!(
        "searches: {} (generation 1: {}, generation 2: {})",
        replies.len(),
        per_generation[1],
        per_generation[2]
    );
    assert!(per_generation[2] > 0, "the new commit became searchable");
    let last = rag.search("stampfn");
    assert_eq!(last["total_matches"], FILES);
    assert!(last["results"][0]["snippet"]
        .as_str()
        .unwrap()
        .contains("generation2"));
}

#[test]
fn search_during_a_first_add_exits_five_or_answers_validly_and_never_crashes() {
    let _s = scenario("2 search during first add");
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let commits = vec![(1, upstream.head_short())];

    let stop = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let replies = std::thread::scope(|scope| {
        let searcher = scope.spawn(|| search_loop(&rag, "stampfn", &stop, &done));
        wait_until("a search before the add", || {
            done.load(Ordering::SeqCst) >= 1
        });
        let added = rag.add(&upstream);
        assert_eq!(added["status"], "ready");
        let after = done.load(Ordering::SeqCst);
        wait_until("searches after the add", || {
            done.load(Ordering::SeqCst) >= after + 2
        });
        stop.store(true, Ordering::SeqCst);
        searcher.join().unwrap()
    });

    let mut exit_five = 0;
    for reply in &replies {
        match reply.code {
            // IndexMissing: nothing searchable yet. Legitimate before the commit.
            5 => {
                exit_five += 1;
                assert!(reply.body["error"]["code"].is_string(), "{}", reply.body);
            }
            // A valid answer: empty, or the complete first commit; never a partial one.
            0 => {
                let generations = generations_in(reply, &commits);
                if !generations.is_empty() {
                    assert_eq!(reply.body["total_matches"], FILES, "{}", reply.body);
                }
            }
            other => panic!("search exited {other} during the first add: {}", reply.body),
        }
    }
    note!("searches: {}, exit 5: {exit_five}", replies.len());
    assert_eq!(rag.search("stampfn")["total_matches"], FILES);
}

#[test]
fn search_during_a_remove_answers_validly_or_finds_nothing() {
    let _s = scenario("3 search during remove");
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let commits = vec![(1, upstream.head_short())];
    let name = rag.name_of(&rag.add(&upstream));

    let stop = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let replies = std::thread::scope(|scope| {
        let searcher = scope.spawn(|| search_loop(&rag, "stampfn", &stop, &done));
        wait_until("three searches before the remove", || {
            done.load(Ordering::SeqCst) >= 3
        });
        rag.json(&["remove", &name], 0);
        let after = done.load(Ordering::SeqCst);
        wait_until("searches after the remove", || {
            done.load(Ordering::SeqCst) >= after + 2
        });
        stop.store(true, Ordering::SeqCst);
        searcher.join().unwrap()
    });

    for reply in &replies {
        match reply.code {
            // The source is gone: no searchable source (5), or an empty answer.
            5 => assert!(reply.body["error"]["code"].is_string(), "{}", reply.body),
            0 => {
                let generations = generations_in(reply, &commits);
                if !generations.is_empty() {
                    assert_eq!(
                        reply.body["total_matches"], FILES,
                        "complete or empty: {}",
                        reply.body
                    );
                }
            }
            other => panic!("search exited {other} during the remove: {}", reply.body),
        }
    }
    let (code, body) = rag.call(&["search", "stampfn"]);
    assert!(
        code == 5 || (code == 0 && body["returned"] == 0),
        "after the remove: {code} {body}"
    );
    assert_eq!(rag.json(&["list"], 0)["sources"], serde_json::json!([]));
}

// --- 4, 5, 6, 7: several writers ---------------------------------------------------------

#[test]
fn four_processes_adding_the_same_repository_end_with_one_source() {
    let _s = scenario("4 four adds of one repo");
    let rag = Rag::new();
    let upstream = Upstream::stamped(100, 1);
    let args = ["add", upstream.location()];
    let running: Vec<Running> = (0..4).map(|_| rag.spawn(&args)).collect();

    let mut already = 0;
    for process in running {
        let (code, response, stderr) = process.finish(&args);
        assert_eq!(code, 0, "{response}\n{stderr}");
        assert_eq!(response["status"], "ready");
        if response["already_added"] == true {
            already += 1;
        }
    }
    assert_eq!(already, 3, "one process added, three found it added");
    assert_eq!(
        rag.json(&["list"], 0)["sources"].as_array().unwrap().len(),
        1
    );
    assert_eq!(rag.search("stampfn")["total_matches"], 100);
    assert!(rag.tmp_entries().iter().all(|n| !n.starts_with("clone-")));
}

#[test]
fn two_different_repositories_added_at_once_both_succeed() {
    let _s = scenario("5 two different adds");
    let rag = Rag::new();
    let one = Upstream::stamped(100, 1);
    let two = Upstream::empty();
    two.write("src/other.rs", "pub fn otherrepofunction() {}\n");
    two.commit("other");
    let a1 = ["add", one.location()];
    let a2 = ["add", two.location()];
    let first = rag.spawn(&a1);
    let second = rag.spawn(&a2);

    for (process, args) in [(first, a1), (second, a2)] {
        let (code, response, stderr) = process.finish(&args);
        assert_eq!(code, 0, "{response}\n{stderr}");
        assert_eq!(response["status"], "ready");
        assert_eq!(response["already_added"], false);
    }
    assert_eq!(rag.statuses(), ["ready", "ready"]);
    assert_eq!(rag.search("stampfn")["total_matches"], 100);
    assert_eq!(
        hit_paths(&rag.search("otherrepofunction")),
        ["src/other.rs"]
    );
}

#[test]
fn update_all_while_a_remove_runs_reports_the_removed_source_as_skipped_or_absent() {
    let _s = scenario("6 update all during remove");
    let rag = Rag::new();
    let keep = Upstream::stamped(FILES, 1);
    let doomed = Upstream::stamped(FILES, 1);
    doomed.write("src/doomed.rs", "pub fn doomedfunction() {}\n");
    doomed.commit("doomed");
    let keep_name = rag.name_of(&rag.add(&keep));
    let doomed_name = rag.name_of(&rag.add(&doomed));
    keep.restamp(FILES, 2);
    doomed.restamp(FILES, 2);

    let update_args = ["update"];
    let remove_args = ["remove", doomed_name.as_str()];
    let update = rag.spawn(&update_args);
    assert!(update.wait_for_line("fetch:"), "the update started");
    let remove = rag.spawn(&remove_args);
    let (update_code, updated, update_err) = update.finish(&update_args);
    let (remove_code, removed, remove_err) = remove.finish(&remove_args);

    // `remove` does not wait for a lock: while the update held the source it answers
    // `index_locked` (exit 6) and changes nothing. Both outcomes are legitimate; in the
    // second the remove is repeated once the update is done.
    match remove_code {
        0 => {}
        6 => {
            assert_eq!(removed["error"]["code"], "index_locked", "{removed}");
            rag.json(&remove_args, 0);
        }
        other => panic!("remove exited {other}: {removed}\n{remove_err}"),
    }
    note!("remove exited {remove_code} while the update ran");
    assert_eq!(update_code, 0, "{updated}\n{update_err}");
    assert_eq!(updated["failed"], 0, "{updated}");
    for entry in updated["sources"].as_array().unwrap() {
        let result = entry["result"].as_str().unwrap();
        if entry["name"] == keep_name.as_str() {
            assert!(["updated", "unchanged"].contains(&result), "{entry}");
        } else {
            // Updated before the remove took the lock, or found removed afterwards.
            assert!(
                ["updated", "skipped_removed", "skipped_busy"].contains(&result),
                "{entry}"
            );
        }
    }
    let listed = rag.json(&["list"], 0);
    let names: Vec<&str> = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, [keep_name.as_str()]);
    assert!(hit_paths(&rag.json(&["search", "doomedfunction"], 0)).is_empty());
    assert_eq!(rag.search("stampfn")["total_matches"], FILES);
}

#[test]
fn two_update_all_at_once_do_not_fail_and_leave_a_consistent_searchable_state() {
    let _s = scenario("7 two update all");
    let rag = Rag::new();
    let one = Upstream::stamped(150, 1);
    let two = Upstream::stamped(150, 1);
    two.write("src/second.rs", "pub fn secondrepofunction() {}\n");
    two.commit("second");
    rag.add(&one);
    rag.add(&two);
    one.restamp(150, 2);
    two.restamp(150, 2);

    for flags in [&["update"][..], &["update", "--no-wait"][..]] {
        let first = rag.spawn(flags);
        let second = rag.spawn(flags);
        for process in [first, second] {
            let (code, response, stderr) = process.finish(flags);
            assert_eq!(code, 0, "{flags:?}: {response}\n{stderr}");
            assert_eq!(response["failed"], 0, "{flags:?}: {response}");
            for entry in response["sources"].as_array().unwrap() {
                let result = entry["result"].as_str().unwrap();
                assert!(
                    ["updated", "unchanged", "skipped_busy"].contains(&result),
                    "{flags:?}: {entry}"
                );
            }
        }
        // Whoever held each source's lock first did the update; nothing is half done.
        assert_eq!(rag.statuses(), ["ready", "ready"]);
        let found = rag.search("stampfn");
        let generations: Vec<bool> = found["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["snippet"].as_str().unwrap().contains("generation2"))
            .collect();
        assert!(generations.iter().all(|new| *new), "{found}");
        assert_eq!(
            hit_paths(&rag.search("secondrepofunction")),
            ["src/second.rs"]
        );
        // The next round starts from generation 1 again.
        one.restamp(150, 1);
        two.restamp(150, 1);
        rag.json(&["update"], 0);
        one.restamp(150, 2);
        two.restamp(150, 2);
    }
}

// --- 8, 9, 12: kill -9 -------------------------------------------------------------------

const STATUSES: [&str; 4] = ["cloning", "indexing", "ready", "failed"];

fn killed_add_can_be_added_again(phase: &str) {
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let args = ["add", upstream.location()];
    let process = rag.spawn(&args);
    assert!(process.wait_for_line(phase), "{phase} was printed");
    process.kill();

    // The registry is readable and sane.
    for status in rag.statuses() {
        assert!(STATUSES.contains(&status.as_str()), "status {status}");
    }
    // The lock died with the process: nothing waits and nothing is refused.
    let added = rag.json(&["add", upstream.location(), "--no-wait"], 0);
    assert_eq!(added["status"], "ready", "{added}");
    assert_eq!(rag.statuses(), ["ready"]);
    assert_eq!(
        hit_paths(&rag.json(&["search", "recoverymarker"], 0)),
        ["src/marker.rs"]
    );
    assert_eq!(rag.search("stampfn")["total_matches"], FILES);
    let leftovers: Vec<String> = rag
        .tmp_entries()
        .into_iter()
        .filter(|name| name.starts_with("clone-") || name.starts_with("trash-"))
        .collect();
    assert!(leftovers.is_empty(), "tmp/ leftovers: {leftovers:?}");
}

#[test]
fn an_add_killed_at_the_fetch_phase_can_be_added_again() {
    let _s = scenario("8 kill add at fetch");
    killed_add_can_be_added_again("fetch:");
}

#[test]
fn an_add_killed_at_the_list_phase_can_be_added_again() {
    let _s = scenario("8 kill add at list");
    killed_add_can_be_added_again("list:");
}

#[test]
fn an_add_killed_at_the_read_phase_can_be_added_again() {
    let _s = scenario("8 kill add at read");
    killed_add_can_be_added_again("read:");
}

#[test]
fn an_add_killed_at_the_commit_phase_can_be_added_again() {
    let _s = scenario("8 kill add at commit");
    killed_add_can_be_added_again("commit:");
}

fn killed_update_keeps_serving_the_old_index(phase: &str) {
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let first = upstream.head_short();
    rag.add(&upstream);
    upstream.restamp(FILES, 2);
    let second = upstream.head_short();
    let commits = vec![(1, first), (2, second)];

    let process = rag.spawn(&["update"]);
    assert!(process.wait_for_line(phase), "{phase} was printed");
    process.kill();

    // Old or new, never a mix, and always searchable. (A kill at the commit phase may
    // land after the commit; both are legitimate.)
    let found = rag.call(&["search", "stampfn", "-k", "20"]);
    let reply = Reply {
        code: found.0,
        body: found.1,
    };
    let generations = generations_in(&reply, &commits);
    assert_eq!(generations.len(), 1, "{}", reply.body);
    assert_eq!(reply.body["total_matches"], FILES);
    for status in rag.statuses() {
        assert!(STATUSES.contains(&status.as_str()), "status {status}");
    }

    // The lock is free and the update can be repeated.
    let updated = rag.json(&["update", "--no-wait"], 0);
    assert!(
        ["updated", "unchanged"].contains(&updated["sources"][0]["result"].as_str().unwrap()),
        "{updated}"
    );
    let last = rag.search("stampfn");
    assert_eq!(last["total_matches"], FILES);
    assert!(last["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|hit| hit["snippet"].as_str().unwrap().contains("generation2")));
    assert_eq!(rag.statuses(), ["ready"]);
}

#[test]
fn an_update_killed_at_the_fetch_phase_still_serves_the_old_index() {
    let _s = scenario("9 kill update at fetch");
    killed_update_keeps_serving_the_old_index("fetch:");
}

#[test]
fn an_update_killed_at_the_list_phase_still_serves_the_old_index() {
    let _s = scenario("9 kill update at list");
    killed_update_keeps_serving_the_old_index("list:");
}

#[test]
fn an_update_killed_at_the_read_phase_still_serves_the_old_index() {
    let _s = scenario("9 kill update at read");
    killed_update_keeps_serving_the_old_index("read:");
}

#[test]
fn an_update_killed_at_the_commit_phase_still_serves_a_whole_index() {
    let _s = scenario("9 kill update at commit");
    killed_update_keeps_serving_the_old_index("commit:");
}

#[test]
fn the_lock_of_a_killed_process_is_released_by_the_operating_system() {
    let _s = scenario("12 lock released after kill");
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let name = rag.name_of(&rag.add(&upstream));
    upstream.restamp(FILES, 2);

    let process = rag.spawn(&["update", &name]);
    assert!(process.wait_for_line("read:"), "the update holds the lock");
    // While it runs, the lock is taken.
    let busy = rag.json(&["update", &name, "--no-wait"], 6);
    assert_eq!(busy["error"]["code"], "index_locked");
    process.kill();

    let started = Instant::now();
    for args in [
        &["update", &name, "--no-wait"][..],
        &["remove", &name, "--no-wait"],
    ] {
        let (code, body) = rag.call(args);
        assert_eq!(code, 0, "{args:?}: {body}");
    }
    note!(
        "after the kill the lock was free in {:.2?}",
        started.elapsed()
    );
}

// --- 11: --no-wait and the default wait ------------------------------------------------

#[test]
fn no_wait_fails_at_once_with_six_while_the_default_waits_and_then_succeeds() {
    let _s = scenario("11 no-wait versus wait (lock held by the test)");
    let rag = Rag::new();
    let upstream = Upstream::stamped(20, 1);
    let name = rag.name_of(&rag.add(&upstream));

    let held = rag.hold_lock_of_only_source();
    let started = Instant::now();
    let error = rag.json(&["update", &name, "--no-wait"], 6);
    let elapsed = started.elapsed();
    assert_eq!(error["error"]["code"], "index_locked");
    assert!(
        elapsed < Duration::from_secs(5),
        "--no-wait took {elapsed:?}"
    );
    note!("--no-wait exited 6 after {elapsed:.2?}");

    let waiting = rag.spawn(&["update", &name]);
    std::thread::sleep(Duration::from_millis(1500));
    drop(held);
    let started = Instant::now();
    let (code, response, stderr) = waiting.finish(&["update", &name]);
    assert_eq!(code, 0, "{response}\n{stderr}");
    assert_eq!(response["sources"][0]["result"], "unchanged");
    note!(
        "the waiting update finished {:.2?} after the release",
        started.elapsed()
    );
}

/// The holder is a real `rag add` of a large repository. The contender may start before
/// the holder finishes (exit 6) or after (exit 0, found already added): both are
/// legitimate, and the second is only possible once the holder has exited.
#[test]
fn a_contender_against_a_running_add_gets_six_or_waits_for_it() {
    let _s = scenario("11 no-wait versus wait (lock held by a rag add)");
    let rag = Rag::new();
    let upstream = Upstream::stamped(FILES, 1);
    let add_args = ["add", upstream.location()];
    let mut holder = rag.spawn(&add_args);
    assert!(holder.wait_for_line("read:"), "the add is working");

    let quick_args = ["add", upstream.location(), "--no-wait"];
    let started = Instant::now();
    let (code, body) = rag.call(&quick_args);
    let elapsed = started.elapsed();
    match code {
        6 => assert_eq!(body["error"]["code"], "index_locked", "{body}"),
        0 => assert!(holder.has_exited(), "got the lock while the add held it"),
        other => panic!("--no-wait exited {other}: {body}"),
    }
    note!("--no-wait: exit {code} after {elapsed:.2?}");

    let waiting_args = ["add", upstream.location()];
    let (code, body) = rag.call(&waiting_args);
    assert_eq!(code, 0, "{body}");
    assert_eq!(body["status"], "ready");
    let (code, body, stderr) = holder.finish(&add_args);
    assert_eq!(code, 0, "{body}\n{stderr}");
    assert_eq!(rag.statuses(), ["ready"]);
}

// --- 10: git integration -----------------------------------------------------------------

/// A repository with a stable file and the one a test changes, added to a fresh home.
fn added_with(files: &[(&str, &str)]) -> (Rag, Upstream, String) {
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/stable.rs", "pub fn stablefunction() {}\n");
    for (name, content) in files {
        upstream.write(name, content);
    }
    upstream.commit("first");
    let name = rag.name_of(&rag.add(&upstream));
    (rag, upstream, name)
}

fn updated_result(rag: &Rag, name: &str) -> String {
    let updated = rag.json(&["update", name], 0);
    updated["sources"][0]["result"].as_str().unwrap().to_owned()
}

#[test]
fn a_renamed_file_is_found_under_its_new_path_only() {
    let _s = scenario("10 rename");
    let (rag, upstream, name) = added_with(&[("src/old.rs", "pub fn renamedfunction() {}\n")]);
    git(&upstream.path, &["mv", "src/old.rs", "src/new.rs"]);
    upstream.commit("rename");

    assert_eq!(updated_result(&rag, &name), "updated");

    assert_eq!(hit_paths(&rag.search("renamedfunction")), ["src/new.rs"]);
    rag.json(&["show", &name, "src/old.rs"], 3);
    assert!(rag.json(&["show", &name, "src/new.rs"], 0)["text"]
        .as_str()
        .unwrap()
        .contains("renamedfunction"));
}

#[test]
fn a_deleted_file_is_no_longer_found_or_shown() {
    let _s = scenario("10 delete");
    let (rag, upstream, name) = added_with(&[("src/doomed.rs", "pub fn deletedfunction() {}\n")]);
    assert_eq!(hit_paths(&rag.search("deletedfunction")), ["src/doomed.rs"]);
    git(&upstream.path, &["rm", "-q", "src/doomed.rs"]);
    upstream.commit("delete");

    assert_eq!(updated_result(&rag, &name), "updated");

    assert!(hit_paths(&rag.search("deletedfunction")).is_empty());
    rag.json(&["show", &name, "src/doomed.rs"], 3);
    assert_eq!(hit_paths(&rag.search("stablefunction")), ["src/stable.rs"]);
}

#[test]
fn a_force_pushed_amend_replaces_the_old_content() {
    let _s = scenario("10 force push");
    let (rag, upstream, name) = added_with(&[("src/amended.rs", "pub fn beforeamend() {}\n")]);
    let before = upstream.head_short();
    upstream.write("src/amended.rs", "pub fn afteramend() {}\n");
    git(&upstream.path, &["add", "-A"]);
    git(
        &upstream.path,
        &["commit", "-q", "--amend", "-m", "amended"],
    );
    let after = upstream.head_short();
    assert_ne!(before, after);

    assert_eq!(updated_result(&rag, &name), "updated");

    assert!(hit_paths(&rag.search("beforeamend")).is_empty());
    assert_eq!(hit_paths(&rag.search("afteramend")), ["src/amended.rs"]);
    let listed = rag.json(&["list"], 0);
    assert_eq!(listed["sources"][0]["commit"], after.as_str());
    assert_eq!(listed["sources"][0]["status"], "ready");
}

/// The upstream's default branch changes from `main` to `trunk`. Whether the source
/// follows the new default or keeps the branch it was added with is not fixed by the
/// plan, so both are accepted; what must hold is that the update succeeds and that
/// the served content and the reported commit belong together.
#[test]
fn a_changed_default_branch_is_followed_or_ignored_consistently() {
    let _s = scenario("10 default branch change");
    let (rag, upstream, name) = added_with(&[("src/main_only.rs", "pub fn mainbranchonly() {}\n")]);
    let on_main = upstream.head_short();
    git(&upstream.path, &["checkout", "-q", "-b", "trunk"]);
    upstream.write("src/trunk_only.rs", "pub fn trunkbranchonly() {}\n");
    git(&upstream.path, &["rm", "-q", "src/main_only.rs"]);
    upstream.commit("trunk");
    let on_trunk = upstream.head_short();
    assert_eq!(
        git_stdout(&upstream.path, &["symbolic-ref", "HEAD"]),
        "refs/heads/trunk"
    );

    let result = updated_result(&rag, &name);
    note!("default branch change: result {result}");

    let listed = rag.json(&["list"], 0);
    let commit = listed["sources"][0]["commit"].as_str().unwrap();
    let main_hits = hit_paths(&rag.search("mainbranchonly"));
    let trunk_hits = hit_paths(&rag.search("trunkbranchonly"));
    if commit == on_trunk {
        assert_eq!(result, "updated");
        assert!(main_hits.is_empty(), "{main_hits:?}");
        assert_eq!(trunk_hits, ["src/trunk_only.rs"]);
    } else {
        assert_eq!(commit, on_main, "an unknown commit");
        assert_eq!(result, "unchanged");
        assert_eq!(main_hits, ["src/main_only.rs"]);
        assert!(trunk_hits.is_empty(), "{trunk_hits:?}");
    }
    assert_eq!(listed["sources"][0]["status"], "ready");
}

#[test]
fn a_file_that_grows_beyond_the_size_cap_becomes_a_removal() {
    let _s = scenario("10 file grows past the cap");
    let (rag, upstream, name) = added_with(&[("src/grows.rs", "pub fn growingfunction() {}\n")]);
    assert_eq!(hit_paths(&rag.search("growingfunction")), ["src/grows.rs"]);
    let mut huge = String::from("pub fn growingfunction() {}\n");
    while huge.len() < 1_200_000 {
        huge.push_str("pub fn padding_line_for_the_size_cap() {}\n");
    }
    upstream.write("src/grows.rs", &huge);
    upstream.commit("grow");

    assert_eq!(updated_result(&rag, &name), "updated");

    assert!(hit_paths(&rag.search("growingfunction")).is_empty());
    rag.json(&["show", &name, "src/grows.rs"], 3);
    assert_eq!(hit_paths(&rag.search("stablefunction")), ["src/stable.rs"]);
}

#[test]
fn a_file_that_becomes_binary_becomes_a_removal() {
    let _s = scenario("10 file becomes binary");
    let (rag, upstream, name) = added_with(&[("src/data.rs", "pub fn textualfunction() {}\n")]);
    assert_eq!(hit_paths(&rag.search("textualfunction")), ["src/data.rs"]);
    upstream.write_bytes(
        "src/data.rs",
        b"pub fn textualfunction() {}\0\0\x01\x02 binary tail",
    );
    upstream.commit("binary");

    assert_eq!(updated_result(&rag, &name), "updated");

    assert!(hit_paths(&rag.search("textualfunction")).is_empty());
    rag.json(&["show", &name, "src/data.rs"], 3);
}

#[test]
fn submodule_and_symlink_entries_are_ignored_without_errors() {
    let _s = scenario("10 submodule and symlink");
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/real.rs", "pub fn realfunction() {}\n");
    upstream.commit("first");
    let first = upstream.head();
    upstream.stage_entry("120000", "src/link.rs", "real.rs");
    upstream.stage_cacheinfo("160000", &first, "vendor/submodule");
    upstream.stage_entry(
        "100644",
        ".gitmodules",
        "[submodule \"vendor/submodule\"]\n\tpath = vendor/submodule\n\turl = https://example.invalid/never.git\n",
    );
    upstream.commit_staged("with a symlink and a submodule");

    let added = rag.json(&["add", upstream.location()], 0);
    let name = rag.name_of(&added);

    assert_eq!(added["status"], "ready");
    let found = rag.search("realfunction");
    assert_eq!(hit_paths(&found), ["src/real.rs"]);
    rag.json(&["show", &name, "src/link.rs"], 3);
    rag.json(&["show", &name, "vendor/submodule"], 3);
    assert_eq!(updated_result(&rag, &name), "unchanged");
}

#[test]
fn crlf_content_is_found_and_shown_line_by_line() {
    let _s = scenario("10 CRLF");
    let (rag, _upstream, name) = added_with(&[(
        "src/windows.rs",
        "pub fn crlffunction() {\r\n    let x = 1;\r\n}\r\n",
    )]);

    assert_eq!(hit_paths(&rag.search("crlffunction")), ["src/windows.rs"]);
    let shown = rag.json(&["show", &name, "src/windows.rs"], 0);
    assert_eq!(shown["total_lines"], 3, "{shown}");
    let lines = rag.json(&["show", &name, "src/windows.rs", "--lines", "2-2"], 0);
    assert!(lines["text"].as_str().unwrap().contains("let x = 1;"));
}

#[test]
fn non_ascii_file_names_are_listed_found_and_shown() {
    let _s = scenario("10 non-ascii names");
    let names = [
        "src/caf\u{e9}.rs",
        "src/\u{65e5}\u{672c}\u{8a9e}.rs",
        "docs/na\u{ef}ve notes.md",
    ];
    let (rag, _upstream, name) = added_with(&[
        (names[0], "pub fn cafefunction() {}\n"),
        (names[1], "pub fn japanesefunction() {}\n"),
        (names[2], "# Notes\n\nnaivenotesmarker lives here.\n"),
    ]);

    assert_eq!(hit_paths(&rag.search("cafefunction")), [names[0]]);
    assert_eq!(hit_paths(&rag.search("japanesefunction")), [names[1]]);
    assert_eq!(hit_paths(&rag.search("naivenotesmarker")), [names[2]]);
    for (file, token) in names
        .iter()
        .zip(["cafefunction", "japanesefunction", "naivenotesmarker"])
    {
        assert!(rag.json(&["show", &name, file], 0)["text"]
            .as_str()
            .unwrap()
            .contains(token));
    }
    assert_eq!(updated_result(&rag, &name), "unchanged");
}

#[test]
fn a_very_long_path_is_indexed_and_shown() {
    let _s = scenario("10 very long path");
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/short.rs", "pub fn shortpathfunction() {}\n");
    git(&upstream.path, &["add", "-A"]);
    let directories: Vec<String> = (0..10).map(|n| format!("{n}{}", "d".repeat(39))).collect();
    let long = format!("{}/deep.rs", directories.join("/"));
    assert!(long.len() > 400);
    upstream.stage_entry("100644", &long, "pub fn deeppathfunction() {}\n");
    upstream.commit_staged("long path");

    let added = rag.json(&["add", upstream.location()], 0);
    let name = rag.name_of(&added);

    assert_eq!(hit_paths(&rag.search("deeppathfunction")), [long.as_str()]);
    assert!(rag.json(&["show", &name, &long], 0)["text"]
        .as_str()
        .unwrap()
        .contains("deeppathfunction"));
    assert_eq!(
        hit_paths(&rag.search("shortpathfunction")),
        ["src/short.rs"]
    );
}

/// An annotated tag on the default branch: the tag object must peel to the commit.
#[test]
fn an_annotated_tag_on_the_default_branch_does_not_confuse_add_or_update() {
    let _s = scenario("10 annotated tag at HEAD");
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/tagged.rs", "pub fn taggedfunction() {}\n");
    upstream.commit("tagged");
    git(&upstream.path, &["tag", "-a", "v1", "-m", "release one"]);

    let added = rag.json(&["add", upstream.location()], 0);
    let name = rag.name_of(&added);

    assert_eq!(added["status"], "ready");
    assert_eq!(hit_paths(&rag.search("taggedfunction")), ["src/tagged.rs"]);
    assert_eq!(
        rag.json(&["list"], 0)["sources"][0]["commit"],
        upstream.head_short().as_str()
    );
    assert_eq!(updated_result(&rag, &name), "unchanged");
}

/// HEAD of the upstream is detached at an annotated tag, so it names no branch. The
/// product has no branch to follow; it must say so as a JSON error without crashing
/// and without leaving a half-registered source behind.
#[test]
fn an_upstream_whose_head_is_detached_at_an_annotated_tag_is_refused_cleanly() {
    let _s = scenario("10 HEAD detached at an annotated tag");
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/tagged.rs", "pub fn detachedfunction() {}\n");
    upstream.commit("tagged");
    git(&upstream.path, &["tag", "-a", "v1", "-m", "release one"]);
    git(&upstream.path, &["checkout", "-q", "--detach", "v1"]);

    let output = rag
        .command()
        .args(["add", upstream.location()])
        .output()
        .unwrap();
    let stderr = text(&output.stderr);
    let code = output.status.code().expect("rag was not killed");
    let body = parse_json(&output.stdout, &["add"], &stderr);

    assert!(!stderr.contains("panicked"), "{stderr}");
    if code == 0 {
        // Following the tag's commit is a legitimate way to handle it too.
        assert_eq!(body["status"], "ready", "{body}");
        assert_eq!(
            hit_paths(&rag.search("detachedfunction")),
            ["src/tagged.rs"]
        );
    } else {
        assert_ne!(code, 101);
        assert!(body["error"]["code"].is_string(), "{body}");
        // The registry either has no row or one honest `failed` row that says why.
        let listed = rag.json(&["list"], 0);
        for source in listed["sources"].as_array().unwrap() {
            assert_eq!(source["status"], "failed", "{source}");
            assert!(source["last_error"]["message"].is_string(), "{source}");
        }
    }
}
