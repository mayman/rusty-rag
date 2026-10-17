## 7. Module layout

Domain-driven design with onion architecture, built test-first (see `.claude/skills/ddd-onion-rust` and `.claude/skills/tdd-rust`; implemented by the `rust-engineer` agent).

```
rag-core (lib)     depends on: serde + one pure hashing crate only
  domain/          Source, Chunk, Manifest, Query, Hit, RepoPath, IndexedAt, Chunker,
                   IdentifierSplitter, policies (AND/OR fallback, hit caps); no I/O, no serde
  application/     use cases (AddSource, UpdateSource, RemoveSource, ListSources, Search,
                   ShowFile, Reindex), request/response DTOs (serde), ALL port traits
                   (SourceRepository, ChunkStore, ChunkSearcher, GitClient, WorkTreeWalker,
                   Clock), a fakes/testkit feature for in-memory fakes and contract tests
rag-infra (lib)    SQLite/FTS5 adapter, git subprocess client, filesystem walker, lock,
                   system clock, config and data dir (RUSTY_RAG_HOME); implements the ports
rag-cli  (bin)    clap parsing, JSON/human rendering, exit codes, composition root
```

Dependency rule: rag-cli to rag-infra to rag-core, and inside rag-core application depends on domain only. Cargo enforces the crate boundaries (rag-core cannot depend on rusqlite, clap, ignore, dirs). A small architecture test enforces (a) `cargo metadata`: rag-core's dependencies equal the allowlist, and (b) inside rag-core, `domain` does not import `application`, serde, or std I/O/time. `clippy.toml` `disallowed-*` rules ban `std::fs`, `std::process`, `SystemTime::now`, and `Instant::now` in rag-core. Workspace lints forbid `unsafe_code` and deny `print_stdout`, `print_stderr`, and `exit` in rag-core and rag-infra.

A `Retriever` trait (query to scored chunk IDs) is defined now so vectors plus rank fusion can be added later without touching callers. Data directory per OS (`dirs` crate): `%LOCALAPPDATA%\rag`, `~/Library/Application Support/rag`, `~/.local/share/rag`; env var override.
