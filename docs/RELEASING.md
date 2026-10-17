# Releasing `rag`

Releases are built and published by `.github/workflows/release.yml`. Nothing is built or published by hand. The tests and lint checks run in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) (Windows, macOS and Linux), and the release workflow runs that same workflow before it builds anything.

## Before you tag

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml` (for example `0.2.0`).
2. Update `Cargo.lock` so it records the new version: run `cargo update -w` (or any `cargo build`). Commit `Cargo.toml` and `Cargo.lock` together. The `verify` job runs `cargo metadata --locked`, which fails when `Cargo.lock` is out of date, so a missing lock update stops the release before any build.
3. Merge the change to `main`, then wait until the **CI** workflow is green on `main` (Actions, then `ci`, on all three systems). Create the tag only after that. A tag on a commit that CI has not passed would publish a release that was never tested.

## Cut a release

1. Tag the commit with `v` plus that exact version, then push the tag:

   ```powershell
   git tag v0.2.0
   git push origin v0.2.0
   ```

   Pre-releases use a hyphen, for example `v0.2.0-rc.1`. The release is then marked as a pre-release.

2. The release workflow runs these jobs in order:
   - `verify`: checks that the tag equals `v` + the workspace version and that formatting is clean. On a mismatch it fails with a message naming both values, and no build runs.
   - `ci`: runs the CI workflow (fmt on Linux, clippy and tests on Windows, macOS and Linux). A failure here stops the release.
   - `build`: builds every target after `verify` and `ci` both pass, and smoke-tests the native targets.
   - `release`: creates a **draft** GitHub Release with generated notes, uploads the archives and `SHA256SUMS`, checks that the draft has exactly the expected assets, and then publishes it with `gh release edit --draft=false`.

   A draft is not visible to other people. The release becomes public only in the last step, so a failure before that leaves a draft and nothing published.

3. Check the release page (assets and notes), then you are done.

Do not move or re-point a tag that has already been published. Cut the next version instead.

## When a run fails

- **Failed before `release`** (`verify`, `ci` or `build`): fix the problem and re-run the failed jobs from the Actions page. Nothing was published, so the same tag can be re-run. A tag that never reached a published release may be deleted and re-created on the fixed commit (`git push --delete origin v0.2.0`, then `git tag -d v0.2.0`). This is the only case where a tag may be replaced.
- **Failed in `release` while the release is a draft**: re-run the failed job. The workflow resumes the draft: it replaces its assets (`--clobber` is used only for a draft), checks the asset list, and publishes. Build artifacts are kept for the default retention period (90 days), so a re-run can still download them.
- **The draft has the wrong assets** (the asset check fails): delete the draft with `gh release delete v0.2.0 --yes`. This keeps the tag. Then re-run the job.
- **A published release already exists for the tag**: the `release` job fails with a clear message. This workflow never changes a published release. Cut the next version.

## Dry run (no release)

Actions, then `release`, then **Run workflow**, choosing a branch. A dry run runs the CI workflow, then builds every target and uploads the archives as workflow artifacts named `rag-<target>`. The `build` jobs wait for CI, so a dry run also reports CI failures. A dry run does not create a release or a draft, does not check the version against a tag, and publishes nothing. The `release` job is skipped. Use a dry run to test a workflow change before you tag.

## Assets

For workspace version `0.2.0`, the release contains:

| Asset | Contents |
|---|---|
| `rag-0.2.0-x86_64-pc-windows-msvc.zip` | `rag.exe`, `README.md` |
| `rag-0.2.0-aarch64-apple-darwin.tar.gz` | `rag`, `README.md` |
| `rag-0.2.0-x86_64-apple-darwin.tar.gz` | `rag`, `README.md` |
| `rag-0.2.0-x86_64-unknown-linux-gnu.tar.gz` | `rag`, `README.md` |
| `rag-0.2.0-aarch64-unknown-linux-gnu.tar.gz` | `rag`, `README.md` |
| `SHA256SUMS` | SHA-256 of every archive above |

There is no `LICENSE` file yet (the license is a maintainer decision that is still pending), so the archives do not include one. The packaging step copies `LICENSE` into each archive if the file exists, so adding the file later needs no workflow change.

Each archive also has a `<archive>.sha256` file. These are workflow artifacts only; they are not attached to the release.

## Supported targets

| Target | Built on | Checks |
|---|---|---|
| `x86_64-pc-windows-msvc` | `windows-latest` | CI tests; smoke test (`rag --version`, `rag list`) |
| `aarch64-apple-darwin` | `macos-latest` (arm64) | CI tests; smoke test |
| `x86_64-apple-darwin` | `macos-latest`, cross-compiled | none: not run anywhere |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | CI tests (on `ubuntu-latest`); smoke test |
| `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | CI tests (on `ubuntu-latest`); smoke test |

The release build for each Linux target runs on a different image from the CI tests. The CI tests on `ubuntu-latest` and `macos-latest` (arm64) do not cover the Intel Mac build at all.

## glibc requirements (Linux)

- `x86_64-unknown-linux-gnu` is built on Ubuntu 22.04 (glibc 2.35). It runs on glibc 2.35 or newer.
- `aarch64-unknown-linux-gnu` is built on Ubuntu 24.04 arm (glibc 2.39). It runs only on glibc 2.39 or newer, which means Ubuntu 24.04 or later on that architecture.

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

`rag` shells out to the system `git` for `add` and `update`. `git` must be installed and on `PATH` on the machine that runs `rag`, at version 2.38 or newer (`rag` asks git `rev-parse --show-object-format` when it reuses a checkout).

## Not covered

- Linux musl or fully static binaries.
- Windows arm64.
- Code signing and notarisation. The binaries are unsigned. Windows may show a SmartScreen warning (choose *More info*, then *Run anyway*). macOS may refuse to open a downloaded binary. If Gatekeeper blocks it, remove the quarantine attribute after extracting:

  ```sh
  xattr -d com.apple.quarantine ./rag
  ```

- Build provenance attestation. This is a documented follow-up, not set up. It would use `actions/attest-build-provenance`, which needs the `id-token: write` and `attestations: write` permissions added to the release job.
- Running the x86_64 macOS binary in CI. It is cross-compiled and never executed by the workflow.
- Package managers and installers (Homebrew, winget, apt, and the like).

## Known risks before the first release

- **macOS and Linux tests are new.** The test suite was developed on Windows. The CI run on macOS and Linux may find platform failures (for example `taskkill`, `.exe` names or Windows paths in tests). Because `build` waits for `ci`, those failures block the release until they are fixed.
- **Draft detection is untested.** The release job checks for an existing release with `gh release view --json isDraft`. If that command cannot see a draft, the job treats the release as new, and `gh release create` then fails with "already exists". Nothing is published in that case. Confirm on the first tag push.
- **Runner availability.** `ubuntu-24.04-arm` may not start for a private repository unless the account has larger runners. `ubuntu-22.04` is an older image that GitHub retires on a schedule; if it is no longer offered, the x86_64 Linux build must move to another image and the glibc note above must be updated. A dry run shows both.

## Pin the actions

The workflows use official GitHub actions at their major version tags: `actions/checkout@v4`, `actions/upload-artifact@v4` and `actions/download-artifact@v4`. These are the only third-party actions used. The only secret is the automatic `GITHUB_TOKEN`, which the release job uses for `gh` calls.

The maintainer should pin each one to a full commit SHA, keeping the version in a comment:

```yaml
uses: actions/checkout@<full-commit-sha> # v4.x.y
```

No commit SHAs are written in these files. They cannot be verified offline, so the tags are used until the maintainer pins them.
