# Setup

How to get a working development environment for `rag` and build the binary. Windows 11 is the primary platform; macOS and Linux notes are at the end.

## Requirements

| Tool | Why | Version |
|---|---|---|
| Git | `rag` shells out to the system `git`; also used for development | **2.38 or newer** (`rag` runs `git rev-parse --show-object-format` when it reuses a checkout) |
| Rust (via `rustup`) | The project is written in Rust | The toolchain is **pinned to 1.99.1** by `rust-toolchain.toml`. rustup installs it automatically on the first command in the repo. |
| C/C++ build tools | Linker for the MSVC target, and a C compiler for the bundled SQLite | VS 2022 Build Tools, "Desktop development with C++" workload |

The workspace also declares `rust-version = "1.89"` in `Cargo.toml`. That is the oldest compiler the code is meant to support. It is not tested in CI. The pinned toolchain is the one that builds and tests the project.

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
git --version        # must be 2.38 or newer
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

The GitHub workflow `.github/workflows/ci.yml` runs fmt, clippy and the tests on Windows, macOS and Linux. Its macOS and Linux runs are new and may need platform fixes.

## Build and install the binary

From the repository root:

```powershell
cargo build --release -p rag-cli
.\target\release\rag.exe --version
```

The release build is written to `target\release\rag.exe` inside the repository. You can copy that file to any folder on your `PATH`.

To install `rag` on your `PATH` in one step, use `cargo install`:

```powershell
cargo install --path rag-cli --locked
rag --version
```

`cargo install` writes `rag.exe` to `%USERPROFILE%\.cargo\bin`. rustup adds that folder to your user `PATH` by default. If `rag` is "not recognized" after installing, open a new terminal. If it still fails, check that the folder is in your user `PATH`: Settings, System, About, Advanced system settings, Environment Variables, then edit `Path` under your user variables.

To use the repository build without installing it, add `<repo>\target\release` to your user `PATH` the same way. Remember to rebuild after changes; the folder is not updated on its own.

On macOS and Linux the binary is `target/release/rag`, and `cargo install` puts it in `~/.cargo/bin/rag`.

## Environment variables

| Variable | Purpose |
|---|---|
| `RUSTY_RAG_HOME` | Overrides the data directory (index, clones, lock file). Tests must always set this to a per-test temp directory so they never touch your real index. |

## Data directory and antivirus (optional)

`rag` keeps its data in `%LOCALAPPDATA%\rag` on Windows (see the README for other systems). Windows Defender real-time scanning can slow down indexing and builds.

You may add exclusions for the repository's `target` folder and for `%LOCALAPPDATA%\rag`. This is optional and your decision. An exclusion means Defender no longer scans those files. Adding one needs administrator rights, and it is a security trade-off, so do not add it unless you want the speed.

## Line endings

The repo normalises text files to LF through `.gitattributes`. If your editor insists on CRLF, let Git handle the conversion; do not commit CRLF-only changes.

## macOS and Linux

Not part of the day-to-day workflow yet, but nothing in the code is Windows-only outside the scripts.

- Install Git (2.38 or newer) and Rust with `rustup` (<https://rustup.rs>).
- macOS: `xcode-select --install` for the compiler toolchain.
- Debian/Ubuntu: `sudo apt install build-essential git`.
- Build with `cargo build --release -p rag-cli`; the binary is `target/release/rag`. Install with `cargo install --path rag-cli --locked`.
- The `scripts/*.ps1` files need PowerShell 7 (`pwsh`) on these platforms; `cargo test`, `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` can be run directly.
- The CI workflow runs the tests on macOS and Linux for the first time (see `.github/workflows/ci.yml`). Failures there are expected to be platform issues to fix, not signs of a broken build.

## Troubleshooting

- **`git` or `cargo` "is not recognized"**: open a new terminal; if it persists, check `PATH` contains `C:\Program Files\Git\cmd` and `%USERPROFILE%\.cargo\bin`.
- **`git --version` is older than 2.38**: update Git. `rag` needs 2.38 or newer.
- **rustup tries to download a toolchain on every command**: the pinned toolchain is not installed yet. Run `rustup show` in the repo once and let it finish.
- **Clippy fails with `disallowed_methods`**: `rag-core` bans `std::fs`, `std::process::Command`, `SystemTime::now` and `Instant::now` on purpose. Pass time and I/O in through a port instead (see CONTRIBUTING.md). Use `include_str!` for golden files in tests.
