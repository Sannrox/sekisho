# Releasing sekisho

Family versioning, tagging, and release policy:
[tenkai ADR 0040](https://github.com/Sannrox/tenkai/blob/main/docs/decisions/0040-family-versioning-and-release-policy.md).

This page is sekisho-local: how a maintainer prepares and tags **this**
repository.

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

Create an annotated, signed tag on the merged release commit:

```text
vX.Y.Z
```

Pre-releases are `vX.Y.Z-rc.N` or `vX.Y.Z-beta.N`. Hotfixes are tagged on
`release/vX.Y`, never on `main` ahead of a minor. A tag is never moved,
deleted, or reused.

## After the tag

The `v*` workflow verifies the tag, builds, tests, writes an SBOM, attests
provenance, drafts then publishes a GitHub Release from the matching
CHANGELOG section, and attaches a signed family release manifest (component,
version, git SHA, binary and image digests, attestation URLs, gate flags).
The release image is tagged with the version only. There is no `latest` tag.
Deploy the image by digest.

Pull requests and `workflow_dispatch` dry-run the same packaging path and
never mint attestations or a GitHub Release. Image builds run only on tag
pushes.

Allowed tag-signing keys live in `security/release-tag-fingerprints` with the
public key in `security/release-tag-key.asc`.
