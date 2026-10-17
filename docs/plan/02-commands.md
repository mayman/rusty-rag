## 2. Commands

```
rag add <url|path> [--name N]     clone (or register local path) and index; same location again = update
rag update [name]                 fetch, diff manifest, re-index changed files; no name = all
rag remove <name>
rag list                          sources: name, repo, status, commit, age, chunk count, last error
rag search "<q>" [-k 5] [--repo R]... [--lang L]... [--path GLOB] [--offset N] [--max-chars N]
rag show --chunk <chunk> [--max-chars N]
rag show <repo> <path> [--lines S-E | --around N] [--max-chars N]
rag reindex [name | --all]        rebuild after a tokenizer/chunker/policy change; no name = all (`--all` is an alias)
Global flags: --human (people-readable output), --no-wait (fail at once instead of waiting up to 20 s for a busy source)
```

`rag add .` indexes the committed HEAD of the current project (not recommended; the skill forbids it unless asked). Clones are object-database only (no working tree), so content is read with `rag show`; see section 12. The default source name is the repo's last path segment (`serde`), falling back to `owner/name`, then the slug, when taken. `--repo` accepts the source `name`, the `host/owner/name` slug, or a unique `/`-boundary suffix of the slug (names are case-insensitive and unique; ambiguous suffixes are a usage error listing candidates). stdout is always exactly one JSON object (or the `--human` text); progress goes to stderr only.

Exit codes: 0 ok (including no results), 1 generic (also an `update`/`reindex` of several sources where real failures had different codes; same code on all failures = that code), 2 usage, 3 not found, 4 auth/network/git timeout, 5 index missing (`index_missing`), 6 index locked (`index_locked`: another process is working, or the index changed under the command), 7 reindex required (`reindex_required`). The JSON error `code` string is stable per case. Sources that are busy or removed mid-run are reported as `skipped_busy` / `skipped_removed` and are not failures.
