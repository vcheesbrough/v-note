#!/usr/bin/env bash
# Exercise scripts/release-version.sh against a local bare repository standing in
# for GitHub, with real git — no network, no token.
#
# The release tag decides which images get built, tagged, verified and deployed,
# and a deployment refuses any commit it does not find a tag on. A regression here
# would surface as images overwriting each other or a deploy of the wrong build,
# so every rule the script enforces gets a case: the pipeline-number patch,
# reuse of an existing tag (annotated or lightweight, full-SHA match only), the
# line check against Cargo.toml, and push-tag's idempotence and immutability.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$ROOT/scripts/release-version.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAILURES=0
check() {
  if [ "$1" = ok ]; then
    echo "ok   $2"
  else
    echo "FAIL $2"
    FAILURES=$((FAILURES + 1))
  fi
}

export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
unset GITHUB_TOKEN

REMOTE="$WORK/remote.git"
git init -q --bare "$REMOTE"
CLONE="$WORK/clone"
git init -q "$CLONE"
cd "$CLONE"

commit() {
  printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "%s"\nedition = "2024"\n' "$1" > Cargo.toml
  echo "$RANDOM" > file
  git add -A && git commit -qm "$2"
  git push -q "$REMOTE" HEAD:refs/heads/main
  git rev-parse HEAD
}

# run MODE SHA PIPELINE — leaves output in $OUT and status in $RC.
run() {
  set +e
  OUT=$(CI_COMMIT_SHA="$2" CI_PIPELINE_NUMBER="$3" CI_REPO=owner/repo \
    RELEASE_REMOTE="$REMOTE" sh "$SCRIPT" "$1" 2>&1)
  RC=$?
  set -e
}

remote_tag_commit() {
  git ls-remote --tags "$REMOTE" | awk -v t="refs/tags/$1^{}" '$2 == t { print $1 }'
}

echo "==> a fresh commit takes its pipeline number as the patch"
A=$(commit 0.67.0 a)
run compute "$A" 465
check "$([ "$RC" -eq 0 ] && echo ok)" "compute succeeds (rc=$RC: $OUT)"
check "$([ "$(cat .release-tag)" = 0.67.465 ] && echo ok)" "tag is 0.67.465 (got $(cat .release-tag))"
check "$([ "$(cat .release-tag-reused)" = false ] && echo ok)" "not reused"

echo "==> push-tag publishes an annotated tag on the commit"
run push-tag "$A" 465
check "$([ "$RC" -eq 0 ] && echo ok)" "push-tag succeeds (rc=$RC: $OUT)"
check "$([ "$(remote_tag_commit 0.67.465)" = "$A" ] && echo ok)" "remote 0.67.465 is annotated and peels to the commit"

echo "==> push-tag again is a no-op"
run push-tag "$A" 465
check "$([ "$RC" -eq 0 ] && echo ok)" "second push-tag succeeds (rc=$RC)"
check "$(echo "$OUT" | grep -q 'already exists' && echo ok)" "and says the tag already exists"

echo "==> a deployment or restart of a tagged commit reuses its tag"
run compute "$A" 470
check "$([ "$(cat .release-tag)" = 0.67.465 ] && echo ok)" "pipeline 470 still gets 0.67.465 (got $(cat .release-tag))"
check "$([ "$(cat .release-tag-reused)" = true ] && echo ok)" "reused"

echo "==> two pipelines of different commits never share a tag"
B=$(commit 0.67.0 b)
run compute "$B" 466
check "$([ "$(cat .release-tag)" = 0.67.466 ] && echo ok)" "the next pipeline gets 0.67.466"

echo "==> a tag name already on another commit is refused"
printf '0.67.465' > .release-tag
run push-tag "$B" 466
check "$([ "$RC" -ne 0 ] && echo ok)" "push-tag of a taken name fails (rc=$RC)"
check "$([ "$(remote_tag_commit 0.67.465)" = "$A" ] && echo ok)" "and the existing tag did not move"

echo "==> a commit already tagged refuses a second tag"
printf '0.67.999' > .release-tag
run push-tag "$A" 999
check "$([ "$RC" -ne 0 ] && echo ok)" "push-tag of a different tag on a tagged commit fails (rc=$RC)"
check "$(echo "$OUT" | grep -q 'already tagged 0.67.465' && echo ok)" "and names the existing tag"

echo "==> a lightweight tag on the commit is found too"
C=$(commit 0.67.0 c)
git tag 0.67.480 "$C" && git push -q "$REMOTE" refs/tags/0.67.480
run compute "$C" 481
check "$([ "$(cat .release-tag)" = 0.67.480 ] && echo ok)" "lightweight 0.67.480 reused (got $(cat .release-tag))"

echo "==> only the full SHA matches, never a prefix"
D=$(commit 0.67.0 d)
fake="${D:0:7}$(printf '0%.0s' {1..33})"
printf '%s\trefs/tags/0.67.490\n' "$fake" > "$WORK/ls"
mkdir -p "$WORK/bin"
cat > "$WORK/bin/git" <<STUB
#!/bin/sh
for a in "\$@"; do [ "\$a" = ls-remote ] && { cat "$WORK/ls"; exit 0; }; done
exec $(command -v git) "\$@"
STUB
chmod +x "$WORK/bin/git"
PATH="$WORK/bin:$PATH" run compute "$D" 491
check "$([ "$(cat .release-tag)" = 0.67.491 ] && echo ok)" "a tag on a commit sharing 7 characters is ignored (got $(cat .release-tag))"

echo "==> a tag from another line is refused"
E=$(commit 0.68.0 e)
git tag -a 0.67.495 "$E" -m x && git push -q "$REMOTE" refs/tags/0.67.495
run compute "$E" 496
check "$([ "$RC" -ne 0 ] && echo ok)" "commit tagged 0.67.x under a 0.68 manifest fails (rc=$RC)"

echo "==> several release tags on one commit are refused"
F=$(commit 0.67.0 f)
git tag 0.67.500 "$F" && git tag 0.67.501 "$F"
git push -q "$REMOTE" refs/tags/0.67.500 refs/tags/0.67.501
run compute "$F" 502
check "$([ "$RC" -ne 0 ] && echo ok)" "two tags on one commit fail (rc=$RC)"

echo "==> bad inputs fail before writing anything"
G=$(commit 0.67.0 g)
rm -f .release-tag
run compute "$G" abc
check "$([ "$RC" -ne 0 ] && [ ! -e .release-tag ] && echo ok)" "non-numeric pipeline number fails (rc=$RC)"
printf '[package]\nversion = "0.1.0"\n' > Cargo.toml
run compute "$G" 510
check "$([ "$RC" -ne 0 ] && echo ok)" "no [workspace.package].version fails (rc=$RC)"
git checkout -q Cargo.toml
run compute "$G" ""
check "$([ "$RC" -ne 0 ] && echo ok)" "missing pipeline number fails (rc=$RC)"
rm -f .release-tag
run push-tag "$G" 510
check "$([ "$RC" -ne 0 ] && echo ok)" "push-tag without .release-tag fails (rc=$RC)"
run nonsense "$G" 510
check "$([ "$RC" -ne 0 ] && echo ok)" "an unknown mode fails (rc=$RC)"

if [ "$FAILURES" -ne 0 ]; then
  echo
  echo "release-version tests FAILED ($FAILURES)"
  exit 1
fi
echo
echo "release-version OK"
