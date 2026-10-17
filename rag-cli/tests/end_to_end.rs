//! End-to-end tests: the built `rag` binary against local git repositories, each
//! test with its own `RUSTY_RAG_HOME` in a temp directory. Nothing touches the
//! user's real data directory or the network.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    let chunk_id = top["id"].as_str().unwrap().to_owned();
    assert_eq!(top["show"], format!("rag show --chunk {chunk_id}").as_str());

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
    assert!(error["error"]["hint"].as_str().unwrap().contains("rag add"));
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
    rag.add(&Upstream::with_sources());
    for args in [
        &["search", "config", "--repo", "nope"][..],
        &["show", "nope", "src/config.rs"],
        &["update", "nope"],
        &["remove", "nope"],
        &["reindex", "nope"],
    ] {
        let error = rag.json(args, 3);
        assert_eq!(error["error"]["code"], "not_found", "rag {args:?}");
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
    let chunk_id = listed["results"][0]["id"].as_str().unwrap().to_owned();
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
fn human_errors_go_to_stderr_with_the_hint() {
    let rag = Rag::new();
    let output = rag.run(&["search", "x", "--human"]);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    let stderr = text(&output.stderr);
    assert!(stderr.contains("error: no index exists yet"), "{stderr}");
    assert!(stderr.contains("hint: run `rag add"), "{stderr}");
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
