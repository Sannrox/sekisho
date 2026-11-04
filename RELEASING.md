# Releasing sekisho

Family versioning, tagging, and release policy:
[tenkai ADR 0040](https://github.com/Sannrox/tenkai/blob/main/docs/decisions/0040-family-versioning-and-release-policy.md).

This page is sekisho-local: how a maintainer prepares and tags **this**
repository.

## Compatibility of config, CLI and JSON output

- **Config.** `sekisho.toml` may declare `version = 1`; omitted means the
  current version. A file declaring any other version is refused at load,
  so settings written for a newer schema never run ignored. A change that
  older files cannot express bumps the version in the same release as its
  migration notes. Releases before this rule reject a `version` key, so omit
  it if you may roll back to one.
- **CLI.** A flag or command is removed only after one minor release that
  still accepts it and prints a deprecation warning naming its replacement.
- **JSON output.** Commands that print JSON (`diagnostics`,
  `mitigation list`, `registry show`, and any added later) are an API:
  fields are only added in a minor; removing or renaming one is a breaking
  change listed under Breaking in `CHANGELOG.md`.

## Who cuts a tag

Agents never push `v*` tags. After the version-bump pull request merges, the
maintainer creates an annotated, signed `vX.Y.Z` tag with the registered
release-signing key. The tag must equal the crate version in `Cargo.toml`.
The workflow refuses unsigned or lightweight tags.

## Prepare

1. Choose the target version. Do not invent one in a feature pull request.
2. Draft notes with `scripts/changelog-draft.sh`. Pull requests never edit
   `CHANGELOG.md`; the release step writes the section from merged work.
3. Land the version-bump pull request. The crate version and CHANGELOG
   heading move together.

## Tag

Create an annotated, signed tag on the merged release commit on `main`:

```text
vX.Y.Z
```

A tag is never moved, deleted, or reused. The tagged commit must be on
`main`; the workflow refuses any other commit, so there are no hotfix tags on
release branches.

## Release candidates

Cut `vX.Y.Z-rc.N` (or `vX.Y.Z-beta.N`) before the final tag:

1. Land a version-bump pull request that sets the crate version to
   `X.Y.Z-rc.N` and adds a `## [X.Y.Z-rc.N]` section to `CHANGELOG.md`.
   The final `vX.Y.Z` needs its own section; it never reuses the rc notes.
2. Tag `vX.Y.Z-rc.N` on that commit.
3. The release publishes as a GitHub pre-release and is never marked latest.

A failed release candidate costs only an rc number. A final tag on a commit
whose crate version is still `X.Y.Z-rc.N` is refused.

## Start, retry, and dry-run

A `v*` tag push runs `.github/workflows/release-dispatch.yml`. It only runs
`gh workflow run release.yml --ref main -f tag=<tag>`, so the release always
runs **main's** `release.yml`, never the copy at the tagged commit.

`release.yml` refuses a tagged run that does not start from `main`. It then:

- checks out the tag and fetches the annotated tag object
- verifies the signed tag with main's `scripts/verify-release-tag.sh` and
  main's allowed keys against the tagged commit's `Cargo.toml`
- requires the tagged commit to be on `main`
- runs the release scripts, tests, packaging and the image build from that
  commit, writes it as the manifest `git_sha`, and creates the release with
  `gh release create <tag> --verify-tag`
- writes an SBOM, attests provenance, drafts the GitHub Release from the
  matching CHANGELOG section with the family release manifest (component,
  version, git SHA, binary and image digests, attestation URLs, gate flags),
  and publishes only after every asset is attached

The release image is tagged with the version only. There is no `latest` tag.
Deploy the image by digest.

To retry a failed release, fix the workflow on `main` and run it again for the
same tag:

```sh
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

A retry picks up fixes to `release.yml` and to the tag verifier and keys only.
Everything else (the release scripts, `CHANGELOG.md`, the `Containerfile` and
the sources) comes from the tagged commit, by design, so a bug there needs a
new rc or patch version.

A re-run resumes the draft release and replaces all of its assets. Every job
that attests, uploads, or publishes refuses a release that is already
published, so neither a new run nor "Re-run failed jobs" on an old one can
change it; cut a new version instead. One run per tag executes at a time.

Pull requests and `workflow_dispatch` without a tag dry-run the same
packaging path on that ref. Dry runs never build images, mint attestations,
or touch a GitHub Release.

## Provenance

Attestations record the workflow that built the artifacts, which is main's
`release.yml` at `refs/heads/main`, not the tag. The source is pinned by the
signed tag and by the manifest `git_sha`, which is the tagged commit.

Allowed tag-signing keys live in `security/release-tag-fingerprints` with the
public key in `security/release-tag-key.asc`. A tagged run uses the copies on
`main`.
