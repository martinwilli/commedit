# Releasing

A release is cut by pushing a signed tag. From that one push, CI builds the GTK
app for every target, bundles the `commedit-mcp` binaries into
`commedit-plugin.zip`, publishes a GitHub Release with all of it attached, and
deploys the plugin marketplace users register to GitHub Pages. Pushing `master`
alone triggers nothing.

## One-time repository setup

- **Pages**: *Settings > Pages > Build and deployment > Source: GitHub Actions*.
- **The `github-pages` environment** needs a **tag** rule `v*` under *Settings >
  Environments > github-pages > Deployment branches and tags*. Its default
  policy allows the `master` branch only, and the marketplace deploy runs on a
  tag: without the rule GitHub refuses it (`Tag "vX.Y.Z" is not allowed to
  deploy to github-pages due to environment protection rules`) while the release
  itself still publishes, leaving the marketplace pinned to the previous version.
- **A GPG key** whose uid matches the committer email, so `git tag -s` finds it
  without `-u`.

## Cutting a release

1. **Bump the version.** Set `[workspace.package] version` in `Cargo.toml`, run
   a build so `Cargo.lock` picks up the three crate versions, and commit both as
   `build: Bump version to X.Y.Z` (subject only, no body — see the history). The
   release build runs `cargo build --locked`, so a `Cargo.lock` left behind
   fails CI.
2. **Tag that commit, signed.** `git tag -s vX.Y.Z -F <notes>`, where the notes
   are the subject `comm(ed)it vX.Y.Z`, a blank line, `Changes since <previous
   minor>:`, and one bullet per user-visible change, wrapped at ~72 columns. A
   patch release that supersedes a minor keeps the same `Changes since` range and
   appends its bullets to that list, without mentioning the release it replaces
   (see `git cat-file -p v0.8.1`).
3. **Push, master first.** `git push origin master && git push origin vX.Y.Z`.

**The tag must point at the bump commit.** CI stamps the plugin manifest's
version from the tag name, while the three crates take theirs from `Cargo.toml`.
Tag ahead of the bump and the release ships binaries that report the previous
version: v0.11.0 did exactly that, and v0.11.1 exists only to correct it.

## What CI does

`.github/workflows/release.yml`, on `refs/tags/v*`: one `build` job per target,
then `plugin` (the zip plus the `marketplace.json` that pins it by digest), then
`release` (draft, attach every asset, flip to published, because a published
immutable release is locked) and `pages` (deploy the manifest), which are
independent of each other. The workflow's header comment carries the *why* for
each of those choices.

## Verifying afterwards

The manifest should advertise the new version and pin the release's asset:

```sh
curl -sSf https://martinwilli.github.io/commedit/marketplace.json -o /tmp/marketplace.json
jq '.plugins[0] | {version, source}' /tmp/marketplace.json
claude plugin validate --strict /tmp/marketplace.json
```

Then install it once from scratch, in a throwaway config directory so your own
`~/.claude` is untouched. This exercises the whole chain — the URL is fetched as
a manifest, the archive is downloaded through GitHub's redirect, its digest is
checked, the launcher gets its `+x` bit:

```sh
export CLAUDE_CONFIG_DIR=$(mktemp -d)
claude plugin marketplace add https://martinwilli.github.io/commedit/marketplace.json
claude plugin install commedit@commedit
claude plugin list
```
