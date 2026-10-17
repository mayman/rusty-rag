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
- [x] First commit `ff93efd`; reviewed by `architect` (findings below)

**Review fixes after commit ff93efd** (privileged agent on Sonnet 5.5; verified by my own `check.ps1` and `test-guard.ps1` runs)
- [x] Dependency allowlist updated (`blake3`; dev-deps `proptest`, `tempfile`, `serde_json`, `syn`); `testkit` feature; `rag-cli` opts into workspace lints
- [x] Architecture test rewritten on `syn` (19 tests: groups, aliases, inline paths, strings/comments, vacuous-pass guards, dev-dep allowlist)
- [x] `scripts/check.ps1` hardened (stderr-safe, PATH append, `--locked`, architecture-test-ran check, lints check, deleted-test check, protected-file hash manifest)
- [x] Guard script hardened and covered by `scripts/test-guard.ps1` (103 cases, 0 failed); fail-closed hook command applied
- [x] README.md, SETUP.md, CONTRIBUTING.md added
- [x] Guard fails closed on empty/unparseable/tool-less hook input (3 new cases, 106 total, 0 failed); manifest regenerated after the approved change; gate rejected the stale manifest first, passes after
- [x] Committed (see git log)
- [x] Live-fire test: the hook fired in real `rust-engineer` runs (it blocked `check.ps1` variants, script runs and a delete); it does not guard git worktrees, so worktree lanes are reviewed by diff

**Build order (section 8)** (each step reviewed by architect and system-design, plus ux-reviewer for the CLI)
- [x] 2. Tokenizer and chunker (domain; reworked after review; commit e08e0c3)
- [x] 2b/2c/2d. Ports, DTOs, domain value objects, fakes and shared contract tests (three review rounds; doc 12)
- [x] 3. SQLite/FTS5 store (lane/sqlite), hardened after review (read-only open, query shape, unique chunk ids, per-source versions)
- [x] 4. Output layer (rendering only after the CLI wiring)
- [x] 5a. Git adapter, operation lock, config, clock (lane/git), hardened after review
- [x] 5b. Indexing use cases: add, update, remove, reindex (lane/usecases-index)
- [x] 6. Query use cases: search, show, list (lane/usecases-query)
- [x] CLI wiring and composition root; end-to-end tests run the real binary (commit 3f905c4)
- [x] Review round 3 round 1: safety and behaviour fixes (doc 12.2; commits 0f9d031, ee3640b, a192e95)
- [x] Review round 3 round 2: output schema and UX fixes (commits 0efe2e9, 9e6d0a2, cecb753)
- [x] Friendly default source names, name-race retry, `remove` lock wait, two startup races fixed (1738d89, f7dd3c8, b115738)
- [x] 7. Relevance set (34 cases, mutation-checked prior), 30 concurrency/crash/git-integration scenarios against the real binary
- [x] Ranking prior (docs over tests, query-time, before LIMIT) validated on real cargo/tokio indexes
- [x] 9. Real-world run: serde, anyhow, tokio, cargo on Windows (search 16-80 ms, first add 1.4-4.2 s, 38 MB peak RSS)
- [x] SKILL.md rewritten and installed to `~/.claude/skills/rag/`; `rag` installed with `cargo install --path rag-cli --locked`
- [x] Hardening round: `--deadline`, `rag doctor`, git lock handling, hints, README/SETUP/RELEASING, CI + release workflows (written, NOT yet run on GitHub)
- [x] Workspace `rust-version` raised to 1.89

**Open before a public release**
- [ ] LICENSE file (maintainer's choice)
- [ ] First GitHub run: CI on macOS/Linux (tests were developed on Windows only) and a `workflow_dispatch` dry run of the release workflow
- [ ] Pin GitHub Actions to commit SHAs; consider provenance attestation and Windows/macOS signing
- [ ] 150k-chunk synthetic scale test (cold start, worst OR query)
- [ ] `rag gc` / FTS optimize; process-tree kill via job object on Windows; per-repo sensitivity exclusion (v1.1 backlog)
**Open items (section 10)**
- [ ] Sensitive-repo exclusion setting, if needed
- [ ] macOS/Linux CI builds in v1 or later
- [ ] Install location for `rag.exe`, cold-start budget
