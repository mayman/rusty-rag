## 8a. Delegation rules

- One PLAN step per delegation to `rust-engineer`.
- Model: Haiku 5.5 (the agent default) for steps 1, 2, 4, 7, 8, 9. **Sonnet 5.5 for steps 3, 5 and 6** (query handling, crash safety and locking, path safety).
- The quality gate is `scripts/check.ps1` (rebuilds PATH from the registry, runs test, fmt --check, clippy -D warnings, prints real exit codes). The caller re-runs it and does not trust a reported "green".
- Review each diff for new dependencies, `#[allow]`, `#[ignore]`, `cfg(not(windows))`, and deleted or weakened tests.
- A hook blocks the engineer from editing `rag-core/tests/architecture.rs`, `PLAN.md`, `rust-toolchain.toml`, `clippy.toml`, the workspace `Cargo.toml`, and `.claude/**`.
- A missing `cargo` is a hard stop, never a reason to install software.
