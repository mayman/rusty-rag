## 3. Search output (schema v1)

```
{ "schema_version": 1, "query": "...", "mode": "and|or_fallback",
  "total_hits": 0, "returned": 0, "offset": 0, "has_more": false, "truncated": false,
  "results": [ { "id": "...", "rank": 1, "score": 0.0, "score_rel": 1.0,
      "repo": "host/owner/name", "commit": "abc123", "indexed_at": "...",
      "path": "src/x.rs", "lines": [10, 22], "language": "rust", "heading": "...",
      "matched_terms": ["..."], "snippet": "...",
      "show": "rag show host/owner/name src/x.rs --lines 10-40" } ],
  "hint": "present when results are empty or weak" }
```

Defaults: `-k 5`, snippet about 12 lines or 600 characters, at most 2 hits per file, hard cap about 8k characters on total output, deterministic ordering. Paths are repo-relative with `/`. Errors in JSON mode: `{"error":{"code","message","hint"}}` on stdout, plain line on stderr.
