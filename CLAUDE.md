# Pythia

## Releases

A release is a commit and a tag on master. `.github/workflows/release.yml`
builds the packages for the tag and leaves a **draft** release whose body is
only GitHub's generated "Full Changelog" link: Pythia has no pull requests
for it to list. The release isn't done until its body says what changed.
Never publish the generated body alone.

1. On a branch from master, commit `Release X.Y.Z`:
   - `version` in the workspace `Cargo.toml`, then `cargo update -w` for
     `Cargo.lock`;
   - in `CHANGELOG.md`, `## [X.Y.Z] - YYYY-MM-DD` under `## [Unreleased]`,
     and the compare links at the bottom (`[Unreleased]` from `vX.Y.Z`, a
     new `[X.Y.Z]` line);
   - `<release version="X.Y.Z" date="YYYY-MM-DD"/>` first among the
     releases in `assets/linux/com.ortic.pythia.metainfo.xml`.
2. Fast-forward master to it, push master, then
   `git tag -a vX.Y.Z -m "Pythia X.Y.Z"` and push the tag.
3. While the workflow runs (about 25 minutes, `gh run watch <id>`), write
   the release notes from the version's CHANGELOG section:
   - `## Highlights`: the most important changes, a bold lead each, with
     their issue numbers;
   - `## Added`, `## Changed`, `## Fixed`: the section's entries, each
     with a bold lead. When Fixed runs to dozens of entries, group them by
     area under bold headings, one line per fix with its issue numbers,
     and link `CHANGELOG.md` at the tag for the details;
   - last, `**Full Changelog**: https://github.com/ortic/pythia/compare/vPREV...vX.Y.Z`.

   Every issue number of the CHANGELOG section appears in the notes:
   compare `grep -o '#[0-9]*'` of both. v0.10.0 (short) and v0.11.0
   (grouped fixes) show the form.
4. Once every job of the run has succeeded, put the notes on the draft
   and publish it: `gh release edit vX.Y.Z --notes-file notes.md
   --draft=false --latest`. Check with `gh release view vX.Y.Z` that the
   body is the notes and the packages (21 files) are attached.
