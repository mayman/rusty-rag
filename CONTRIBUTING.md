# Contributing

Thanks for helping build `rag`. This document explains how the project is organised and the rules every change follows. Read [PLAN.md](PLAN.md) (an index of `docs/plan/`) first: it is the source of truth for behaviour, commands, the JSON schema, and the build order. Set up your environment with [SETUP.md](SETUP.md).

## Ground rules

1. **Test first.** No production code without a failing test that demands it (red, green, refactor). Fix a bug by writing a test that reproduces it first.
2. **Respect the layers** (domain-driven design, onion architecture).
3. **Run the gate** before every commit: `powershell -File scripts\check.ps1`. It must exit 0. Report real output; do not claim "green" without running it.
4. **One build step per change.** Follow the order in docs/plan/08-build-order.md. Do not expand scope.
5. **Ask before adding dependencies.** The allowlist is in docs/plan/01-decisions.md. Anything else needs approval.

## Architecture

```
rag-cli  ->  rag-infra  ->  rag-core { application -> domain }
```

| Layer | Crate / module | Rules |
|---|---|---|
| Domain | `rag-core/src/domain` | Entities, value objects, policies. No I/O, no serde, no clock, no filesystem. `blake3` is allowed for ids. |
| Application | `rag-core/src/application` | Use cases (one per command), request/response DTOs, and **all port traits**. Depends on domain only. |
| Infrastructure | `rag-infra` | SQLite/FTS5, git, filesystem, lock, clock, config. Implements the ports. |
| Presentation | `rag-cli` | Argument parsing, JSON/human rendering, exit codes, wiring (the composition root). No business logic. |

Dependencies point inward only. Cargo enforces the crate boundaries, and `rag-core/tests/architecture.rs` checks the rest. Time and I/O reach the core only through ports; the core never prints or exits. Use the project vocabulary (`Source`, `Manifest`, `Chunk`, `Query`, `Hit`, `RepoPath`, ...) and avoid synonyms. Full guidance: [`.claude/skills/ddd-onion-rust/SKILL.md`](.claude/skills/ddd-onion-rust/SKILL.md).

## Testing

| What | Where |
|---|---|
| Domain and application behaviour | `rag-core` (unit tests, property tests with `proptest`) |
| Adapters, SQLite, real `git` on temp repos, search relevance | `rag-infra/tests` |
| End-to-end CLI behaviour | `rag-cli/tests` |
| Layer rules | `rag-core/tests/architecture.rs` (protected) |

- Use in-memory fakes for ports (behind the `testkit` feature of `rag-core`), and run the same contract tests against the fake and the real adapter.
- Never touch your real data directory. Set `RUSTY_RAG_HOME` to a per-test temp directory (`tempfile`).
- Tests must be deterministic and parallel-safe: no sleeping, no network, no shared global state.
- Load golden files with `include_str!` (`rag-core` bans `std::fs`).
- Do not add `#[allow]`, `#[ignore]` or `cfg(not(windows))` to make a test pass.

More detail: [`.claude/skills/tdd-rust/SKILL.md`](.claude/skills/tdd-rust/SKILL.md).

## Protected files

These files define the project's safety net and are changed only with explicit maintainer approval:

`Cargo.toml` (workspace root), `rust-toolchain.toml`, `rag-core/clippy.toml`, `rag-core/tests/architecture.rs`, `scripts/check.ps1`, `scripts/guard-protected.ps1`, `scripts/test-guard.ps1`, `scripts/update-protected.ps1`, `scripts/protected.sha256`, `.cargo/`, `.gitattributes`, `PLAN.md`, everything under `docs/plan/`, and everything under `.claude/`.

`scripts/check.ps1` verifies the hashed subset against the committed manifest `scripts/protected.sha256`. If you legitimately change one of those files, say so in the pull request and regenerate the manifest with `scripts\update-protected.ps1` after approval. `scripts\test-guard.ps1` exercises the guard that stops the engineer agent from editing protected files; run it after changing the guard.

## Working with the project agents

The `.claude/` folder contains agents and skills used to build this project with Claude Code:

| Agent | Role |
|---|---|
| `rust-engineer` | Implements one PLAN step, test-first. A hook blocks it from editing protected files. |
| `architect` | Read-only review: correctness, security, boundaries. |
| `system-design` | Read-only review: storage, failure modes, scope, operability. |
| `ux-reviewer` | Read-only review: CLI surface, JSON schema, token cost, the Claude skill. |

Review agent output with the same scepticism as any contributor: re-run the gate yourself, and check diffs for new dependencies, `#[allow]`, `#[ignore]`, and deleted or weakened tests.

## Commits and reviews

- Keep commits small and focused; the message says what changed and why.
- Update the progress checklist in `docs/plan/00-progress.md` when a step lands (`[x]` verified, `[~]` done but unverified, `[ ]` not started).
- Before merging, get a review of the diff. Reviewers should mark findings as confirmed (traced in code) or plausible, with the failing scenario.
- Do not commit secrets, `.env` files, or index databases.

## Security

Indexed content is untrusted data and may be returned to an AI assistant. Never put credentials in URLs, logs or the registry, set `GIT_TERMINAL_PROMPT=0` for git subprocesses, and treat any path from outside as hostile (`RepoPath` rejects `..`, absolute paths, drive/UNC forms and NTFS alternate streams by construction).
