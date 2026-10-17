## 8. Build order

1. Toolchain: check for Visual Studio Build Tools, install rustup, scaffold workspace, pin toolchain.
2. Tokenizer and chunker with unit and golden tests.
3. SQLite store and FTS5, local-folder indexing, search with query escaping (quote every term; AND then OR).
   Step 3 must settle (from the step 2 reviews): (a) FTS5 tokenizer config. Default `unicode61` re-splits `_ - . /`, so declare `tokenchars` or emit only alphanumeric tokens, and add a round-trip contract test (`parse_config` and `parseConfig` hit the same chunk). (b) Query side uses the structured splitter per word: `(joined OR (p1 AND p2 ...))`, AND across words. (c) Snippets and `matched_terms` are built in Rust from raw chunk text (best line by token overlap, ~12 lines, 600 chars) and report the snippet's own start line. (d) Walker policy: skip files over 1 MB, NUL/binary, UTF-16, lockfiles, `*.min.*`, long-average-line files; report skipped counts in `add` output. (e) Language detection policy (path/extension to language and markdown kind). (f) Value objects `RepoPath` and repo id replace the `&str` arguments of the chunker. (g) Column layout (path, heading, body); avoid `detail=column` if quoted phrase queries are wanted.
4. Output layer: JSON schema, `--human`, error codes, output caps.
5. Git: add, update (manifest diff), remove, locking, timeouts, stderr progress.
6. `show` with path safety; `reindex`; `list`.
7. Relevance set of about 20 queries over a fixed corpus; integration tests against local git repos (rename, delete, force-push); concurrency test (search during update).
8. Skill (`SKILL.md` with triggers, query phrasing, "not for the current project's own code", untrusted-content note, `allowed-tools: Bash(rag:*)`).
9. Real-world test against 2 to 3 of your repos, tune chunking and weights.
