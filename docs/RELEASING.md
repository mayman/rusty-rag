# Releasing `rag`

Releases are built and published by `.github/workflows/release.yml`. Nothing is built or published by hand.

## Cut a release

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml` (for example `0.2.0`). Commit it with the change it belongs to.
2. Tag the commit with `v` plus that exact version, then push the tag:

   ```powershell
   git tag v0.2.0
   git push origin v0.2.0
   ```

   Pre-releases use a hyphen, for example `v0.2.0-rc.1`. The release is then marked as a pre-release.
3. The `verify` job checks that the tag equals `v` + the workspace version. On a mismatch it fails with a message naming both values, and no build runs. Fix the version or the tag and push again.
4. When every build succeeds, the `release` job creates the GitHub Release for the tag with generated notes and attaches the archives and `SHA256SUMS`.

Do not move or re-point a tag that has already been published. Cut the next version instead.

If a run fails part-way, re-run the failed jobs from the Actions page. If the release already exists, the `release` job uploads its assets with `--clobber`, so a re-run replaces them rather than failing.

## Dry run (no release)

Actions, then `release`, then **Run workflow**, choosing a branch. This builds every target and uploads the archives as workflow artifacts named `rag-<target>`. It does not create a release, and it does not check the version against a tag.

## Assets

For workspace version `0.2.0`, the release contains:

| Asset | Contents |
|---|---|
| `rag-0.2.0-x86_64-pc-windows-msvc.zip` | `rag.exe`, `README.md`, `LICENSE` if the repo has one |
| `rag-0.2.0-aarch64-apple-darwin.tar.gz` | `rag`, `README.md`, `LICENSE` if present |
| `rag-0.2.0-x86_64-apple-darwin.tar.gz` | `rag`, `README.md`, `LICENSE` if present |
| `rag-0.2.0-x86_64-unknown-linux-gnu.tar.gz` | `rag`, `README.md`, `LICENSE` if present |
| `rag-0.2.0-aarch64-unknown-linux-gnu.tar.gz` | `rag`, `README.md`, `LICENSE` if present |
| `SHA256SUMS` | SHA-256 of every archive above |

Each archive also has a `<archive>.sha256` file. These are workflow artifacts only; they are not attached to the release.

Supported targets:

| Target | Built on | Smoke-tested in CI |
|---|---|---|
| `x86_64-pc-windows-msvc` | `windows-latest` | yes (`rag --version`, `rag list`) |
| `aarch64-apple-darwin` | `macos-latest` | yes |
| `x86_64-apple-darwin` | `macos-latest`, cross-compiled | no (not run) |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` (glibc 2.35 or newer) | yes |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | yes |

## Verify a download

Put the archive and `SHA256SUMS` in the same folder and compare the hash.

Linux:

```sh
grep 'rag-0.2.0-x86_64-unknown-linux-gnu.tar.gz' SHA256SUMS | sha256sum -c -
```

macOS:

```sh
grep 'rag-0.2.0-aarch64-apple-darwin.tar.gz' SHA256SUMS | shasum -a 256 -c -
```

Windows (PowerShell): compare the output with the matching line in `SHA256SUMS`.

```powershell
(Get-FileHash .\rag-0.2.0-x86_64-pc-windows-msvc.zip -Algorithm SHA256).Hash
```

## Runtime requirement

`rag` shells out to the system `git` for `add` and `update`. `git` must be installed and on `PATH` on the machine that runs `rag`.

## Not covered

- Linux musl or fully static binaries.
- Windows arm64.
- Code signing and notarisation. Windows may show a SmartScreen warning (choose *More info*, then *Run anyway*). macOS may refuse to open a downloaded binary. If Gatekeeper blocks it, remove the quarantine attribute after extracting:

  ```sh
  xattr -d com.apple.quarantine ./rag
  ```

- Running the x86_64 macOS binary in CI. It is cross-compiled and never executed by the workflow.
- Package managers and installers (Homebrew, winget, apt, and the like).

## Known gaps before relying on macOS and Linux builds

- The project's quality gate is the PowerShell script `scripts/check.ps1`, which runs on Windows. The workflow does **not** run the full test suite or clippy, and unix test coverage has not been verified.
- Recommended: add a CI workflow that runs `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` on Windows, macOS and Linux, and make it green before depending on the macOS and Linux archives. That is a separate CI decision for the maintainer.
- The Linux arm build runs on GitHub's `ubuntu-24.04-arm` runner. For a private repository, GitHub may require a larger-runner plan for it.

## Pin the actions

The workflow uses official GitHub actions at their major version tags (`actions/checkout@v4`, `actions/cache@v4`, `actions/upload-artifact@v4`, `actions/download-artifact@v4`). These are the only third-party actions used, and the only secret is the automatic `GITHUB_TOKEN`.

The maintainer should pin each one to a full commit SHA, keeping the version in a comment:

```yaml
uses: actions/checkout@<full-commit-sha> # v4.x.y
```
