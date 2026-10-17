## 8b. Phases and parallel work

The steps in section 8 group into phases. Work inside a phase can run in parallel only after the phase before it is merged. Diagrams: `docs/DESIGN.md`.

| Phase | Steps | Parallel? | Gate to leave the phase |
|---|---|---|---|
| A. Foundation | 1, 2, then **2b**: application ports, DTOs, `RepoPath`/`SourceId`, testkit fakes | Sequential. 2b is the contract everything else builds on, so it must be reviewed and frozen first | check.ps1 green, architect review of the port signatures |
| B. Adapters and surfaces | 3 (SQLite/FTS5 store, Sonnet), 5a (git client adapter, Sonnet), 4 (output layer and exit codes), 8 (draft skill) | **Yes, 4 lanes.** Each lane touches different files and depends only on 2b | each lane green on its own; contract tests pass against the fake and the real adapter |
| C. Use cases and wiring | 5b (add/update/remove use cases, locking, timeouts), 6 (`show`, `reindex`, `list`, Sonnet) | Mostly sequential. `show` and `list` can run beside 5b | end-to-end CLI tests pass |
| D. Proof | 7 (relevance set, git integration, concurrency), 9 (real repos) | 7 and the skill polish in parallel; 9 last | relevance targets met, p95 search under 200 ms |

Rules for parallel lanes:
- Each lane runs in its own git worktree and branch, so cargo's target directory lock, the gate, and half-finished files do not collide.
- Lanes must not edit the same files. Shared files (`rag-infra/src/lib.rs`, `mod.rs` lists, workspace `Cargo.toml`) are edited by the caller at merge time, not by the lanes.
- Port or DTO changes during phase B stop the lanes: change 2b first, then rebase. Lanes never edit ports themselves.
- The caller merges lanes one at a time, re-runs `scripts/check.ps1` after each merge, and sends the architect and system-design reviews on the combined diff before the phase gate.
- New dependencies still need approval; phase B needs none (`rusqlite`, `ignore`, `dirs`, `thiserror` are already on the allowlist).
