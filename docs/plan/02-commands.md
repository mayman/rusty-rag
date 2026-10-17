## 2. Commands

```
rag add <url|path> [--name N]     clone (or register local path) and index
rag update [name]                 fetch, diff manifest, re-index changed files; no name = all
rag remove <name>
rag list                          repos, indexed commit, age, chunk count
rag search "<q>" [-k 5] [--repo R]... [--lang L]... [--path GLOB] [--offset N] [--max-chars N] [--human]
rag show <repo> <path> [--lines S-E] [--around N] [--json]
rag reindex [name]                rebuild FTS from stored chunk text (after tokenizer/chunker change)
```

`rag add .` indexes the current project (allowed). Clones are object-database only (no working tree), so content is read with `rag show`; see section 12.

Exit codes: 0 ok (including no results), 1 generic, 2 usage, 3 not found, 4 auth/network, 5 index missing (`index_missing`: run `rag add`), 6 index locked (`index_locked`: wait and retry), 7 reindex required (`reindex_required`: run `rag reindex`). The JSON error `code` string is stable per case.
