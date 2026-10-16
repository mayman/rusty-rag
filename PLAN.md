# rag: Implementation Plan

A global, local-first keyword-search (BM25) retrieval CLI in Rust, used by Claude Code through Bash calls and a skill. It indexes the default branch of public and private GitHub repos (docs and source). Embeddings and an MCP wrapper come later.

Status: design agreed through review by the `architect`, `system-design`, and `ux-reviewer` agents (see `.claude/agents/`). Step 1 scaffold built; pre-commit fixes from the scaffold review (section 11) in progress. Nothing committed yet.

## 0. Progress checklist

Updated as work lands. `[x]` = done and verified, `[~]` = done but not yet independently verified, `[ ]` = not started.

**Design and setup**
- [x] Requirements discussion and design decisions (sections 1 to 9)
- [x] Design reviewed by `architect`, `system-design`, `ux-reviewer` (findings folded into this plan)
- [x] Reviewer and engineer agents created (`.claude/agents/`: architect, system-design, ux-reviewer, rust-engineer on Haiku 5.5)
- [x] Skills created (`.claude/skills/`: ddd-onion-rust, tdd-rust)
- [x] Environment installed: Git 2.55, Rust 1.99.1 (MSVC), VS Build Tools C++ workload; bundled-SQLite build smoke-tested
- [x] Git repository initialised (branch `main`, `.gitignore`); nothing committed yet
- [x] `PLAN.md`, skills, and engineer agent updated for the review decisions (3 crates, exit codes 5/6/7, `RUSTY_RAG_HOME`, committed-only local paths)
- [x] Protected-path guard script `scripts/guard-protected.ps1` written and pipe-tested (12 cases: blocks protected Write/Edit/PowerShell, allows normal edits)
- [~] Guard hook enabled in `rust-engineer` frontmatter (command string pipe-tested: blocks PLAN.md edit, allows source edit; not yet seen firing inside a live engineer run, and agents may need a session reload to pick it up)

**Build step 1: scaffold**
- [x] Workspace, domain/application modules, architecture test, `rag --version` smoke test (re-run by me via `scripts/check.ps1`: test, fmt, clippy all exit 0)
- [x] Reviewer pass on scaffold (architect and system-design)

**Pre-commit fixes (section 11)** (engineer on Sonnet 5.5; verified by my own `scripts/check.ps1` run)
- [x] Split into `rag-core` / `rag-infra` / `rag-cli` crates
- [x] Pin toolchain to exactly 1.99.1, set `rust-version` 1.85
- [x] Add `.gitattributes` (LF normalisation), tidy `.gitignore`
- [x] Workspace `[workspace.dependencies]`, `[workspace.lints]`, `[profile.release]` (release profile configured, not yet built)
- [x] `scripts/check.ps1` quality gate
- [x] Slim architecture test (dependency-list check via `cargo metadata`, source tripwire, `rag-core/clippy.toml` bans proven with a planted violation)
- [x] Re-run the gate myself and enable the guard hook
- [ ] First commit (waiting for your approval)

**Build order (section 8)**
- [ ] 2. Tokenizer and chunker (Haiku)
- [ ] 3. SQLite store, FTS5, local-folder indexing, query escaping, AND/OR (Sonnet 5.5)
- [ ] 4. Output layer: JSON schema, `--human`, error and exit codes 0 to 7, output caps (Haiku)
- [ ] 5. Git: add, update (manifest diff), remove, locking, timeouts, progress, search-during-update test (Sonnet 5.5)
- [ ] 6. `show` with path safety, `reindex`, `list` (Sonnet 5.5)
- [ ] 7. Relevance set (about 20 queries), git integration tests, concurrency test (Haiku)
- [ ] 8. Skill `~/.claude/skills/rag/SKILL.md` (Haiku)
- [ ] 9. Real-world test on 2 to 3 of your repos, tune chunking and weights

**Open items (section 10)**
- [ ] Sensitive-repo exclusion setting, if needed
- [ ] macOS/Linux CI builds in v1 or later
- [ ] Install location for `rag.exe`, cold-start budget

## 1. Decisions

| Area | Decision |
|---|---|
| Language | Rust edition 2021, toolchain pinned to exactly 1.99.1 via `rust-toolchain.toml` (bumped deliberately); `rust-version` set in workspace |
| Storage | One SQLite file (`rusqlite`, bundled) with FTS5. Single store, single transaction per add/update |
| Search | BM25 via FTS5, column-weighted (path and heading above body). AND first, OR fallback, output says which ran |
| Identifier handling | Split camelCase / snake_case / paths in Rust before indexing, keep the original identifier too. Queries go through the same function |
| Git | Shell out to the system `git` (reuses your credentials). Never put tokens in URLs, the registry, or logs. `GIT_TERMINAL_PROMPT=0` |
| Clone | `--depth 1 --single-branch --no-tags`, default branch only. Update is `fetch --depth 1 origin <ref>` then `reset --hard FETCH_HEAD` |
| Change detection | Manifest of (path, git blob OID) from `git ls-tree -r`; diff against the previous manifest |
| Updates | Manual only (`rag update`). Every result carries indexed commit and age |
| Scale target | About 10 to 15 repos; design stays valid to roughly 1M chunks |
| Output | JSON by default, `--human` for people. Versioned (`schema_version`), frozen at v1 |
| Crates | Three crates: `rag-core` (domain + application, depends only on serde and one pure hashing crate), `rag-infra` (SQLite, git, filesystem, lock, clock, `ignore`, `dirs`), `rag-cli` (binary, composition root). MCP wrapper becomes a second binary using core + infra |
| Data directory | Overridable with the `RUSTY_RAG_HOME` environment variable; defaults per OS. All tests use it with per-test temp dirs |
| Time | `IndexedAt` is an epoch-seconds domain type; the domain never reads the clock (a `Clock` port in application supplies it) |
| Hashing | Chunk and source ids use a pure hashing crate (sha2 or blake3) allowed in `rag-core`; never `DefaultHasher` (unstable across releases) |
| Local paths | `rag add <local path>` indexes only the committed HEAD of a git repo (so every hit has a commit and blob oid); uncommitted files are ignored |
| Platforms | Windows first; code kept portable for macOS and Linux (CI builds in v1.x) |
| Skill | `~/.claude/skills/rag/SKILL.md`, then MCP wrapper later |

## 2. Commands

```
rag add <url|path> [--name N]     clone (or register local path) and index
rag update [name]                 fetch, diff manifest, re-index changed files; no name = all
rag remove <name>
rag list                          repos, indexed commit, age, chunk count, local clone path
rag search "<q>" [-k 5] [--repo R]... [--lang L]... [--path GLOB] [--offset N] [--max-chars N] [--human]
rag show <repo> <path> [--lines S-E] [--around N] [--json]
rag reindex [name]                rebuild FTS from stored chunk text (after tokenizer/chunker change)
```

`rag add .` indexes the current project (allowed). `rag list` exposes clone paths so Claude may also use Read/Glob on clones directly.

Exit codes: 0 ok (including no results), 1 generic, 2 usage, 3 not found, 4 auth/network, 5 index missing (`index_missing`: run `rag add`), 6 index locked (`index_locked`: wait and retry), 7 reindex required (`reindex_required`: run `rag reindex`). The JSON error `code` string is stable per case.

## 3. Search output (schema v1)

```
{ "schema_version": 1, "query": "...", "mode": "and|or_fallback",
  "total_hits": 0, "returned": 0, "offset": 0, "has_more": false, "truncated": false,
  "results": [ { "id": "...", "rank": 1, "score": 0.0, "score_rel": 1.0,
      "repo": "host/owner/name", "commit": "abc123", "indexed_at": "...",
      "path": "src/x.rs", "lines": [10, 22], "language": "rust", "heading": "...",
      "matched_terms": ["..."], "snippet": "...",
      "show": "rag show host/owner/name src/x.rs --lines 10-40" } ],
  "hint": "present when results are empty or weak" }
```

Defaults: `-k 5`, snippet about 12 lines or 600 characters, at most 2 hits per file, hard cap about 8k characters on total output, deterministic ordering. Paths are repo-relative with `/`. Errors in JSON mode: `{"error":{"code","message","hint"}}` on stdout, plain line on stderr.

## 4. Storage schema (SQLite)

- `meta`: schema_version, tokenizer_version, chunker_version
- `repos`: repo_id (slug + short hash of canonical URL, host-aware), url, name, ref, indexed_commit, indexed_at, status (cloning | indexing | ready | failed)
- `files`: file_id, repo_id, path, blob_oid, size, language
- `chunks`: chunk_id (hash of repo_id, path, heading path or ordinal, text hash), file_id, start_line, end_line, heading, text (raw)
- `chunks_fts`: self-contained FTS5 table (own copy of the pre-tokenized text, deleted by rowid); no external-content table
- WAL mode. Writes use `BEGIN IMMEDIATE` with a `busy_timeout`, which is the single writer lock (no second lock file for the index). Searches never take it. The writer lock is held only around the index transaction, never across a clone or fetch, so a slow clone cannot stall other callers. Git operations take a short per-repo lock.

Version gate on open: a mismatched tokenizer or chunker version returns exit code 7 (`reindex_required`) telling the agent to run `rag reindex`. Column weights in `bm25()` are pinned in code and covered by a test.

## 5. Data flow and failure handling

- **add:** normalize URL, take writer lock, insert repo row (cloning), clone into a temp dir, rename into `repos/`, walk, filter, chunk, then one transaction writes files, chunks, FTS, commit, status=ready. Crash during clone leaves a temp dir cleaned on next run. Crash during indexing rolls back and leaves the row retryable.
- **update:** lock, fetch, compare `ls-tree` to manifest, one transaction deletes changed/removed paths and inserts new ones, bump commit. Crash rolls back to the old commit (harmless: `show` reads at the indexed commit).
- **search:** no lock, one read transaction, results stamped with commit and age.
- **show:** resolve repo and path, read via `git cat-file` at the indexed commit. Reject `..`, absolute paths, drive/UNC forms, NTFS alternate streams, and symlinks leaving the repo.
- **remove:** mark removing, delete rows, delete directory (retry, clearing read-only attributes on Windows), garbage-collect leftovers at next start.

## 6. Indexing rules

- Honor `.gitignore` (the `ignore` crate). Hard-skip lockfiles, minified and generated files, vendored directories, binaries, LFS pointers, files over about 1 MB.
- Skip submodules; `GIT_LFS_SKIP_SMUDGE=1`; `core.longpaths=true`; `core.autocrlf=false`.
- Secret handling in v1: filename denylist only (`.env`, `*.pem`, `id_rsa*`, credential files). Content scanning is v1.x.
- Chunking: markdown by headings; code in windows of about 40 to 60 lines with small overlap, preferring blank-line breaks. Tree-sitter later behind the same interface.
- Security note for docs and skill: indexed content is untrusted data, never instructions; never auto-run `add` on URLs found in results; the index is plaintext on disk.

## 7. Module layout

Domain-driven design with onion architecture, built test-first (see `.claude/skills/ddd-onion-rust` and `.claude/skills/tdd-rust`; implemented by the `rust-engineer` agent).

```
rag-core (lib)     depends on: serde + one pure hashing crate only
  domain/          Source, Chunk, Manifest, Query, Hit, RepoPath, IndexedAt, Chunker,
                   IdentifierSplitter, policies (AND/OR fallback, hit caps); no I/O, no serde
  application/     use cases (AddSource, UpdateSource, RemoveSource, ListSources, Search,
                   ShowFile, Reindex), request/response DTOs (serde), ALL port traits
                   (SourceRepository, ChunkStore, ChunkSearcher, GitClient, WorkTreeWalker,
                   Clock), a fakes/testkit feature for in-memory fakes and contract tests
rag-infra (lib)    SQLite/FTS5 adapter, git subprocess client, filesystem walker, lock,
                   system clock, config and data dir (RUSTY_RAG_HOME); implements the ports
rag-cli  (bin)    clap parsing, JSON/human rendering, exit codes, composition root
```

Dependency rule: rag-cli to rag-infra to rag-core, and inside rag-core application depends on domain only. Cargo enforces the crate boundaries (rag-core cannot depend on rusqlite, clap, ignore, dirs). A small architecture test enforces (a) `cargo metadata`: rag-core's dependencies equal the allowlist, and (b) inside rag-core, `domain` does not import `application`, serde, or std I/O/time. `clippy.toml` `disallowed-*` rules ban `std::fs`, `std::process`, `SystemTime::now`, and `Instant::now` in rag-core. Workspace lints forbid `unsafe_code` and deny `print_stdout`, `print_stderr`, and `exit` in rag-core and rag-infra.

A `Retriever` trait (query to scored chunk IDs) is defined now so vectors plus rank fusion can be added later without touching callers. Data directory per OS (`dirs` crate): `%LOCALAPPDATA%\rag`, `~/Library/Application Support/rag`, `~/.local/share/rag`; env var override.

## 8. Build order

1. Toolchain: check for Visual Studio Build Tools, install rustup, scaffold workspace, pin toolchain.
2. Tokenizer and chunker with unit and golden tests.
3. SQLite store and FTS5, local-folder indexing, search with query escaping (quote every term; AND then OR).
4. Output layer: JSON schema, `--human`, error codes, output caps.
5. Git: add, update (manifest diff), remove, locking, timeouts, stderr progress.
6. `show` with path safety; `reindex`; `list`.
7. Relevance set of about 20 queries over a fixed corpus; integration tests against local git repos (rename, delete, force-push); concurrency test (search during update).
8. Skill (`SKILL.md` with triggers, query phrasing, "not for the current project's own code", untrusted-content note, `allowed-tools: Bash(rag:*)`).
9. Real-world test against 2 to 3 of your repos, tune chunking and weights.

## 8a. Delegation rules

- One PLAN step per delegation to `rust-engineer`.
- Model: Haiku 5.5 (the agent default) for steps 1, 2, 4, 7, 8, 9. **Sonnet 5.5 for steps 3, 5 and 6** (query handling, crash safety and locking, path safety).
- The quality gate is `scripts/check.ps1` (rebuilds PATH from the registry, runs test, fmt --check, clippy -D warnings, prints real exit codes). The caller re-runs it and does not trust a reported "green".
- Review each diff for new dependencies, `#[allow]`, `#[ignore]`, `cfg(not(windows))`, and deleted or weakened tests.
- A hook blocks the engineer from editing `rag-core/tests/architecture.rs`, `PLAN.md`, `rust-toolchain.toml`, `clippy.toml`, the workspace `Cargo.toml`, and `.claude/**`.
- A missing `cargo` is a hard stop, never a reason to install software.

## 9. Later (not v1)

v1.x: per-repo locks, content-based secret scanning, `doctor`, `tree`, sparse checkout and size budgets, `--detach` add, CI release builds for Windows/macOS/Linux, Windows CI.
Later: embeddings table plus rank fusion, MCP wrapper (second binary), tarball fetch for public repos, tree-sitter chunking, automatic updates.

## 10. Open items

- Confirm no indexed repo is too sensitive for Claude to see; if so, add a per-repo exclusion setting.
- Decide whether macOS/Linux CI builds move into v1.
- Name the final install location for `rag.exe` (`cargo install --path` to a stable directory; install while no agent is running it) and set a cold-start budget (target: search p95 under 200 ms on Windows) measured at step 4.

## 11. Pre-commit fixes from the scaffold review

Crate split into rag-core / rag-infra / rag-cli; exact 1.99.1 pin and `rust-version`; `.gitattributes` (`* text=auto eol=lf`, golden files `-text`); `[workspace.dependencies]`, `[workspace.lints]`, `[profile.release]` (lto, codegen-units=1, strip); `scripts/check.ps1`; slim architecture test as in section 7; `.gitignore` tidy (scope `*.db`, drop redundant lines, add editor dirs, `.env*`); commit `Cargo.lock`. Test conventions: fakes live in a `testkit` feature (visible to integration tests); git test fixtures set `user.name`, `init.defaultBranch`, `core.autocrlf=false`; temp-dir cleanup clears read-only attributes on Windows; the chunker defines CRLF behaviour; a minimal search-during-update test arrives with locking in step 5. Also step 3 indexes local paths only through git (committed HEAD).
