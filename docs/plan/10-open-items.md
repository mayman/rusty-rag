## 10. Open items

- Confirm no indexed repo is too sensitive for Claude to see; if so, add a per-repo exclusion setting.
- Decide whether macOS/Linux CI builds move into v1.
- Name the final install location for `rag.exe` (`cargo install --path` to a stable directory; install while no agent is running it) and set a cold-start budget (target: search p95 under 200 ms on Windows) measured at step 4.
