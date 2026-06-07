# Crate release process

Publishing a new `taska` release to crates.io. Examples below use `v0.2.0` as the
previous release tag and `v0.3.0` as the new one — substitute the real versions.
Get the previous tag with `git describe --tags --abbrev=0`.

crates.io versions are **immutable**: once `cargo publish` succeeds you cannot
overwrite or re-upload that version. Everything below the publish step exists to
make sure the artifact is correct *before* it goes out.

## Pre-flight

0. Be on `master`, up to date, with a clean working tree and CI green on the last
   commit. Make sure you're authenticated to crates.io (`cargo login`, or
   `CARGO_REGISTRY_TOKEN` set) — otherwise the final publish fails after all the
   work.

## Review what's shipping

1. Check what has changed since the last release: `git log v0.2.0..HEAD`.
2. Check the files that changed: `git diff v0.2.0..HEAD --name-status`.
3. Read all the changed Rust files in full and make sure:
   - a) all code comments are correct and precise;
   - b) all CLI commands are well documented — every supported feature is
     discoverable via `--help`.
4. **Look for opportunities to improve the code** — abstractions that can be
   simplified, code that can be made more readable, duplication that can be
   removed. THIS IS REALLY IMPORTANT. Ask a human if you have ideas you are not
   certain about. Commit and re-test any changes you make here before continuing.

## Validate

5. Make the full gate pass cleanly (and commit any fixes):
   ```bash
   cargo clippy --all --all-features --all-targets -- -D warnings
   cargo fmt --all -- --check
   cargo test --all --all-features
   ```

## Update the changelog

6. Update `CHANGELOG.md` at the repo root (create it on the first release, using
   the [Keep a Changelog](https://keepachangelog.com/) format). From the
   `git log` review above, write a new section for the version, dated with today's
   date, grouping notable changes under `Added` / `Changed` / `Fixed` / `Removed`.
   Keep it human-readable — summarize what users care about, not raw commit
   subjects. This section is the single source of truth for the GitHub release
   notes in the last step.
   ```markdown
   ## [0.3.0] - 2026-06-04
   ### Added
   - `ta dep add` / `ta dep remove`: typed relationship edges.
   ### Changed
   - ...
   ### Fixed
   - ...
   ```
   (`CHANGELOG.md` ships in the published crate — `exclude` only drops `/docs`.)

## Bump and verify the artifact

7. Decide the new version from the review above (pre-1.0 semver: breaking changes
   bump the **minor**, features/fixes bump the **patch**). Bump `version` in
   `Cargo.toml`, then run `cargo build` so `Cargo.lock` picks up the new `taska`
   version.
8. Verify the package on a **clean** tree (do not use `--allow-dirty` — it would
   validate a tarball containing uncommitted changes you'll never tag):
   ```bash
   cargo package --list          # eyeball the included files (note: /docs is excluded)
   cargo publish --dry-run
   ```

## Commit, tag, push

9. Commit the bump and changelog (all three files) as the release commit:
   ```bash
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -F- <<'MSG'
   Release v0.3.0
   MSG
   ```
10. Tag **that** commit so the tag and the published version agree:
    ```bash
    git tag v0.3.0
    ```
11. Push the commit and the tag:
    ```bash
    git push origin master
    git push origin v0.3.0
    ```

## Publish

12. Publish from the clean, tagged tree:
    ```bash
    cargo publish
    ```

## Create the GitHub release

13. Create a GitHub release for the tag, using the new `CHANGELOG.md` section as
    the notes. Write that section to a **temporary** scratch file at
    `docs/release-notes-v0.3.0.md` — `docs/` keeps it out of the crate package
    (`exclude = ["/docs"]`), but it is still a scratch file: **never commit it,
    and delete it as soon as the release exists**. Preferred (automated, via the
    `gh` CLI):
    ```bash
    gh release create v0.3.0 --title "v0.3.0" --notes-file docs/release-notes-v0.3.0.md
    rm docs/release-notes-v0.3.0.md   # done with it — remove, don't commit
    ```
    If you'd rather have GitHub draft the notes from merged commits/PRs instead,
    use `--generate-notes` (no scratch file needed). If `gh` is unavailable or
    unauthenticated, create it manually: GitHub → **Releases** → **Draft a new
    release** → choose the existing `v0.3.0` tag → paste the changelog section →
    **Publish release** — then delete the scratch file.
