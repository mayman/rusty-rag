# Setup

How to get a working development environment for `rag`. Windows 11 is the primary platform; macOS and Linux notes are at the end.

## Requirements

| Tool | Why | Version |
|---|---|---|
| Git | `rag` shells out to the system `git`; also used for development | 2.40+ |
| Rust (via `rustup`) | The project is written in Rust | Pinned to **1.99.1** by `rust-toolchain.toml` (rustup installs it automatically) |
| C/C++ build tools | Linker for the MSVC target, and a C compiler for the bundled SQLite | VS 2022 Build Tools, "Desktop development with C++" workload |

## Windows

Run these in PowerShell. Each command opens an installer and may prompt for administrator rights.

```powershell
winget install --id Git.Git -e
winget install --id Rustlang.Rustup -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

The Build Tools download is several gigabytes. **Open a new terminal afterwards** so the updated `PATH` is picked up (a terminal that was already open will not see `git` or `cargo`).

Verify:

```powershell
git --version
rustc --version      # rustup downloads 1.99.1 on first use in this repo
cargo --version
```

If `cargo build` fails with a linker error such as `link.exe not found`, the C++ workload is missing. Open "Visual Studio Installer", choose Modify on the Build Tools, and tick "Desktop development with C++".

## Get the code and run the checks

```powershell
git clone <repository-url> cli-rag
cd cli-rag
powershell -File scripts\check.ps1
```

`scripts\check.ps1` is the single quality gate. It verifies the protected-file hashes and the `[lints]` blocks, then runs `cargo test`, `cargo fmt --check` and `cargo clippy -D warnings` (with `--locked`), requires the architecture tests to have actually run, prints each step's real exit code, and exits non-zero if anything failed. It also adds the registry `PATH` to the process `PATH`, so it works even from a stale terminal. Run it before every commit.

Run just the binary:

```powershell
cargo run -p rag-cli -- --version
```

## Environment variables

| Variable | Purpose |
|---|---|
| `RUSTY_RAG_HOME` | Overrides the data directory (index, clones, lock file). Tests must always set this to a per-test temp directory so they never touch your real index. |

## Line endings

The repo normalises text files to LF through `.gitattributes`. If your editor insists on CRLF, let Git handle the conversion; do not commit CRLF-only changes.

## macOS and Linux

Not part of the day-to-day workflow yet, but nothing in the code is Windows-only outside the scripts.

- Install Git and Rust with `rustup` (<https://rustup.rs>).
- macOS: `xcode-select --install` for the compiler toolchain.
- Debian/Ubuntu: `sudo apt install build-essential git`.
- The `scripts\*.ps1` files need PowerShell 7 (`pwsh`) on these platforms; `cargo test`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` can be run directly.

## Troubleshooting

- **`git` or `cargo` "is not recognized"**: open a new terminal; if it persists, check `PATH` contains `C:\Program Files\Git\cmd` and `%USERPROFILE%\.cargo\bin`.
- **rustup tries to download a toolchain on every command**: the pinned toolchain is not installed yet. Run `rustup show` in the repo once and let it finish.
- **Clippy fails with `disallowed_methods`**: `rag-core` bans `std::fs`, `std::process::Command`, `SystemTime::now` and `Instant::now` on purpose. Pass time and I/O in through a port instead (see CONTRIBUTING.md). Use `include_str!` for golden files in tests.
