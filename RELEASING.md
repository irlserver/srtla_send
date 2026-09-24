# Releasing

Releases are tag driven. Pushing a `vX.Y.Z` tag runs `.github/workflows/release.yml`. It builds both debs through the regular build workflow and creates a draft GitHub release. Nothing is public until you publish the draft by hand.

## Steps

1. Set `version = "X.Y.Z"` in the `[package]` table of the root `Cargo.toml`. This is the one source of truth: the deb names, the deb control file and `srtla_send -v` all read it. The member crates under `crates/` keep their own versions.

2. Run `cargo build` so `Cargo.lock` picks up the new version, then commit both files:

   ```
   git commit -am "chore(srtla_send): bump version to X.Y.Z"
   ```

3. Preview the release notes:

   ```
   scripts/changelog.sh HEAD
   ```

4. Tag and push:

   ```
   git tag vX.Y.Z
   git push origin main vX.Y.Z
   ```

5. Wait for the Release workflow. If the tag and the `Cargo.toml` version disagree, it fails before it builds anything. On success it creates a draft release with these files:

   * `srtla_X.Y.Z_amd64.deb`
   * `srtla_X.Y.Z_arm64.deb`
   * `sha256sums.txt`

6. Install each deb on a real device and stream through it before you publish. At minimum, the sender registers all uplinks and forwards data to a receiver.

7. Read the release notes, edit anything that reads badly, then publish the draft.

## The changelog

`scripts/changelog.sh` groups every non-merge commit between the previous `v*` tag and the new one by its conventional commit type. `feat` goes under Features, `fix` under Bug fixes, then `perf`, `refactor`, `docs`, and `build` or `ci`. A commit with `!` or a `BREAKING CHANGE:` body goes to the top under Breaking changes, followed by the text after the marker. A subject that is not a conventional commit still appears, under Other changes.

Commit subjects are the changelog, so write them for a reader. The script prints a scope in bold ahead of the subject (`**srtla-core:** ...`) and drops the `srtla_send` scope, because most commits have it.

The release body starts with the install instructions in `.github/release-notes-header.md`.

## Fixing a bad tag

If the version check fails or an artifact is broken, delete the draft release in the GitHub UI and fix the problem. Then move the tag:

```
git tag -f vX.Y.Z
git push -f origin vX.Y.Z
```

## glibc floor

`GLIBC_FLOOR` at the top of `.github/workflows/build.yml` sets the oldest glibc the debs support. `cargo zigbuild` links against that glibc version, so the runner's newer glibc does not leak into the binary. `scripts/package-deb.sh` checks the binary's glibc symbol versions against the floor and writes it into the deb's `Depends`. Raise the floor only to drop support for older distributions.

To build a deb locally the same way CI does (needs `zig` and `cargo-zigbuild`):

```
cargo zigbuild --locked --profile release-lto --target aarch64-unknown-linux-gnu.2.27
scripts/package-deb.sh target/aarch64-unknown-linux-gnu/release-lto/srtla_send arm64 2.27 dist
```
