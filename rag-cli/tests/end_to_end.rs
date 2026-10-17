//! End-to-end tests: the built `rag` binary against local git repositories, each
//! test with its own `RUSTY_RAG_HOME` in a temp directory. Nothing touches the
//! user's real data directory or the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use rag_core::application::ports::{OperationLock, SourceRepository};
use rag_core::domain::index_versions::IndexVersions;
use rag_infra::lock::FileOperationLock;
use rag_infra::sqlite::SqliteStore;

use serde_json::Value;
use tempfile::TempDir;

/// Runs git in `dir` with a fixed identity and no prompts.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A throwaway upstream repository on disk.
struct Upstream {
    _temp: TempDir,
    path: PathBuf,
}

impl Upstream {
    /// An initialised repository with no commits.
    fn empty() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join("project");
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        Self { _temp: temp, path }
    }

    /// An initialised repository with no commits, in a directory called dir.
    fn empty_in(dir: &str) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join(dir);
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        Self { _temp: temp, path }
    }

    /// Records a file in the next commit without writing it to the work tree, so
    /// names Windows cannot create (ux.rs) still get into the repository.
    fn stage_blob(&self, name: &str, content: &str) {
        let scratch = self.path.join(".blob-scratch");
        std::fs::write(&scratch, content).unwrap();
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["hash-object", "-w", ".blob-scratch"])
            .output()
            .expect("git runs");
        assert!(output.status.success());
        std::fs::remove_file(&scratch).unwrap();
        let oid = text(&output.stdout).trim().to_owned();
        // Git for Windows refuses device names in the index unless told otherwise.
        git(
            &self.path,
            &[
                "-c",
                "core.protectNTFS=false",
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{oid},{name}"),
            ],
        );
    }

    /// A repository with one commit holding a Rust file and a guide.
    fn with_sources() -> Self {
        let upstream = Self::empty();
        upstream.write(
            "src/config.rs",
            "pub fn parse_config(input: &str) -> Config {\n    Config::from(input)\n}\n",
        );
        upstream.write(
            "docs/guide.md",
            "# Guide\n\n## Setup\n\nRun the installer, then open the dashboard.\n",
        );
        upstream.commit("first");
        upstream
    }

    /// A repository with one commit in `<temp>/<owner>/<name>`, so that two of them can
    /// share a last path segment. Its file mentions `owner`, so a search can tell them
    /// apart.
    fn owned_by(owner: &str, name: &str) -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join(owner).join(name);
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        let upstream = Self { _temp: temp, path };
        upstream.write("src/config.rs", &format!("pub fn {owner}_config() {{}}\n"));
        upstream.commit("first");
        upstream
    }

    fn write(&self, name: &str, content: &str) {
        let file = self.path.join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, content).unwrap();
    }

    fn commit(&self, message: &str) {
        git(&self.path, &["add", "-A"]);
        git(&self.path, &["commit", "-q", "-m", message]);
    }

    fn location(&self) -> &str {
        self.path.to_str().unwrap()
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

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rag"));
        command
            .env("RUSTY_RAG_HOME", self.home.path().join("data"))
            .env("GIT_TERMINAL_PROMPT", "0");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("rag starts")
    }

    /// Runs `args`, expects `code`, and returns stdout parsed as exactly one JSON value.
    fn json(&self, args: &[&str], code: i32) -> Value {
        let output = self.run(args);
        assert_eq!(
            output.status.code(),
            Some(code),
            "rag {args:?}\nstdout: {}\nstderr: {}",
            text(&output.stdout),
            text(&output.stderr)
        );
        parse_json(&output.stdout, args)
    }

    fn add(&self, upstream: &Upstream) -> Value {
        self.json(&["add", upstream.location()], 0)
    }

    fn index_db(&self) -> PathBuf {
        self.home.path().join("data").join("index.db")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_json(stdout: &[u8], args: &[&str]) -> Value {
    let stdout = text(stdout);
    assert_eq!(
        stdout.trim_end().lines().count(),
        1,
        "rag {args:?}: stdout is one JSON line, nothing else: {stdout:?}"
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

#[test]
fn a_local_repository_goes_from_add_to_remove() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();

    // add
    let added = rag.add(&upstream);
    assert_eq!(added["schema_version"], 1);
    assert_eq!(added["status"], "ready");
    assert_eq!(added["already_added"], false);
    assert!(added["index"]["chunks_written"].as_u64().unwrap() >= 2);
    let name = added["name"].as_str().unwrap().to_owned();

    // list
    let listed = rag.json(&["list"], 0);
    let sources = listed["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["name"], name.as_str());
    assert_eq!(sources[0]["status"], "ready");
    assert!(sources[0]["chunks"].as_u64().unwrap() >= 2);

    // search: a camelCase query finds the snake_case source
    let found = rag.json(&["search", "parseConfig"], 0);
    assert_eq!(found["schema_version"], 1);
    assert!(hit_paths(&found).contains(&"src/config.rs".to_owned()));
    assert!(found["total_is_exact"].is_boolean());
    assert_eq!(found["truncated"], false);
    let top = &found["results"][0];
    assert_eq!(top["rank"], 1);
    assert!(top["snippet"].as_str().unwrap().contains("parse_config"));
    let chunk_id = top["chunk"].as_str().unwrap().to_owned();
    assert!(top.get("show").is_none() && top.get("id").is_none());
    assert!(top.get("commit").is_none() && top.get("indexed_at").is_none());
    assert!(top["snippet_lines"].is_array() && top["chunk_lines"].is_array());
    assert_eq!(found["mode"], "and");
    assert_eq!(
        (found["k"].as_u64(), found["max_chars"].as_u64()),
        (Some(5), Some(8000))
    );
    assert!(found["total_matches"].as_u64().unwrap() >= 1);
    assert!(found["next_offset"].is_null());
    assert_eq!(found["budget_cut"], false);
    let info = &found["sources"][top["repo"].as_str().unwrap()];
    assert_eq!(info["name"], name.as_str());
    assert_eq!(info["commit"].as_str().unwrap().len(), 7);
    assert!(info["age_seconds"].is_u64() && info["indexed_at"].is_string());

    // show --chunk
    let chunk = rag.json(&["show", "--chunk", &chunk_id], 0);
    assert_eq!(chunk["chunk"], chunk_id.as_str());
    assert_eq!(chunk["path"], "src/config.rs");
    assert!(chunk["text"].as_str().unwrap().contains("parse_config"));

    // show <repo> <path> --lines
    let lines = rag.json(&["show", &name, "src/config.rs", "--lines", "1-2"], 0);
    assert_eq!(lines["lines"], serde_json::json!([1, 2]));
    assert_eq!(lines["total_lines"], 3);
    assert!(lines["text"]
        .as_str()
        .unwrap()
        .starts_with("pub fn parse_config"));
    let around = rag.json(&["show", &name, "src/config.rs", "--around", "2"], 0);
    assert_eq!(around["lines"], serde_json::json!([1, 3]));

    // update after a new commit: unchanged first, then the new text becomes searchable
    let same = rag.json(&["update"], 0);
    assert_eq!(same["sources"][0]["result"], "unchanged");
    upstream.write("src/zebra.rs", "pub fn zebracrossing() {}\n");
    upstream.commit("second");
    assert!(hit_paths(&rag.json(&["search", "zebracrossing"], 0)).is_empty());
    let updated = rag.json(&["update", &name], 0);
    assert_eq!(updated["sources"][0]["result"], "updated");
    assert_eq!(updated["failed"], 0);
    let after = rag.json(&["search", "zebracrossing"], 0);
    assert_eq!(hit_paths(&after), ["src/zebra.rs"]);

    // reindex of an up-to-date index changes nothing
    let reindexed = rag.json(&["reindex", "--all"], 0);
    assert_eq!(reindexed["sources"][0]["result"], "up_to_date");

    // remove: the source is gone and nothing is left to search
    let removed = rag.json(&["remove", &name], 0);
    assert_eq!(removed["name"], name.as_str());
    let error = rag.json(&["search", "zebracrossing"], 5);
    assert_eq!(error["error"]["code"], "index_missing");
    assert_eq!(rag.json(&["list"], 0)["sources"], serde_json::json!([]));
}

#[test]
fn a_relative_path_is_made_absolute_before_it_is_registered() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let output = rag
        .command()
        .current_dir(&upstream.path)
        .args(["add", "."])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let added = parse_json(&output.stdout, &["add", "."]);
    assert_eq!(added["status"], "ready");
    let listed = rag.json(&["list"], 0);
    assert!(!listed["sources"][0]["repo"]
        .as_str()
        .unwrap()
        .contains('\\'));
}

#[test]
fn add_refuses_a_directory_that_is_not_a_git_repository() {
    let rag = Rag::new();
    let plain = tempfile::tempdir().unwrap();
    let error = rag.json(&["add", plain.path().to_str().unwrap()], 2);
    assert_eq!(error["error"]["code"], "usage");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a git repository"));
}

#[test]
fn add_reports_a_repository_without_commits_as_an_error() {
    let rag = Rag::new();
    let empty = Upstream::empty();
    let output = rag.run(&["add", empty.location()]);
    assert_ne!(output.status.code(), Some(0));
    let error = parse_json(&output.stdout, &["add"]);
    let message = error["error"]["message"].as_str().unwrap().to_lowercase();
    assert!(
        message.contains("commit") || message.contains("empty"),
        "the error should say the repository has no commits: {message}"
    );
}

#[test]
fn adding_the_same_repository_again_is_an_update() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    rag.add(&upstream);
    let again = rag.add(&upstream);
    assert_eq!(again["already_added"], true);
}

#[test]
fn a_search_without_an_index_exits_five_and_creates_nothing() {
    let rag = Rag::new();
    let output = rag.run(&["search", "anything"]);
    assert_eq!(output.status.code(), Some(5));
    let error = parse_json(&output.stdout, &["search"]);
    assert_eq!(error["error"]["code"], "index_missing");
    assert!(error["error"]["hint"]
        .as_str()
        .unwrap()
        .starts_with("ask the user to run `rag add"));
    assert!(
        text(&output.stderr).contains("no index"),
        "plain line on stderr"
    );
    assert!(
        !rag.index_db().exists(),
        "a read must not create the database"
    );
}

#[test]
fn show_and_list_without_an_index_behave_for_a_first_run() {
    let rag = Rag::new();
    assert_eq!(
        rag.json(&["show", "--chunk", &"a".repeat(64)], 5)["error"]["code"],
        "index_missing"
    );
    assert_eq!(rag.json(&["list"], 0)["sources"], serde_json::json!([]));
    assert!(!rag.index_db().exists());
}

#[test]
fn usage_errors_exit_two_and_still_print_the_json_error_shape() {
    let rag = Rag::new();
    for args in [
        &[][..],
        &["frobnicate"],
        &["search"],
        &["search", "x", "-k", "many"],
        &["show", "only-a-repo"],
        &["reindex", "r", "--all"],
    ] {
        let output = rag.run(args);
        assert_eq!(output.status.code(), Some(2), "rag {args:?}");
        let error = parse_json(&output.stdout, args);
        assert_eq!(error["error"]["code"], "usage", "rag {args:?}");
        assert!(error["error"]["message"].is_string());
        assert!(error["error"].get("hint").is_some());
        assert!(
            text(&output.stderr).starts_with("error:"),
            "plain line on stderr"
        );
    }
    for (args, usage) in [
        (&["search"][..], "usage: rag search"),
        (&["show", "only-a-repo"], "usage: rag show"),
        (&["reindex", "r", "--all"], "usage: rag reindex"),
    ] {
        let message = rag.json(args, 2)["error"]["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(message.contains(usage), "rag {args:?}: {message}");
    }
}

#[test]
fn a_query_equal_to_the_human_flag_does_not_switch_to_human_output() {
    let rag = Rag::new();
    let error = rag.json(&["search", "--", "--human"], 5);
    assert_eq!(error["error"]["code"], "index_missing");
}

#[test]
fn a_usage_error_in_human_mode_writes_nothing_to_stdout() {
    let rag = Rag::new();
    let output = rag.run(&["search", "--human"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(text(&output.stderr).starts_with("error:"));
}

#[test]
fn an_unknown_source_exits_three_with_not_found() {
    let rag = Rag::new();
    let name = rag.add(&Upstream::with_sources())["name"]
        .as_str()
        .unwrap()
        .to_owned();
    for args in [
        &["search", "config", "--repo", "nope"][..],
        &["show", "nope", "src/config.rs"],
        &["update", "nope"],
        &["remove", "nope"],
        &["reindex", "nope"],
    ] {
        let error = rag.json(args, 3);
        assert_eq!(error["error"]["code"], "not_found", "rag {args:?}");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("rag list") && message.contains(&name),
            "{message}"
        );
        assert_eq!(
            error["error"]["details"]["valid_names"],
            serde_json::json!([name]),
            "rag {args:?}"
        );
        assert!(error["error"]["hint"].is_string());
    }
}

#[test]
fn a_file_that_is_not_indexed_exits_three() {
    let rag = Rag::new();
    let added = rag.add(&Upstream::with_sources());
    let name = added["name"].as_str().unwrap();
    let error = rag.json(&["show", name, "src/missing.rs"], 3);
    assert_eq!(error["error"]["code"], "not_found");
}

#[test]
fn invalid_arguments_to_a_use_case_are_usage_errors() {
    let rag = Rag::new();
    let added = rag.add(&Upstream::with_sources());
    let name = added["name"].as_str().unwrap();
    for args in [
        &["search", "x", "--lang", "klingon"][..],
        &["show", name, "src/config.rs", "--lines", "9-2"],
        &["show", "--chunk", "not-a-chunk-id"],
    ] {
        assert_eq!(rag.json(args, 2)["error"]["code"], "usage", "rag {args:?}");
    }
}

#[test]
fn a_query_with_nothing_to_find_succeeds_with_a_hint() {
    let rag = Rag::new();
    rag.add(&Upstream::with_sources());
    let found = rag.json(&["search", "qqqxyzzy"], 0);
    assert_eq!(found["returned"], 0);
    assert_eq!(found["mode"], "none");
    assert_eq!(found["sources"], serde_json::json!({}));
    assert_eq!(found["results"], serde_json::json!([]));
    assert!(found["hint"].is_string());
}

#[test]
fn search_options_filter_page_and_cap_the_output() {
    let rag = Rag::new();
    let added = rag.add(&Upstream::with_sources());
    let name = added["name"].as_str().unwrap();

    let rust_only = rag.json(&["search", "config", "--lang", "rust", "--repo", name], 0);
    assert_eq!(hit_paths(&rust_only), ["src/config.rs"]);
    let docs_only = rag.json(&["search", "installer", "--path", "docs/**"], 0);
    assert_eq!(hit_paths(&docs_only), ["docs/guide.md"]);

    let skipped = rag.json(&["search", "config", "--offset", "50"], 0);
    assert_eq!(skipped["returned"], 0);
    assert_eq!(skipped["offset"], 50);

    let capped = rag.run(&["search", "config", "--max-chars", "400"]);
    assert_eq!(capped.status.code(), Some(0));
    assert!(capped.stdout.len() <= 401, "{}", capped.stdout.len());
    let capped = parse_json(&capped.stdout, &["search"]);
    assert_eq!(capped["truncated"], true);
}

#[test]
fn human_output_is_plain_text_without_ansi() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let added = rag.run(&["add", upstream.location(), "--human"]);
    assert_eq!(added.status.code(), Some(0));
    let mut outputs = vec![("add", added)];
    outputs.push(("list", rag.run(&["list", "--human"])));
    outputs.push(("search", rag.run(&["search", "parseConfig", "--human"])));
    outputs.push(("update", rag.run(&["update", "--human"])));
    outputs.push(("reindex", rag.run(&["reindex", "--human"])));
    let listed = rag.json(&["search", "parseConfig"], 0);
    let chunk_id = listed["results"][0]["chunk"].as_str().unwrap().to_owned();
    outputs.push(("show", rag.run(&["show", "--chunk", &chunk_id, "--human"])));

    for (name, output) in &outputs {
        assert_eq!(output.status.code(), Some(0), "{name}");
        let stdout = text(&output.stdout);
        assert!(!stdout.is_empty(), "{name}");
        assert!(!stdout.contains('\u{1b}'), "{name}: no ANSI");
        assert!(
            serde_json::from_str::<Value>(&stdout).is_err(),
            "{name}: human output is not JSON"
        );
    }
    let search = text(&outputs[2].1.stdout);
    assert!(search.contains("src/config.rs"), "{search}");
    assert!(search.contains("rag show --chunk"), "{search}");
    let list = text(&outputs[1].1.stdout);
    assert!(list.starts_with("NAME"), "{list}");
}

#[test]
fn a_long_file_is_cut_with_a_concrete_next_range_and_list_shows_the_short_commit() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let numbered: String = (1..=300).map(|i| format!("line {i}\n")).collect();
    upstream.write("notes/big.txt", &numbered);
    upstream.commit("big");
    let name = rag.add(&upstream)["name"].as_str().unwrap().to_owned();

    let cut = rag.json(&["show", &name, "notes/big.txt"], 0);
    assert_eq!(cut["truncated"], true);
    assert_eq!(cut["lines"], serde_json::json!([1, 200]));
    assert_eq!(cut["next_lines"], "201-300");
    let rest = rag.json(&["show", &name, "notes/big.txt", "--lines", "201-300"], 0);
    assert_eq!(rest["truncated"], false);
    assert!(rest.get("next_lines").is_none());

    let small = rag.json(&["show", &name, "notes/big.txt", "--max-chars", "50"], 0);
    assert_eq!(small["truncated"], true);
    assert!(small["text"].as_str().unwrap().chars().count() <= 100);
    let human = rag.run(&["show", &name, "notes/big.txt", "--human"]);
    let human = text(&human.stdout);
    assert!(human.contains("--lines 201-300"), "{human}");
    assert!(!human.contains("<end>"), "{human}");

    let listed = rag.json(&["list"], 0);
    assert_eq!(listed["sources"][0]["commit"].as_str().unwrap().len(), 7);
    assert_eq!(listed["sources"][0]["name"], name.as_str());
}

#[test]
fn human_errors_go_to_stderr_with_the_hint() {
    let rag = Rag::new();
    let output = rag.run(&["search", "x", "--human"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("error: no index exists yet"), "{stderr}");
    assert!(
        stderr.contains("hint: ask the user to run `rag add"),
        "{stderr}"
    );
}

#[test]
fn stdout_holds_only_the_result_and_progress_goes_to_stderr() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let output = rag.run(&["add", upstream.location()]);
    assert_eq!(output.status.code(), Some(0));
    parse_json(&output.stdout, &["add"]);
    assert!(
        text(&output.stderr).contains("indexing"),
        "progress line on stderr: {:?}",
        text(&output.stderr)
    );
    for args in [&["update"][..], &["reindex"]] {
        let output = rag.run(args);
        parse_json(&output.stdout, args);
        assert!(
            !text(&output.stderr).is_empty(),
            "rag {args:?} reports progress"
        );
    }
    for args in [&["list"][..], &["search", "config"]] {
        let output = rag.run(args);
        parse_json(&output.stdout, args);
        assert!(output.stderr.is_empty(), "reads print nothing on stderr");
    }
}

#[test]
fn update_and_reindex_with_no_sources_say_so_and_succeed() {
    let rag = Rag::new();
    for command in ["update", "reindex"] {
        let response = rag.json(&[command], 0);
        assert_eq!(response["sources"], serde_json::json!([]));
        assert!(response["message"].as_str().unwrap().contains("rag add"));
    }
}

#[test]
fn help_and_version_exit_zero_on_stdout() {
    let rag = Rag::new();
    for flag in ["--help", "--version"] {
        let output = rag.run(&[flag]);
        assert_eq!(output.status.code(), Some(0), "{flag}");
        assert!(!output.stdout.is_empty(), "{flag}");
    }
}

// --- round 1 of section 12.2: safety, partial failure, locking, recovery ----------

impl Rag {
    fn data_dir(&self) -> PathBuf {
        self.home.path().join("data")
    }

    /// The operation lock of the only registered source, held by this test process
    /// while a real `rag` process runs.
    fn hold_lock_of_only_source(&self) -> rag_core::application::ports::LockGuard {
        let store = SqliteStore::open_for_read(&self.data_dir()).unwrap();
        let sources = store.list().unwrap();
        assert_eq!(sources.len(), 1);
        FileOperationLock::new(self.data_dir().join("locks"))
            .acquire(sources[0].id())
            .expect("the lock is free")
    }
}

impl Upstream {
    fn commit_staged(&self, message: &str) {
        git(&self.path, &["commit", "-q", "-m", message]);
    }
}

const SECRET_TOKEN: &str = "tok3n-must-not-leak";

#[test]
fn a_credential_in_the_url_never_reaches_stdout_stderr_or_the_data_directory() {
    let rag = Rag::new();
    let url = format!("https://user:{SECRET_TOKEN}@127.0.0.1:9/o/r.git");
    for human in [false, true] {
        let mut args = vec!["add", url.as_str()];
        if human {
            args.push("--human");
        }
        let output = rag.run(&args);
        assert_ne!(output.status.code(), Some(0), "{args:?}");
        assert!(
            !text(&output.stdout).contains(SECRET_TOKEN),
            "stdout {args:?}: {}",
            text(&output.stdout)
        );
        assert!(
            !text(&output.stderr).contains(SECRET_TOKEN),
            "stderr {args:?}: {}",
            text(&output.stderr)
        );
    }
    // A bad argument makes clap quote it back.
    let output = rag.run(&["search", "x", "--bogus", &url]);
    assert_eq!(output.status.code(), Some(2));
    assert!(!text(&output.stdout).contains(SECRET_TOKEN));
    assert!(!text(&output.stderr).contains(SECRET_TOKEN));
    // Nothing the registry stored mentions it either.
    let listed = rag.run(&["list"]);
    assert!(!text(&listed.stdout).contains(SECRET_TOKEN));
    if let Ok(database) = std::fs::read(rag.index_db()) {
        let database = String::from_utf8_lossy(&database);
        assert!(!database.contains(SECRET_TOKEN));
    }
}

/// True when `value` holds a control or invisible character other than TAB and newline.
fn has_hidden_characters(value: &str) -> bool {
    value.chars().any(|c| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}' | '\u{e0000}'..='\u{e007f}')
    })
}

fn strings_of(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings_of(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings_of(item, out)),
        _ => {}
    }
}

#[test]
fn hostile_file_content_cannot_reach_the_terminal_through_json_or_human_output() {
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write(
        "src/hostile.rs",
        "pub fn hostilemarker() {} // \u{1b}[2J\u{1b}]0;owned\u{7} \u{202e}reversed \u{200b}zero \u{e0041}\u{e0042}tag \u{9b}31m\n",
    );
    upstream.write(
        "docs/hostile.md",
        "# Title \u{202e}evil \u{e0041}\n\nhostilemarker in a paragraph \u{1b}[31mred\n",
    );
    upstream.commit("hostile");
    let added = rag.add(&upstream);
    let name = added["name"].as_str().unwrap().to_owned();

    let found = rag.run(&["search", "hostilemarker"]);
    assert_eq!(found.status.code(), Some(0));
    let found_json = parse_json(&found.stdout, &["search"]);
    assert_eq!(found_json["content"], "untrusted");
    assert!(!found_json["results"].as_array().unwrap().is_empty());
    let mut strings = Vec::new();
    strings_of(&found_json, &mut strings);
    for s in &strings {
        assert!(!has_hidden_characters(s), "search JSON string: {s:?}");
    }
    assert!(
        !text(&found.stdout).contains("\\u001b"),
        "no escaped ESC either"
    );

    let chunk = found_json["results"][0]["chunk"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = rag.json(&["show", "--chunk", &chunk], 0);
    assert_eq!(shown["content"], "untrusted");
    let mut strings = Vec::new();
    strings_of(&shown, &mut strings);
    for s in &strings {
        assert!(!has_hidden_characters(s), "show JSON string: {s:?}");
    }
    let by_path = rag.json(&["show", &name, "src/hostile.rs"], 0);
    assert!(by_path["text"].as_str().unwrap().contains("hostilemarker"));
    assert!(!has_hidden_characters(by_path["text"].as_str().unwrap()));

    for args in [
        &["search", "hostilemarker", "--human"][..],
        &["show", "--chunk", &chunk, "--human"],
        &["show", &name, "src/hostile.rs", "--human"],
        &["list", "--human"],
    ] {
        let output = rag.run(args);
        assert_eq!(output.status.code(), Some(0), "{args:?}");
        let stdout = text(&output.stdout);
        assert!(!has_hidden_characters(&stdout), "{args:?}: {stdout:?}");
    }
}

#[test]
fn files_judged_secret_are_reported_and_cannot_be_shown_or_found() {
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/ok.rs", "pub fn visiblefunction() {}\n");
    upstream.write(".env", "API_KEY=supersecretvalue\n");
    upstream.write(
        "notes.txt",
        "private notes\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAsecretbody\n-----END RSA PRIVATE KEY-----\n",
    );
    upstream.commit("secrets");

    let added = rag.add(&upstream);
    let name = added["name"].as_str().unwrap().to_owned();

    assert_eq!(added["index"]["files_added"], 1, "only ok.rs counts");
    let listed: Vec<&str> = added["index"]["skipped_secret_paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(listed, [".env", "notes.txt"]);
    for path in [".env", "notes.txt"] {
        let error = rag.json(&["show", &name, path], 3);
        assert_eq!(error["error"]["code"], "not_found", "{path}");
    }
    for query in ["supersecretvalue", "secretbody"] {
        assert!(hit_paths(&rag.json(&["search", query], 0)).is_empty());
    }
    assert_eq!(
        hit_paths(&rag.json(&["search", "visiblefunction"], 0)),
        ["src/ok.rs"]
    );
}

#[test]
fn a_stale_index_exits_seven_until_reindexed() {
    let rag = Rag::new();
    let added = rag.add(&Upstream::with_sources());
    let name = added["name"].as_str().unwrap().to_owned();
    rag_infra::testing::age_index(
        &rag.data_dir(),
        IndexVersions {
            tokenizer: 0,
            ..IndexVersions::current()
        },
    );

    let search = rag.json(&["search", "config"], 7);
    assert_eq!(search["error"]["code"], "reindex_required");
    let show = rag.json(&["show", &name, "src/config.rs"], 7);
    assert_eq!(show["error"]["code"], "reindex_required");

    let reindexed = rag.json(&["reindex", "--all"], 0);
    assert_eq!(reindexed["failed"], 0);
    assert_eq!(
        hit_paths(&rag.json(&["search", "parseConfig"], 0)),
        ["src/config.rs"]
    );
}

#[test]
fn a_source_busy_in_another_process_exits_six_and_a_short_wait_succeeds() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let name = rag.add(&upstream)["name"].as_str().unwrap().to_owned();

    {
        let _held = rag.hold_lock_of_only_source();
        for args in [
            &["update", &name, "--no-wait"][..],
            &["add", upstream.location(), "--no-wait"],
            &["remove", &name],
        ] {
            let error = rag.json(args, 6);
            assert_eq!(error["error"]["code"], "index_locked", "{args:?}");
        }
        // Update of everything skips the busy source; that is not a failure.
        let all = rag.json(&["update", "--no-wait"], 0);
        assert_eq!(all["sources"][0]["result"], "skipped_busy");
        assert_eq!(all["failed"], 0);
        // Searching never takes the lock.
        assert_eq!(rag.json(&["search", "parseConfig"], 0)["returned"], 1);
    }

    // The default wait outlasts a holder that lets go within a second.
    let held = rag.hold_lock_of_only_source();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(800));
        drop(held);
    });
    let updated = rag.json(&["update", &name], 0);
    assert_eq!(updated["sources"][0]["result"], "unchanged");
    releaser.join().unwrap();
}

#[test]
fn update_all_with_one_failing_source_updates_the_others_and_exits_with_its_code() {
    let rag = Rag::new();
    let good = Upstream::with_sources();
    let doomed = Upstream::with_sources();
    let good_name = rag.add(&good)["name"].as_str().unwrap().to_owned();
    let doomed_name = rag.add(&doomed)["name"].as_str().unwrap().to_owned();
    good.write("src/fresh.rs", "pub fn freshfunction() {}\n");
    good.commit("fresh");
    std::fs::remove_dir_all(doomed.path.join(".git")).unwrap();

    let output = rag.run(&["update"]);
    let response = parse_json(&output.stdout, &["update"]);

    assert_eq!(response["failed"], 1, "{response}");
    let entry = |name: &str| {
        response["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap_or_else(|| panic!("{name} in {response}"))
            .clone()
    };
    assert_eq!(entry(&good_name)["result"], "updated");
    let failed = entry(&doomed_name);
    assert_eq!(failed["result"], "failed");
    let code = failed["error"]["code"].as_str().unwrap();
    let expected_exit = match code {
        "not_found" => 3,
        "network_or_auth" => 4,
        _ => 1,
    };
    assert_eq!(output.status.code(), Some(expected_exit), "{code}");
    assert_ne!(code, "index_locked");
    // The good source really was updated, and the bad one says why in `list`.
    assert_eq!(
        hit_paths(&rag.json(&["search", "freshfunction"], 0)),
        ["src/fresh.rs"]
    );
    let listed = rag.json(&["list"], 0);
    let broken = listed["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == doomed_name.as_str())
        .unwrap();
    assert_eq!(broken["status"], "failed");
    assert_eq!(broken["last_error"]["code"], code);
    assert!(
        broken["commit"].is_string(),
        "the old index is still served"
    );
}

#[test]
fn a_taken_last_segment_gives_the_second_repository_its_owner_as_well() {
    let rag = Rag::new();
    let first = rag.add(&Upstream::owned_by("alice", "serde"));
    let second = rag.add(&Upstream::owned_by("bob", "serde"));
    assert_eq!(first["name"], "serde");
    assert_eq!(second["name"], "bob/serde");
    assert_ne!(first["repo"], second["repo"]);
}

#[test]
fn repo_accepts_the_short_name_and_the_owner_and_name_of_a_source() {
    let rag = Rag::new();
    let alice = rag.add(&Upstream::owned_by("alice", "serde"));
    let bob = rag.add(&Upstream::owned_by("bob", "serde"));
    for (query, repo, source) in [("alice", "serde", &alice), ("bob", "bob/serde", &bob)] {
        let found = rag.json(&["search", query, "--repo", repo], 0);
        let results = found["results"].as_array().unwrap();
        assert!(!results.is_empty(), "rag search --repo {repo}: {found}");
        assert!(
            results
                .iter()
                .all(|result| result["repo"] == source["repo"]),
            "rag search --repo {repo}: {found}"
        );
    }
}

/// Registers `url` with `rag add` at a port nothing listens on. The add fails, but the
/// source stays registered as failed, which is all that name resolution needs, and
/// nothing touches the network.
fn register_unreachable(rag: &Rag, url: &str, name: Option<&str>) {
    let mut args = vec!["add", url];
    if let Some(name) = name {
        args.extend(["--name", name]);
    }
    let output = rag.run(&args);
    assert_ne!(output.status.code(), Some(0), "rag {args:?} should fail");
}

#[test]
fn a_unique_owner_and_name_suffix_resolves_a_remote_source() {
    let rag = Rag::new();
    register_unreachable(&rag, "https://127.0.0.1:1/serde-rs/serde", None);
    let removed = rag.json(&["remove", "serde-rs/serde"], 0);
    assert_eq!(removed["repo"], "127.0.0.1/serde-rs/serde");
    assert_eq!(removed["name"], "serde");
}

#[test]
fn a_unique_last_segment_resolves_a_remote_source() {
    let rag = Rag::new();
    register_unreachable(&rag, "https://127.0.0.1:1/serde-rs/serde", None);
    let removed = rag.json(&["remove", "serde"], 0);
    assert_eq!(removed["repo"], "127.0.0.1/serde-rs/serde");
}

#[test]
fn an_ambiguous_suffix_is_a_usage_error_naming_both_sources() {
    let rag = Rag::new();
    register_unreachable(&rag, "https://127.0.0.1:1/alice/serde", Some("one"));
    register_unreachable(&rag, "https://127.0.0.1:1/bob/serde", Some("two"));
    let error = rag.json(&["remove", "serde"], 2);
    assert_eq!(error["error"]["code"], "usage");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("several sources"), "{message}");
    assert!(
        message.contains("\"one\"") && message.contains("\"two\""),
        "{message}"
    );
}

#[test]
fn a_file_named_like_a_windows_device_is_showable() {
    let rag = Rag::new();
    let upstream = Upstream::empty();
    upstream.write("src/lib.rs", "pub fn libfunction() {}\n");
    upstream.stage_blob("src/aux.rs", "pub fn auxfunction() {}\n");
    git(&upstream.path, &["add", "src/lib.rs"]);
    upstream.commit_staged("with aux");
    let name = rag.add(&upstream)["name"].as_str().unwrap().to_owned();

    let shown = rag.json(&["show", &name, "src/aux.rs"], 0);
    assert!(shown["text"].as_str().unwrap().contains("auxfunction"));
    assert_eq!(
        hit_paths(&rag.json(&["search", "auxfunction"], 0)),
        ["src/aux.rs"]
    );
}

#[test]
fn a_repository_in_a_non_ascii_path_with_spaces_is_indexed_and_shown() {
    let rag = Rag::new();
    let upstream = Upstream::empty_in("pr\u{f6}ject dir \u{65e5}\u{672c}");
    upstream.write("src/caf\u{e9} file.rs", "pub fn cafefunction() {}\n");
    upstream.commit("unicode");

    let added = rag.add(&upstream);
    let name = added["name"].as_str().unwrap().to_owned();
    assert!(!added["repo"].as_str().unwrap().contains('\\'));

    let found = rag.json(&["search", "cafefunction"], 0);
    assert_eq!(hit_paths(&found), ["src/caf\u{e9} file.rs"]);
    let shown = rag.json(&["show", &name, "src/caf\u{e9} file.rs"], 0);
    assert!(shown["text"].as_str().unwrap().contains("cafefunction"));
    assert_eq!(
        rag.json(&["update", &name], 0)["sources"][0]["result"],
        "unchanged"
    );
}

#[test]
fn adding_a_repository_without_commits_exits_three_with_not_found() {
    let rag = Rag::new();
    let empty = Upstream::empty();
    let output = rag.run(&["add", empty.location()]);
    assert_eq!(output.status.code(), Some(3));
    let error = parse_json(&output.stdout, &["add"]);
    assert_eq!(error["error"]["code"], "not_found");
}

fn big_upstream(files: usize) -> Upstream {
    let upstream = Upstream::empty();
    for n in 0..files {
        upstream.write(
            &format!("src/module{n}.rs"),
            &format!("pub fn function_number_{n}() {{}}\n").repeat(40),
        );
    }
    upstream.write("src/marker.rs", "pub fn recoverymarker() {}\n");
    upstream.commit("big");
    upstream
}

#[test]
fn an_add_killed_midway_can_be_added_again() {
    let rag = Rag::new();
    let upstream = big_upstream(300);
    for delay in [20, 150, 400] {
        let mut child = rag
            .command()
            .args(["add", upstream.location()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("rag starts");
        std::thread::sleep(Duration::from_millis(delay));
        let _ = child.kill();
        let _ = child.wait();
    }

    let added = rag.add(&upstream);

    assert_eq!(added["status"], "ready");
    assert_eq!(
        hit_paths(&rag.json(&["search", "recoverymarker"], 0)),
        ["src/marker.rs"]
    );
    let listed = rag.json(&["list"], 0);
    assert_eq!(listed["sources"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sources"][0]["status"], "ready");
}

#[test]
fn two_processes_adding_the_same_repository_at_once_end_with_one_source() {
    let rag = Rag::new();
    let upstream = big_upstream(120);
    let spawn = || {
        rag.command()
            .args(["add", upstream.location()])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("rag starts")
    };
    let first = spawn();
    let second = spawn();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];

    let mut already = 0;
    for output in &outputs {
        assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
        let response = parse_json(&output.stdout, &["add"]);
        assert_eq!(response["status"], "ready");
        if response["already_added"] == true {
            already += 1;
        }
    }
    assert_eq!(already, 1, "the loser waited for the lock and then updated");
    assert_eq!(
        rag.json(&["list"], 0)["sources"].as_array().unwrap().len(),
        1
    );
    assert_eq!(rag.json(&["search", "recoverymarker"], 0)["returned"], 1);
}

#[test]
fn a_panic_free_run_prints_phase_lines_and_never_a_credential_on_stderr() {
    let rag = Rag::new();
    let upstream = Upstream::with_sources();
    let output = rag.run(&["add", upstream.location()]);
    let stderr = text(&output.stderr);
    for phase in ["resolve:", "fetch:", "list:", "read:", "commit:"] {
        assert!(stderr.contains(phase), "{phase} in {stderr}");
    }
    let response = parse_json(&output.stdout, &["add"]);
    let phases: Vec<&str> = response["index"]["phases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["phase"].as_str().unwrap())
        .collect();
    assert_eq!(phases, ["resolve", "fetch", "list", "read", "commit"]);
    assert!(response["index"]["phases"][0]["elapsed_ms"].is_u64());
}
