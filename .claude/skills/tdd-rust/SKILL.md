---
name: tdd-rust
description: Test-driven development workflow for the rag Rust project. Use whenever implementing or changing behavior: write a failing test first, make it pass minimally, then refactor. Covers test layout per onion layer, fakes for ports, golden and property tests, and the cargo commands to run.
---

# TDD for `rag` (Rust)

No production code without a failing test that demands it. Pair with the `ddd-onion-rust` skill for where code and tests live.

## The loop (Red, Green, Refactor)

1. **Red:** write one small test for the next behavior. Run it and watch it fail **for the expected reason** (a compile error for a missing type counts as the first red; then make it compile with a stub that fails the assertion).
2. **Green:** write the simplest code that passes. Resist generality you have no test for.
3. **Refactor:** with all tests green, improve names and structure, remove duplication. Run tests after every change.
4. Repeat. Commit-sized steps: one behavior per cycle.

Rules:
- Never write more of a test than is needed to fail; never write more code than is needed to pass.
- Never change a test and production code in the same step to make it green. If a test is wrong, fix the test while red/green is explicit.
- A bug fix starts with a failing test that reproduces the bug.
- If you cannot write the test first, the design is unclear; clarify the interface, then try again.
- Report honestly: show failing output when red, and the real pass count when green. Do not claim green without running the tests.

## Test pyramid and placement

| Layer | Test kind | How | Where |
|---|---|---|---|
| Domain | Fast unit tests, no I/O | Plain values, table-driven cases, property tests for invariants | `#[cfg(test)]` module next to the code |
| Application | Use-case tests | In-memory fakes of every port; assert on responses and recorded calls | `#[cfg(test)]` module or `rag-core/tests/` |
| Infrastructure | Adapter contract/integration tests | Real SQLite (in-memory or temp dir), real `git` against local temp repos (`file://`), real filesystem in temp dirs. rag-core must never dev-depend on rag-infra | `rag-infra/tests/` |
| Presentation | CLI end-to-end | Spawn the built binary with a temp data dir (`RUSTY_RAG_HOME`); assert JSON, exit codes, stderr | `rag-cli/tests/` |
| Architecture | Dependency rule | Protected test: `cargo metadata` allowlist plus a syntax-tree check of `domain/` and `application/`. Do not edit it | `rag-core/tests/architecture.rs` |
| Relevance | Search quality | About 20 queries over a fixed corpus asserting expected hits appear in the top k (needs SQLite, so it lives with the adapter) | `rag-infra/tests/relevance.rs` |

Golden files are loaded with `include_str!` (rag-core bans `std::fs` through clippy, and you must not add `#[allow]`). Fakes live behind a `testkit` feature of rag-core so `rag-infra` and `rag-cli` tests can use them.

Keep most tests at the domain and application level. Few slow tests at the edges.

## Fakes over mocks

For each port keep an in-memory fake (for example `InMemorySourceRepository`, `FakeGitClient`, `FixedClock`) in a shared `test_support` module. Fakes behave like the real thing; mocks that assert call sequences are a last resort. Run the **same contract test suite** against the fake and the real adapter so they cannot drift.

## What to test, specifically for rag

- **Tokenizer / IdentifierSplitter:** `parseConfig`, `parse_config`, `HTTPServer`, `src/foo/bar.rs`, digits, unicode, empty input. Both the split parts and the original identifier must be produced. Golden files for larger samples.
- **Chunker:** property tests: every line is covered, no overlap beyond the configured amount, chunks never exceed limits, markdown headings start new chunks, line numbers are correct, chunk ids are stable for unchanged text and change when text changes.
- **Manifest diff:** added, changed, removed, renamed (as remove + add), empty, identical.
- **Query:** escaping of reserved words and punctuation (AND, OR, NOT, NEAR, `-`, `:`, quotes) never errors; AND first then OR fallback reports the mode; empty and stop-word-only queries return a hint, not a failure.
- **Search policy:** per-file hit cap, snippet bounds, output budget and `truncated` flag, deterministic ordering, `score_rel`.
- **RepoPath:** rejects `..`, absolute paths, drive and UNC forms, alternate streams, reserved Windows names.
- **Use cases:** crash and failure paths with failing fakes (clone fails, index transaction fails): state is unchanged or retryable.
- **Git adapter:** local temp repos covering add, update after new commit, force-push, rename, delete, empty repo.
- **Store:** rollback leaves old data intact; version mismatch yields the reindex error; concurrent readers during a write.
- **CLI:** JSON matches schema v1, exit codes 0 to 5, errors as JSON, `--human` has no ANSI when piped.

## Style

- Test names state behavior: `search_falls_back_to_or_when_and_matches_nothing`.
- Arrange / Act / Assert, one logical assertion per test; use builders for test data.
- No sleeping, no network, no dependence on the real home directory or the real data dir. Use temp dirs and the env-var data-dir override.
- Tests must be deterministic and runnable in any order and in parallel.
- Use `proptest` for properties, `tempfile` for temp dirs, `insta` or hand-rolled golden files when output is large.

## Commands (run from the repo root; PowerShell)

- Single test while iterating: `cargo test -p rag-core <name_fragment>`
- Whole suite: `cargo test --workspace`
- Format and lint before finishing a task: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`

## Definition of done for any task

- [ ] Each new behavior had a test that was seen failing first.
- [ ] `cargo test --workspace` passes; output reviewed, not assumed.
- [ ] fmt and clippy are clean.
- [ ] Architecture test passes.
- [ ] Refactor step done; no dead code or unused ports left behind.
