## 5. Data flow and failure handling

- **add:** normalize URL, take writer lock, insert repo row (cloning), clone into a temp dir, rename into `repos/`, walk, filter, chunk, then one transaction writes files, chunks, FTS, commit, status=ready. Crash during clone leaves a temp dir cleaned on next run. Crash during indexing rolls back and leaves the row retryable.
- **update:** lock, fetch, compare `ls-tree` to manifest, one transaction deletes changed/removed paths and inserts new ones, bump commit. Crash rolls back to the old commit (harmless: `show` reads at the indexed commit).
- **search:** no lock, one read transaction, results stamped with commit and age.
- **show:** resolve repo and path, read via `git cat-file` at the indexed commit. Reject `..`, absolute paths, drive/UNC forms, NTFS alternate streams, and symlinks leaving the repo.
- **remove:** mark removing, delete rows, delete directory (retry, clearing read-only attributes on Windows), garbage-collect leftovers at next start.
