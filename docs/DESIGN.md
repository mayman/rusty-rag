# System design

Diagrams for `rag`. PLAN.md is the source of truth for behaviour; this file is the picture. Each diagram is given twice: Mermaid (renders on GitHub and in most editors) and ASCII (renders anywhere).

## 1. Layers (onion architecture)

```mermaid
flowchart LR
    CLI["rag-cli<br/>clap, JSON/human output,<br/>exit codes, composition root"]
    INFRA["rag-infra<br/>SQLite FTS5, git client,<br/>file walker, lock, clock, config"]
    subgraph CORE["rag-core"]
        APP["application<br/>use cases, DTOs, ports"]
        DOM["domain<br/>Chunk, Chunker, Query, Hit,<br/>RepoPath, IdentifierSplitter"]
        APP --> DOM
    end
    CLI --> INFRA
    CLI --> APP
    INFRA -->|implements ports| APP
```

```
        +-----------------------------------------------+
        | rag-cli   clap | JSON/--human | exit codes    |   composition root
        +----------------------+------------------------+
                               | wires
        +----------------------v------------------------+
        | rag-infra   SQLite/FTS5 | git | walker | lock |   implements the ports
        |             clock | config (RUSTY_RAG_HOME)   |
        +----------------------+------------------------+
                               | depends on (inward only)
        +----------------------v------------------------+
        | rag-core                                      |
        |  +-----------------------------------------+  |
        |  | application: use cases, DTOs, PORTS     |  |
        |  |  +-----------------------------------+  |  |
        |  |  | domain: Chunk, Chunker, Query,    |  |  |
        |  |  | Hit, RepoPath, IdentifierSplitter |  |  |
        |  |  | (no I/O, no serde, no clock)      |  |  |
        |  |  +-----------------------------------+  |  |
        |  +-----------------------------------------+  |
        +-----------------------------------------------+
```

## 2. Components and ports

```mermaid
flowchart TB
    subgraph app["application (rag-core)"]
        UC["Use cases:<br/>AddSource, UpdateSource, RemoveSource,<br/>ListSources, Search, ShowFile, Reindex"]
        P1(["SourceRepository"])
        P2(["ChunkStore / ChunkSearcher"])
        P3(["GitClient"])
        P4(["WorkTreeWalker"])
        P5(["Clock"])
        UC --> P1 & P2 & P3 & P4 & P5
    end
    subgraph infra["rag-infra adapters"]
        A1["SQLite repos table"]
        A2["SQLite chunks + FTS5<br/>(bm25)"]
        A3["git subprocess"]
        A4["ignore-crate walker"]
        A5["system clock"]
    end
    P1 -.-> A1
    P2 -.-> A2
    P3 -.-> A3
    P4 -.-> A4
    P5 -.-> A5
    A3 --> GH[("GitHub<br/>public + private")]
    A2 --> DB[("index.db<br/>WAL")]
    A3 --> CL[("repos/ clones")]
```

```
 Use cases (application)         Ports (traits)            Adapters (rag-infra)      Outside world
 -----------------------         --------------            --------------------      -------------
 AddSource    --------+         SourceRepository  <.....   SQLite repos table  ----> index.db (WAL)
 UpdateSource --------+         ChunkStore        <.....   SQLite chunks+FTS5  ----> index.db (WAL)
 RemoveSource --------+-uses->  ChunkSearcher     <.....   FTS5 bm25 query
 ListSources  --------+         GitClient         <.....   git subprocess      ----> GitHub, repos/ clones
 Search       --------+         WorkTreeWalker    <.....   ignore-crate walker ----> working tree files
 ShowFile     --------+         Clock             <.....   system clock
 Reindex      --------+
```

## 3. Add / update flow

```mermaid
sequenceDiagram
    actor U as Claude (Bash)
    participant C as rag-cli
    participant UC as AddSource / UpdateSource
    participant G as GitClient
    participant W as Walker + Chunker
    participant S as Store (SQLite)
    U->>C: rag add <url|path>
    C->>UC: request DTO
    UC->>G: shallow clone / fetch default branch
    G-->>UC: commit + git ls-tree (blob OIDs)
    UC->>S: read manifest
    UC->>UC: diff manifest vs ls-tree
    UC->>W: changed files only
    W-->>UC: chunks
    UC->>S: BEGIN IMMEDIATE: delete changed, insert new, bump commit
    S-->>UC: committed (or rolled back, old commit stays valid)
    UC-->>C: response DTO
    C-->>U: JSON (schema v1), exit code
```

```
 rag add/update
   |
   v
 [rag-cli] parse args --> request DTO
   |
   v
 [use case] --> GitClient: shallow clone or fetch default branch (no lock held)
   |                  \--> commit id + `git ls-tree -r` (blob OIDs)
   v
 read Manifest from store --> diff against ls-tree --> changed / removed paths
   |
   v
 Walker + Chunker: changed files only (gitignore, size, binary, denylist filters)
   |
   v
 Store: BEGIN IMMEDIATE  (the single writer lock, held only here)
          delete changed + removed, insert new chunks + FTS rows, bump commit
        COMMIT  (a crash rolls back; old commit stays consistent)
   |
   v
 response DTO --> JSON --> exit code (0 ok, 3 not found, 4 auth/network, 6 locked ...)
```

## 4. Search flow

```mermaid
flowchart LR
    Q["rag search 'getUserName'"] --> SPLIT["IdentifierSplitter<br/>per word: joined OR parts"]
    SPLIT --> AND["FTS5 query:<br/>AND of all words"]
    AND -->|hits| RANK
    AND -->|none| OR["OR fallback"] --> RANK
    RANK["bm25 rank<br/>cap 2 hits per file, top k=5"] --> SNIP["Build snippet in Rust<br/>from raw chunk text"]
    SNIP --> OUT["JSON schema v1<br/>~8k char cap, show command per hit"]
```

```
 query --> IdentifierSplitter (per word: joined OR parts)
       --> FTS5 AND query --hits--> bm25 rank
                |no hits
                +--> OR fallback --> bm25 rank
       --> cap: 2 hits per file, top k (default 5)
       --> snippet built in Rust from raw chunk text (match-centred, ~600 chars)
       --> JSON schema v1, output capped near 8k chars, `show` command per hit
 (search takes no writer lock: one read transaction on the WAL database)
```

## 5. Build phases and parallel lanes

```mermaid
flowchart TB
    A1["Step 1 toolchain"] --> A2["Step 2 tokenizer + chunker"] --> A3["Step 2b ports, DTOs,<br/>value objects, fakes"]
    A3 --> B3["Step 3 SQLite/FTS5 store"]
    A3 --> B5a["Step 5a git client adapter"]
    A3 --> B4["Step 4 output + exit codes"]
    A3 --> B8["Step 8 draft skill"]
    B3 --> C5b["Step 5b add/update/remove,<br/>locking, timeouts"]
    B5a --> C5b
    B3 --> C6["Step 6 show, reindex, list"]
    B4 --> C5b
    C5b --> D7["Step 7 relevance +<br/>integration + concurrency"]
    C6 --> D7
    B8 --> D7
    D7 --> D9["Step 9 real-repo test"]
```

```
 Phase A (sequential)   1 toolchain -> 2 tokenizer/chunker -> 2b ports+DTOs+fakes  [freeze]
                                                                  |
 Phase B (4 parallel lanes, one worktree each)  ------------------+-----------------------
                          3 SQLite/FTS5 store | 5a git adapter | 4 output+exit codes | 8 draft skill
                                  |                 |                |                   |
 Phase C                          +--------+--------+----------------+                   |
                                  5b add/update/remove, locking     6 show/reindex/list  |
                                           |                              |              |
 Phase D                                   +---------------+--------------+--------------+
                                                    7 relevance + integration + concurrency
                                                                  |
                                                       9 real-repo test and tuning
```

## 6. On-disk layout

```
 RUSTY_RAG_HOME  (default: %LOCALAPPDATA%\rag | ~/Library/Application Support/rag | ~/.local/share/rag)
   index.db        SQLite, WAL: meta, repos, files, chunks, chunks_fts
   repos/<id>/     shallow clones of the default branch (read by `show` via git cat-file)
   tmp/            in-progress clones, cleaned on next start
```
