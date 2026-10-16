---
name: ddd-onion-rust
description: Domain-driven design with onion architecture for the rag Rust project. Use when creating or changing any module, type, trait, use case, or adapter in rag-core or rag-cli, deciding which layer code belongs in, naming domain concepts, or checking the dependency rule.
---

# DDD + Onion architecture for `rag`

Follow `PLAN.md` for behavior. This skill governs how the code is structured. Always pair it with the `tdd-rust` skill: tests come first.

## The dependency rule

Dependencies point inward only. Inner layers never know outer layers exist.

```
presentation (rag-cli)  ->  infrastructure  ->  application  ->  domain
```

| Layer | Location | May depend on | Contains |
|---|---|---|---|
| Domain | `rag-core/src/domain/` | std only, plus one pure hashing crate for ids (no serde, no I/O crates, no clock, no filesystem) | Entities, value objects, aggregates, domain services, domain errors |
| Application | `rag-core/src/application/` | domain, serde | Use cases (one per command), request/response DTOs, **all port traits** (repositories and external capabilities), application errors, in-memory fakes behind a `testkit` feature |
| Infrastructure | `rag-infra/` (separate crate) | rag-core | SQLite/FTS5 store, git subprocess client, filesystem walker (`ignore`), lock, system clock, config and data directory (`RUSTY_RAG_HOME`, `dirs`). Implements the ports |
| Presentation | `rag-cli/` (binary crate) | rag-core, rag-infra (composition root only) | clap parsing, JSON/human rendering, exit codes, wiring (the composition root) |

Cargo enforces the crate boundaries: `rag-core` cannot depend on `rusqlite`, `clap`, `ignore`, or `dirs`. Inside `rag-core`, a small architecture test and `clippy.toml` bans enforce domain/application separation. Time is a domain value (`IndexedAt` as epoch seconds); only the `Clock` port supplies "now".

Rules to enforce:
- Domain code has no `use` of `rusqlite`, `std::fs`, `std::process`, `std::time::SystemTime::now`, `serde`, or `clap`. Time, randomness, and I/O enter through ports.
- Use cases take their ports as constructor arguments (trait objects or generics). They never construct infrastructure.
- Only `rag-cli` wires concrete adapters to use cases. Nothing in `rag-core` prints or calls `process::exit`.
- Infrastructure errors are translated to application/domain errors at the adapter boundary. No `rusqlite::Error` or `io::Error` crosses into inner layers.
- Add a test (or `cargo` lint script) that fails if `domain/` or `application/` import forbidden crates or modules. Treat a violation as a failing build.

## Bounded contexts and ubiquitous language

Use these names in code, tests, and docs. Do not invent synonyms (no "repo" vs "source" vs "project" drift).

**Catalog** (what is registered)
- `Source` (aggregate root): a registered repository. Identity `SourceId` (slug + short hash of canonical URL). Holds `RepoUrl`, `RefName`, `SourceStatus` (cloning, indexing, ready, failed), `IndexedCommit`, `IndexedAt`.
- Value objects: `RepoUrl` (normalized host/owner/name, ssh == https), `SourceName`, `CommitSha`.
- Invariants live here: e.g. a `Source` cannot become `ready` without an `IndexedCommit`; only a `ready` source is searchable.

**Indexing** (turning files into searchable chunks)
- `Manifest` (value object): map of `RepoPath` to `BlobOid`. `Manifest::diff` yields added/changed/removed paths.
- `Document` (a file at a path), `Chunk` (entity, identity `ChunkId` = hash of source id, path, heading path or ordinal, text hash), `LineRange`, `Language`, `Heading`.
- Domain services: `Chunker` (markdown by heading, code by line windows), `IdentifierSplitter` (camelCase, snake_case, paths; keeps the original token), `FileFilter` policy (skip lockfiles, minified, binaries, secret filenames, size cap).
- Versions are domain concepts: `TokenizerVersion`, `ChunkerVersion`, `SchemaVersion`.

**Retrieval** (answering queries)
- `Query` (value object, validated and normalized through the same `IdentifierSplitter`), `QueryMode` (And, OrFallback), `SearchFilters` (repos, languages, path glob), `Page` (limit, offset).
- `Hit` (chunk reference + score + matched terms), `SearchResult` (hits, totals, mode used), `Snippet`, `RelativeScore`.
- Domain policies: AND-first with OR fallback, per-file hit cap, snippet bounding, output budget.

**Safe file access**
- `RepoPath` is a value object that cannot be constructed from `..`, absolute paths, drive/UNC forms, or NTFS alternate streams. Path safety is enforced by construction in the domain, not by checks scattered in adapters.

## Ports (traits) and who implements them

All ports are defined in the application layer, as traits, and implemented in `rag-infra`.

| Port | Defined in | Implemented by |
|---|---|---|
| `SourceRepository` (load/save/list/delete sources) | application | SQLite adapter |
| `ChunkStore` (write chunks, manifest, FTS in one unit of work) | application | SQLite adapter |
| `ChunkSearcher` (query to scored chunk refs) | application | SQLite FTS5 adapter (a future vector adapter plus rank fusion implements the same trait) |
| `GitClient` (clone, fetch, ls-tree, cat-file) | application | subprocess adapter around system `git` |
| `FileReader` / `WorkTreeWalker` | application | filesystem + `ignore` crate adapter |
| `WriterLock` | application | lock-file adapter |
| `Clock` | application | system clock adapter; fixed clock in tests |

## Use cases (application layer)

One struct per command, each with a single `execute(request) -> Result<response, AppError>`: `AddSource`, `UpdateSource`, `RemoveSource`, `ListSources`, `Search`, `ShowFile`, `Reindex`. Request/response types are plain serde structs and are exactly what the CLI prints (JSON schema v1 is frozen).

## Working method

1. Identify the concept. Is it a rule about the business (domain), an orchestration step (application), or a technical detail (infrastructure)?
2. Put it in the innermost layer where it can live without outward dependencies.
3. Model with types: newtypes for ids and paths, enums for statuses, `Result` with typed errors. Make illegal states unrepresentable.
4. Keep aggregates small. Change one aggregate per use case; load and save through its repository.
5. Prefer pure functions and immutable value objects in the domain.
6. When unsure which layer, ask: "could this compile and be tested with no SQLite, no git, and no filesystem?" If yes, it belongs in domain or application.

## Anti-patterns to reject

- Domain types deriving `Serialize` just for output (use application DTOs and map).
- A use case calling `Command::new("git")` or `rusqlite` directly.
- Adapters containing business rules (query fallback, hit caps, skip rules) instead of calling domain services.
- "Manager"/"Helper"/"Utils" modules; name things by domain concept.
- Shared mutable global state; hidden clock or environment reads in inner layers.
- Leaking SQL, FTS5 syntax, or git output formats outside their adapter.

## Review checklist before finishing a change

- [ ] Every new type is in the layer its dependencies allow.
- [ ] No forbidden imports in `domain/` or `application/` (the architecture test passes).
- [ ] Names match the ubiquitous language above.
- [ ] New external capability has a port, a real adapter, and an in-memory fake used by tests.
- [ ] Errors are translated at adapter boundaries.
- [ ] `rag-cli` contains no business logic, only parsing, wiring, and rendering.
