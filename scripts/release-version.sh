#!/bin/sh
set -eu

# The release tag of this commit: what every image is tagged with, what the
# binaries report (V_NOTE_RELEASE) and what a deployment rolls out. Replaces
# woodpecker-plugin-release-versions (#462).
#
#   release-version.sh compute    write .release-tag and .release-tag-reused
#   release-version.sh push-tag   push .release-tag as an annotated git tag
#
# The tag is MAJOR.MINOR.PATCH: major and minor from [workspace.package].version
# in Cargo.toml (the iteration's line), the patch from $CI_PIPELINE_NUMBER.
# Woodpecker allocates pipeline numbers one at a time per repository, and every
# workflow of one pipeline sees the same number, so web, android and deploy each
# compute the same tag on their own and no two pipelines can ever compute the
# same one. The plugin this replaces took the highest patch on the remote plus
# one: two pipelines on one line got the same tag and overwrote each other's
# images.
#
# A commit that already carries a release tag keeps it ("reused"). That is how a
# deployment, or a restarted pipeline, which both have pipeline numbers of their
# own, find the tag the commit was built and tested under — and only a green push
# pipeline tags a commit, so "not reused" on a deployment means there is nothing
# tested to deploy (verify-release-images in deploy.yml refuses it).
#
# Immutability is git's own: pushing a tag name that already exists is rejected
# by the server, atomically, without --force.
#
# Environment: CI_COMMIT_SHA, CI_PIPELINE_NUMBER (compute), CI_REPO, and
# GITHUB_TOKEN for a private remote. RELEASE_REMOTE overrides the remote URL
# (the tests point it at a local bare repository); CARGO_TOML the manifest.

MODE="${1:-}"
SHA="${CI_COMMIT_SHA:?CI_COMMIT_SHA must be set}"
REMOTE="${RELEASE_REMOTE:-https://github.com/${CI_REPO:?CI_REPO must be set}.git}"
CARGO_TOML="${CARGO_TOML:-Cargo.toml}"

die() {
  echo "ERROR: $*" >&2
  exit 1
}

# The token travels as a header, never in the URL, so a git error message
# cannot print it.
git_remote() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    auth=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
    git -c safe.directory='*' -c http.extraHeader="Authorization: Basic $auth" "$@"
  else
    git -c safe.directory='*' "$@"
  fi
}

# major.minor of [workspace.package].version — the line every tag of this
# commit must be on.
cargo_line() {
  [ -f "$CARGO_TOML" ] || die "$CARGO_TOML not found"
  version=$(awk '
    /^\[/ { section = $0; next }
    section == "[workspace.package]" && $1 == "version" {
      gsub(/[" ]/, "", $0); sub(/^version=/, "", $0); print; exit
    }' "$CARGO_TOML")
  echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "$CARGO_TOML: [workspace.package].version '$version' is not MAJOR.MINOR.PATCH"
  echo "${version%.*}"
}

# The release tags pointing at this commit, one per line. An annotated tag's
# ref points at the tag object; only its peeled `^{}` line carries the commit,
# so both shapes are matched — on the full SHA, never a prefix.
tags_on_commit() {
  listing=$(git_remote ls-remote --tags "$REMOTE") || die "git ls-remote $REMOTE failed"
  echo "$listing" | awk -v sha="$SHA" '$1 == sha { print $2 }' \
    | sed -e 's|^refs/tags/||' -e 's|\^{}$||' \
    | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' | sort -u || true
}

# The one release tag on this commit, or nothing.
existing_tag() {
  found=$(tags_on_commit)
  [ "$(echo "$found" | grep -c .)" -le 1 ] \
    || die "commit $SHA carries several release tags: $(echo "$found" | tr '\n' ' ')"
  echo "$found"
}

compute() {
  line=$(cargo_line)
  tag=$(existing_tag)
  if [ -n "$tag" ]; then
    [ "${tag%.*}" = "$line" ] \
      || die "commit $SHA is tagged $tag, which is not on $CARGO_TOML's line $line.x"
    reused=true
  else
    number="${CI_PIPELINE_NUMBER:?CI_PIPELINE_NUMBER must be set}"
    echo "$number" | grep -Eq '^[0-9]+$' || die "CI_PIPELINE_NUMBER '$number' is not a number"
    tag="$line.$number"
    reused=false
  fi
  printf '%s' "$tag" > .release-tag
  printf '%s' "$reused" > .release-tag-reused
  echo "RELEASE_TAG=$tag (reused: $reused)"
}

push_tag() {
  [ -s .release-tag ] || die ".release-tag is missing or empty; run compute first"
  tag=$(cat .release-tag)
  existing=$(existing_tag)
  if [ "$existing" = "$tag" ]; then
    echo "git tag $tag already exists at $SHA"
    return 0
  fi
  [ -z "$existing" ] || die "commit $SHA is already tagged $existing; refusing to add $tag"

  git -c safe.directory='*' -c user.name=v-note-ci -c user.email=ci@v-note.invalid \
    tag -f -a "$tag" "$SHA" -m "Release $tag

commit: $SHA
branch: ${CI_COMMIT_BRANCH:-unknown}
pipeline: ${CI_PIPELINE_NUMBER:-unknown}"
  # Rejected by the server if the name exists — on another commit, that is an
  # immutability violation and must fail the step.
  git_remote push "$REMOTE" "refs/tags/$tag" || die "pushing tag $tag was rejected"
  echo "pushed git tag $tag"
}

case "$MODE" in
  compute) compute ;;
  push-tag) push_tag ;;
  *) die "usage: $0 compute|push-tag" ;;
esac
