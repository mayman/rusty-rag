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
| Hashing | Chunk and source ids use `blake3` (pure Rust, allowed in `rag-core`); never `DefaultHasher` (unstable across releases) |
| Dependency allowlist | `rag-core` normal deps: `serde`, `blake3`. `rag-core` dev-deps: `proptest`, `tempfile`. `rag-infra`: `rusqlite` (bundled), `ignore`, `dirs`, `thiserror`, `serde_json` as needed. `rag-cli`: `clap`, `serde_json`, plus rag-core/rag-infra. Architecture test dev-dep: `syn`. Golden files via `include_str!`, no `insta` for now. Anything else needs user approval |
| Test locations | Domain/application tests in `rag-core`. Adapter, contract, and relevance tests (they need SQLite/git) in `rag-infra/tests`. End-to-end CLI tests in `rag-cli/tests`. Never make rag-core dev-depend on rag-infra |
| Local paths | `rag add <local path>` indexes only the committed HEAD of a git repo (so every hit has a commit and blob oid); uncommitted files are ignored |
| Platforms | Windows first; code kept portable for macOS and Linux (CI builds in v1.x) |
| Skill | `~/.claude/skills/rag/SKILL.md`, then MCP wrapper later |
