## 4. Storage schema (SQLite)

- `meta`: schema_version, tokenizer_version, chunker_version
- `repos`: repo_id (slug + short hash of canonical URL, host-aware), url, name, ref, indexed_commit, indexed_at, status (cloning | indexing | ready | failed)
- `files`: file_id, repo_id, path, blob_oid, size, language
- `chunks`: chunk_id (hash of repo_id, path, heading path or ordinal, text hash), file_id, start_line, end_line, heading, text (raw)
- `chunks_fts`: self-contained FTS5 table (own copy of the pre-tokenized text, deleted by rowid); no external-content table
- WAL mode. Writes use `BEGIN IMMEDIATE` with a `busy_timeout`, which is the single writer lock (no second lock file for the index). Searches never take it. The writer lock is held only around the index transaction, never across a clone or fetch, so a slow clone cannot stall other callers. Git operations take a short per-repo lock.

Version gate on open: a mismatched tokenizer or chunker version returns exit code 7 (`reindex_required`) telling the agent to run `rag reindex`. Column weights in `bm25()` are pinned in code and covered by a test.
