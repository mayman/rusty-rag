# rag: Implementation Plan

A global, local-first keyword-search (BM25) retrieval CLI in Rust, used by Claude Code through Bash calls and a skill. It indexes the default branch of public and private GitHub repos (docs and source). Embeddings and an MCP wrapper come later.

Status: design agreed through review by the `architect`, `system-design`, and `ux-reviewer` agents (see `.claude/agents/`). Scaffold committed; build step 2 in review. This file is only the index: each section lives in its own file under `docs/plan/` so a task needs to read only the parts it uses. Diagrams: [docs/DESIGN.md](docs/DESIGN.md).

| File | Section | Read it when |
|---|---|---|
| [00-progress.md](docs/plan/00-progress.md) | 0. Progress checklist | checking or updating what is done |
| [01-decisions.md](docs/plan/01-decisions.md) | 1. Decisions (incl. dependency allowlist) | adding a dependency, choosing an approach |
| [02-commands.md](docs/plan/02-commands.md) | 2. Commands and exit codes | touching the CLI surface |
| [03-output-schema.md](docs/plan/03-output-schema.md) | 3. Search output (schema v1) | touching JSON output or the skill |
| [04-storage.md](docs/plan/04-storage.md) | 4. Storage schema (SQLite) | touching the store |
| [05-data-flow.md](docs/plan/05-data-flow.md) | 5. Data flow and failure handling | add, update, search, show, remove |
| [06-indexing-rules.md](docs/plan/06-indexing-rules.md) | 6. Indexing rules | walker, filters, chunking |
| [07-module-layout.md](docs/plan/07-module-layout.md) | 7. Module layout | any code placement question |
| [08-build-order.md](docs/plan/08-build-order.md) | 8. Build order (steps 1 to 9) | picking the next task |
| [08a-delegation.md](docs/plan/08a-delegation.md) | 8a. Delegation rules | delegating to `rust-engineer` |
| [08b-phases.md](docs/plan/08b-phases.md) | 8b. Phases and parallel work | planning parallel lanes |
| [09-later.md](docs/plan/09-later.md) | 9. Later (not v1) | scope questions |
| [10-open-items.md](docs/plan/10-open-items.md) | 10. Open items | unresolved decisions |
| [11-scaffold-fixes.md](docs/plan/11-scaffold-fixes.md) | 11. Scaffold review fixes | history only |

Section numbers are stable: other documents say "PLAN section 8" meaning `docs/plan/08-build-order.md`.
