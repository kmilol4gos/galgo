# macOS release procedure

This is a maintainer runbook, **not a published release**. Do not run the publishing steps until the intended commit, tag, permissions, and release notes have been reviewed. The existing [release workflow](../.github/workflows/release.yml) starts on a pushed `v*` tag; it accepts only `vMAJOR.MINOR.PATCH` or that form followed by `-` and letters, digits, or dots.

## What the workflow actually does

On `macos-14` it checks out the tagged commit, installs Rust `1.100.0`, runs `scripts/install.sh` into a temporary binary directory, and runs `scripts/make-app.sh`. It archives `bin/` and `galgo.app` as `galgo-v0.1.0-macos-<uname -m>.tar.gz`. This is **one runner architecture**, not a universal binary. The publishing job downloads that artifact and uses `gh release`: it creates the release with generated notes, or replaces the matching asset with `--clobber` if the release already exists. It does not upload checksums, sign or notarize the app, or publish to Homebrew. The release workflow itself does not run tests. Its tag-triggered publishing also does not explicitly mark prerelease tags as prereleases.

## Before publishing v0.1.0

1. Review the commit and verify that CI and the tests you require have passed. The separate CI workflow runs on `main` pushes and pull requests, not on this tag. Check the version in the app bundle script and Cargo manifests; tagging does not change them. Confirm Rust `1.100.0` is available on the hosted runner.
2. Review the unsigned app and Gatekeeper behavior on the target macOS version. Decide whether to ship unsigned; do not imply notarization. Verify that `v0.1.0` does not already exist locally or remotely, and protect release tags in GitHub. Ensure the workflow's `contents: write` permission is allowed by repository policy.
3. From a clean, reviewed checkout, inspect local status and tags:

   ```sh
   git status --short
   git tag --list v0.1.0
   ```

   The local tag listing does **not** check the remote. Inspect the repository's remote tags and releases separately before proceeding.

## Publish — future maintainer action

The following commands create and push a tag and thereby trigger publication. They were **not run for this documentation task**:

```sh
git tag -a v0.1.0 -m "galgo v0.1.0"
git push origin refs/tags/v0.1.0
gh run list --workflow release.yml
gh release view v0.1.0
```

Watch the matching run and inspect its logs in GitHub Actions; `gh release view` only succeeds after publication. Confirm the asset's actual architecture and contents, and test the downloaded app and CLI on a clean machine. A failed build does not publish; a rerun after a release exists can overwrite its asset.

For a local checksum of the **published** asset, after confirming exactly which archive to download:

```sh
tmp="$(mktemp -d)"
gh release download v0.1.0 --pattern 'galgo-v0.1.0-macos-*.tar.gz' --dir "$tmp"
shasum -a 256 "$tmp"/galgo-v0.1.0-macos-*.tar.gz
```

Compare the downloaded file and its architecture with the actual workflow artifact. A checksum computed from a separate local build is not the release checksum. Remove the temporary directory after review. No checksum is independently authenticated by the current workflow.

## Proposed Homebrew cask

[`packaging/galgo.rb`](../packaging/galgo.rb) is an **unpublished template**, not a working distribution channel. It targets the `arm64` archive and macOS 14 or later; first confirm that the workflow actually produced that architecture. Replace the explicit SHA-256 placeholder with the checksum of the verified published archive. The cask proposes installing the app and linking all four CLI binaries. Review conflicts with existing binaries and the unsigned-app warning before testing. If the release is Intel-only, this template cannot be used unchanged.

After substituting the URL/version/checksum and reviewing the release, test the cask in an isolated macOS/Homebrew environment. The relevant Homebrew commands are `brew audit --cask --strict` and `brew install --cask`; neither publication of a tap nor installation is performed here. A future maintainer must create and maintain any tap separately. Do not use `sha256 :no_check` to bypass the missing checksum.
