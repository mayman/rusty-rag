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
- [ ] Live-fire test: confirm the hook blocks a protected edit inside a real `rust-engineer` run (needs a session reload to load the agent's hook)

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
