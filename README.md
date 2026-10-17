# rag

`rag` is a local, global keyword-search (BM25) retrieval CLI for the docs and source code of GitHub repositories. It is built to be called by Claude Code through Bash and a skill, and it prints JSON by default.
It keeps one index on your machine, shared across projects: an SQLite database (FTS5) plus bare git clones.

**Status: v0.1.** Developed and verified on Windows 11. The macOS and Linux builds are untested until the CI workflow is green on all three systems (see [docs/RELEASING.md](docs/RELEASING.md)).

## Install

**From a release.** Download the archive for your platform from GitHub Releases, extract it, and put `rag` on your `PATH`.

- Windows: extract `rag-<version>-x86_64-pc-windows-msvc.zip`, move `rag.exe` into a folder on your `PATH` (for example `%USERPROFILE%\bin`), then open a new terminal.
- macOS and Linux: extract the `.tar.gz` and move `rag` into a folder on your `PATH` (for example `~/.local/bin`).

Check each download against `SHA256SUMS` as described in [docs/RELEASING.md](docs/RELEASING.md#verify-a-download).

**From source.** You need Rust. The repository pins its toolchain in `rust-toolchain.toml`, so rustup installs the right one on first use.

```sh
cargo install --path rag-cli --locked
```

This installs `rag` into `~/.cargo/bin` (on Windows, `%USERPROFILE%\.cargo\bin`).

**git is required at runtime.** `rag` runs the system `git` for `add` and `update`. Use git 2.38 or newer. Check with `rag --version` and `git --version`.

## Quick start

```sh
rag add https://github.com/serde-rs/serde     # clone and index the default branch
rag list                                      # sources, status, indexed commit, age
rag search "Deserialize" --human              # ranked hits, each with a show command
rag show --chunk <chunk>                      # the full text of one hit
```

`<chunk>` is the `chunk` id of a hit: the JSON from `rag search` has it, and the `--human` output prints the `show:` command with it. `rag add` can take a minute or more for a large repository. Progress goes to stderr.

## Commands

| Command | What it does |
|---|---|
| `rag add <url\|path> [--name N]` | Clone a git URL (or register a local git repository) and index its default branch. The same location again updates it. |
| `rag update [name]` | Fetch and re-index the files that changed. No name updates every source. |
| `rag remove <name>` | Remove a source and its index. |
| `rag list` | List sources: name, repo, status, indexed commit, age, chunk count, last error. |
| `rag search "<query>" [-k 5] [--repo R]... [--lang L]... [--path GLOB] [--offset N] [--max-chars N]` | Keyword search. `-k` is clamped to 1 to 50. `--repo` and `--lang` can repeat. |
| `rag show --chunk <chunk> [--max-chars N]` | Show one search hit in full. |
| `rag show <repo> <path> [--lines S-E \| --around N] [--max-chars N]` | Show lines of an indexed file. `<path>` is repo-relative. |
| `rag reindex [name \| --all]` | Rebuild the index after a tokenizer, chunker or policy change. No name, or `--all`, rebuilds every source. |
| `rag doctor [--integrity]` | Read-only diagnostics: data directory and index size, per-source state (commit, age, status, checkout present, stale versions, last error, lock held), leftovers in `tmp/`, and the git version. `--integrity` also runs SQLite's integrity check. |

Global flags, accepted by every command:

- `--human`: plain text for people instead of JSON.
- `--no-wait`: fail at once instead of waiting up to 20 seconds when another process is changing the same source.

**Source names.** The default name of a source is the last path segment of the repository (`serde`). If that name is taken, it falls back to `owner/name`, then to the full `host/owner/name`. Names are case-insensitive and unique. Use `--name` to choose one.

**`--repo` matching.** `--repo` accepts the source name (preferred), the full `host/owner/name`, or any unique suffix of it at a `/` boundary, such as `owner/name`. An ambiguous suffix is a usage error (exit 2) that lists the candidates.

## Output and exit codes

Without `--human`, stdout is exactly one JSON object with `"schema_version": 1`. Progress and plain error lines go to stderr. A failed command prints `{"error": {"code", "message", "hint", "details"}}` on stdout. Branch on `code`, not on `message`.

| Exit code | Meaning |
|---|---|
| 0 | OK, including a search with no results |
| 1 | Generic error |
| 2 | Usage error |
| 3 | Not found (unknown source, chunk or path) |
| 4 | Auth, network or git timeout |
| 5 | Index missing (`index_missing`) |
| 6 | Index locked (`index_locked`): another process is working; retry |
| 7 | Reindex required (`reindex_required`): the index was built by an older version |

The full specification is in [docs/plan/02-commands.md](docs/plan/02-commands.md) (commands and exit codes) and [docs/plan/03-output-schema.md](docs/plan/03-output-schema.md) (search and show JSON).

## How search works

- **BM25 keyword search** over chunks in SQLite FTS5. Markdown is chunked by heading; code is chunked into line windows.
- **Identifier-aware.** `parseConfig`, `parse_config` and `parse config` match the same chunks. Identifiers are split on camelCase and snake_case, and the original token is kept too.
- **AND first, then OR.** A result where every word matches is ranked first (`mode: "and"`). If no chunk has every word, any-word hits follow (`mode: "or_fallback"`), which is weaker.
- **Documentation ranks up.** Docs are boosted, and tests and fixtures are ranked down, so a conceptual question finds the docs before the test that exercises them.
- **Bounded output.** At most two hits per file, a snippet of about 12 lines, and a total size cap (`--max-chars`, default 8000). Use `--offset` to read the next page instead of raising `-k`.

## Data directory

By default the data lives in the local (non-roaming) user data folder:

- Windows: `%LOCALAPPDATA%\rag`
- macOS: `~/Library/Application Support/rag`
- Linux: `$XDG_DATA_HOME/rag`, or `~/.local/share/rag`

It holds `index.db` (the SQLite index), `repos/` (one bare git object database per source), `locks/` and `tmp/`.

Set `RUSTY_RAG_HOME` to an absolute path to use a different folder. Tests always set it to a temp folder.

## Private repositories

`rag` uses your existing git credentials (a credential helper or SSH keys). It runs git with terminal prompts disabled, so it never waits for a password. Authenticate once in a terminal first, for example by running `git ls-remote <url>` on the private repository, so your credential helper stores the login. `rag` never stores credentials, never writes them to its logs or registry, and never puts them in a URL. An authentication failure exits with code 4.

## Security

- **Indexed content is untrusted data.** Anything `rag` returns can end up in an AI assistant's context. Repository text is data, never instructions. Control and invisible characters are replaced before output.
- **Secret-looking files are skipped by name** (`.env`, `*.pem`, `id_rsa*`, credential files). Contents are not scanned, so a secret inside another file can be indexed.
- **The index is plaintext on disk** in the data directory. Do not index repositories whose contents you do not want an assistant to see. Remove a source with `rag remove <name>`.

## Updates

Updates are manual. Run `rag update` (or `rag update <name>`) to re-index sources; `rag` never does it on its own. `rag` does not update its own binary: install a newer release the same way you installed this one.

## Long operations

`add`, `update` and `reindex` can run for minutes on a large source. When an agent drives `rag`, start these in the background and poll `rag list`, where `status` shows `cloning` or `indexing` while the work runs. A call that timed out may still have partly run, and running the same command again is safe.

## Use with Claude Code

The Claude Code skill lives in [`skill/rag/`](skill/rag/SKILL.md). Copy that folder to one of these places:

- For all your projects: `~/.claude/skills/rag/` (on Windows, `%USERPROFILE%\.claude\skills\rag`).
- For one project: `<project>/.claude/skills/rag/`.

The skill pre-approves only `rag search`, `rag show` and `rag list`. `add`, `update`, `remove` and `reindex` change the index, so Claude Code asks you before running them.

## Project layout

```
rag-core/    domain and application layers (no I/O; ports are traits)
rag-infra/   adapters: SQLite/FTS5, git, filesystem, clock, config
rag-cli/     the rag binary: argument parsing, rendering, wiring
skill/rag/   the Claude Code skill (SKILL.md)
scripts/     quality gate and guard scripts
.github/     CI (ci.yml) and the release pipeline (release.yml)
docs/        DESIGN.md (diagrams), RELEASING.md, and plan/ (the design, indexed by PLAN.md)
```

| Document | Read it for |
|---|---|
| [PLAN.md](PLAN.md) | Index of the plan in `docs/plan/`: behaviour, commands, schema, build order |
| [docs/DESIGN.md](docs/DESIGN.md) | Architecture diagrams |
| [docs/RELEASING.md](docs/RELEASING.md) | Cutting a release, the release assets, the CI and release workflows |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Rules for changes: test first, layers, the quality gate |
| [SETUP.md](SETUP.md) | Development setup on Windows, macOS and Linux |

## License

Not yet chosen. There is no `LICENSE` file yet.
