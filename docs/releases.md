# Testing, builds, and releases

The workflows follow Jobscout's structure: Actions Toolbox diagnostics, a test
gate before release builds, native platform archives, build provenance, and
GitHub release notes. Checks and packaging run Cargo, Bash, and jq directly.

## Workflows

| Workflow | Trigger | Result |
| --- | --- | --- |
| CI | Pull requests, pushes to `main`, manual runs, release gate | Formatting, Clippy, Rust tests, shell syntax, and CLI/MCP smoke on Linux/macOS; tests and smoke in Termux Bionic |
| Build | Pull requests, pushes to `main`, manual runs, release pipeline | Native Linux/macOS archives, extracted-binary smoke tests, and SHA-256 files |
| Actionlint | Workflow changes and manual runs | GitHub Actions syntax, expressions, and shell checks |
| Release | Manual exact version/bump or a published GitHub release | Version preparation, CI gate, attested builds, checksums, and release assets |
| Native Termux smoke | Manual run on the existing Termux ARM64 runner | Tests, isolated installation, and CLI/MCP smoke on Android |

Build produces these native targets:

| Platform | Rust target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple Silicon | `aarch64-apple-darwin` |

Linux archives use glibc from the Ubuntu 24.04 build environment. Android/Termux
continues to use the source installer; a Linux archive is not an Android binary.

## Run the checks locally

Rust with rustfmt/Clippy, Bash, jq, Git, and a C compiler are required.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
bash scripts/smoke.sh target/debug/panoptes
```

On Termux, set `CARGO_BUILD_JOBS=1` before compiling to limit memory use. Run
`actionlint` when editing workflows. The smoke script isolates its repository,
database, and provider configuration in temporary directories.

To package the current Cargo version for your host:

```sh
bash scripts/package-release.sh release
```

Each archive contains the executable, README, license, shell
completions, and a `release.json` identifying its version, target, and source
commit. Packaging validates that identity and smoke-tests the extracted binary.

## Publish a release

For the initial release, run **Actions -> Release -> Run workflow** on `main`
with `version` set to `v0.1.0`. For subsequent releases, provide an exact SemVer
tag or select `patch`, `minor`, or `major`. An exact version takes precedence.
Bumps use Actions Toolbox's `GH_NEXT_PATCH`, `GH_NEXT_MINOR`, or `GH_NEXT_MAJOR`,
matching Jobscout. Use an explicit version for the first release.

The equivalent first-release command is:

```sh
gh workflow run release.yml --repo wallentx/panoptes --ref main -f version=v0.1.0
```

For a new tag, the workflow performs this sequence:

```text
Resolve version
  -> Update Cargo.toml with cssnr/toml-action
  -> Synchronize Cargo.lock with Cargo
  -> Commit and fast-forward push to main (if the version changed)
  -> Test the exact commit
  -> Build, unpack, smoke-test, and attest all four archives
  -> Verify all archive checksums and source identities
  -> Publish the tag, release notes, and assets with softprops/action-gh-release
```

The version action edits `package.version`; it may normalize TOML formatting.
Cargo synchronizes the root package's lockfile entry while retaining locked
dependency versions. The workflow validates the resulting version before committing.
New releases must start from the default branch. The workflow uses the built-in
GitHub Actions token with write access only in the version-preparation and
publication jobs. Branch protections are respected; it never force-pushes.

If tests or builds fail after version preparation, the version commit remains
on `main`, but no new release is published. Fix the failure and rerun with the
same explicit version. Existing tags are rebuilt at their original commit;
their Cargo versions must already match. A tag that moves during a build causes
publication to fail. Existing release assets may be replaced on a successful
rerun of the same tag.

Publishing a release manually on GitHub also starts the asset pipeline. Because
that tag already exists, this path validates its Cargo version instead of
changing source. Prefer the manual workflow when the version needs updating.
SemVer prerelease tags such as `v0.2.0-rc.1` create prereleases.

## Update Homebrew

After a release, update `Formula/panoptes.rb` in
[wallentx/homebrew-tap](https://github.com/wallentx/homebrew-tap) to the new source
tag URL, SHA-256 checksum, and build commit. Cargo and the source tag now carry
the same version. The tap's macOS/Linux checks validate the formula separately;
this workflow does not write to the tap repository.
