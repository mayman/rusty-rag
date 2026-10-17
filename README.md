# rag

A local, global keyword-search (BM25) retrieval CLI for indexing GitHub repos (docs and source code), designed to be called by Claude Code through Bash and a skill.

> **Status: early development.** Only the workspace scaffold and the quality gate exist. `rag --version` works; the commands below are the planned v1 surface and are **not implemented yet**. Progress is tracked in the checklist in [docs/plan/00-progress.md](docs/plan/00-progress.md).

## What it will do

- Index the default branch of public and private GitHub repos (it uses your existing `git` credentials; no tokens are stored).
- Search with BM25 over chunks of docs and code, with identifier-aware matching (`parseConfig` matches "parse config").
- Return **JSON by default** (stable, versioned, size-bounded) for Claude, with a `--human` mode for people.
- Keep one global index on your machine, shared across projects.
- Stay fast and boring: one SQLite file with FTS5, system `git`, no network calls except `git clone` and `git fetch`.

## Planned commands

```
rag add <url|path> [--name N]     clone (or register a local git repo) and index it
rag update [name]                 fetch and re-index changed files; no name = all
rag remove <name>
rag list                          repos, indexed commit, age, chunk count, clone path
rag search "<query>" [-k 5] [--repo R] [--lang L] [--path GLOB] [--offset N] [--human]
rag show <repo> <path> [--lines S-E] [--around N]
rag reindex [name]                rebuild the index after a tokenizer or chunker change
```

Exit codes: `0` ok (including no results), `1` generic, `2` usage, `3` not found, `4` auth/network, `5` index missing, `6` index locked (retry), `7` reindex required.

The data directory defaults to the per-OS user data folder and can be overridden with the `RUSTY_RAG_HOME` environment variable.

## Quick start (development)

See [SETUP.md](SETUP.md) for installing the toolchain, then:

```powershell
powershell -File scripts\check.ps1     # test + fmt + clippy, with real exit codes
cargo run -p rag-cli -- --version
```

## Repository layout

```
rag-core/    domain + application layers (no I/O; ports are traits)
rag-infra/   adapters: SQLite/FTS5, git, filesystem, clock, config
rag-cli/     the `rag` binary: argument parsing, rendering, wiring
scripts/     quality gate and guard scripts
PLAN.md      index of the plan; sections live in docs/plan/ (design, build order, progress checklist)
.claude/     project agents and skills used to build this
```

The code follows domain-driven design with an onion architecture, built test-first. See [CONTRIBUTING.md](CONTRIBUTING.md).

## Platforms

Windows is the primary development platform. The code is kept portable for macOS and Linux (per-OS data directory, `/` in output paths); release builds for those platforms are planned but not set up yet.

## Security note

Indexed repository content is untrusted data. Anything `rag` returns may end up in an AI assistant's context, and the index is stored in plain text on disk. Do not index repositories whose contents you do not want your assistant to see.
