#!/bin/sh
set -eu

# Tests scripts/lane-key.sh (#467) against a scratch copy of this repository and
# a local bare remote:
#
#   - which changes move which lane's key, and which must not (a key that moves
#     too little skips tests a change can break; one that moves too much only
#     costs time, but docs-only pushes are the point of the card);
#   - the skip/mark round trip, per step, FULL_RUN, an existing marker, and an
#     unreachable remote (which must run, never skip);
#   - every path the lanes' workflow and e2e compose files reference is in that
#     lane's key or declared deliberately not an input.
#
# test-lane-key-context.sh separately holds the .dockerignore matcher to Docker.

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LANE_KEY="$ROOT/scripts/lane-key.sh"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Referenced by a lane's steps but deliberately outside its key.
NOT_INPUTS='scripts/release-version.sh'

failed=0
pass() { echo "ok: $*"; }
fail() {
  echo "FAIL: $*"
  failed=1
}

gitc() {
  git -c safe.directory='*' -c user.name=t -c user.email=t@t.invalid "$@"
}

mkdir "$WORK/repo"
(cd "$ROOT" && git -c safe.directory='*' ls-files -co --exclude-standard) \
  | (cd "$ROOT" && tar -cf - -T -) | tar -xf - -C "$WORK/repo"
cd "$WORK/repo"
git init -q
gitc add -A
gitc commit -qm snapshot
SNAPSHOT=$(git rev-parse HEAD)

key() { "$LANE_KEY" key "$1"; }

# --- which changes move which key -------------------------------------------

first() {
  f=$(git ls-files "$1" | head -n 1)
  [ -n "$f" ] || {
    echo "ERROR: no tracked file under $1" >&2
    exit 1
  }
  echo "$f"
}

# expect_keys <description> <android: same|changed> <web: same|changed> <command…>
expect_keys() {
  desc=$1 want_android=$2 want_web=$3
  shift 3
  before_android=$(key android) before_web=$(key web)
  "$@"
  gitc add -A
  gitc commit -qm "$desc" --allow-empty
  got_android=same got_web=same
  [ "$(key android)" = "$before_android" ] || got_android=changed
  [ "$(key web)" = "$before_web" ] || got_web=changed
  if [ "$got_android" = "$want_android" ] && [ "$got_web" = "$want_web" ]; then
    pass "$desc: android $got_android, web $got_web"
  else
    fail "$desc: android $got_android (want $want_android), web $got_web (want $want_web)"
  fi
}

append() { echo "# lane-key test" >>"$1"; }

[ "$(key android)" = "$(key android)" ] && [ "$(key web)" = "$(key web)" ] \
  && pass "keys are deterministic" || fail "keys differ between two runs"
[ "$(key android)" != "$(key web)" ] && pass "lanes have distinct keys" \
  || fail "android and web share a key"

expect_keys "empty commit" same same true
expect_keys "docs/ change" same same append "$(first docs)"
expect_keys "AGENTS.md change" same same append AGENTS.md
expect_keys "CLAUDE.md change" same same append CLAUDE.md
expect_keys "README.md change" same same append README.md
expect_keys ".woodpecker/checks.yml change" same same append .woodpecker/checks.yml
expect_keys "release-version.sh change" same same append scripts/release-version.sh
expect_keys "deploy/docker-compose.yml change" same same append deploy/docker-compose.yml
expect_keys "server integration test change" same same append "$(first crates/server/tests)"
expect_keys "android/ change" changed same append "$(first android)"
expect_keys "android/ file added" changed same touch android/lane-key-test-new-file
expect_keys "android/ mode change" changed same chmod +x "$(first android/app)"
expect_keys "contracts/ change" changed same append "$(first contracts)"
expect_keys "sync-version.sh change" changed same append scripts/sync-version.sh
expect_keys "build-box pin change" changed same append scripts/android-build-box-image.ref
expect_keys ".woodpecker/android.yml change" changed same append .woodpecker/android.yml
expect_keys "Dockerfile.android change" changed same append Dockerfile.android
expect_keys "e2e/ change" same changed append "$(first e2e/tests)"
expect_keys "crates/ change" same changed append "$(first crates/server/src)"
expect_keys "frontend/ change" same changed append "$(first frontend)"
expect_keys "deploy/grafana change" same changed append "$(first deploy/grafana)"
expect_keys ".woodpecker/web.yml change" same changed append .woodpecker/web.yml
expect_keys "Dockerfile.web change" same changed append Dockerfile.web
expect_keys "Cargo.toml change" changed changed append Cargo.toml
expect_keys "check-image-metadata.sh change" changed changed append scripts/check-image-metadata.sh
expect_keys "lane-key.sh change" changed changed append scripts/lane-key.sh
expect_keys "new root file" changed changed touch lane-key-test-new-root-file

# Uncommitted edits are not the commit's content.
before=$(key web)
append "$(first crates/server/src)"
[ "$(key web)" = "$before" ] && pass "uncommitted edit leaves the key alone" \
  || fail "uncommitted edit moved the key"
gitc checkout -q -- .

# --- skip / mark --------------------------------------------------------------

git init -q --bare "$WORK/remote.git"
export LANE_KEY_REMOTE="file://$WORK/remote.git"
unset FULL_RUN GITHUB_TOKEN 2>/dev/null || true

# expect_exit <description> <code> <command…>; output in $WORK/out
expect_exit() {
  desc=$1 want=$2
  shift 2
  got=0
  "$@" >"$WORK/out" 2>&1 || got=$?
  if [ "$got" = "$want" ]; then
    pass "$desc (exit $got)"
  else
    fail "$desc: exit $got, want $want"
    sed 's/^/    /' "$WORK/out"
  fi
}

expect_exit "no marker: run" 1 "$LANE_KEY" skip android-api-29
CI_PIPELINE_NUMBER=7 CI_COMMIT_BRANCH=feat/x \
  expect_exit "mark after a pass" 0 "$LANE_KEY" mark android-api-29
ref="refs/ci/green/android-api-29/$(key android)"
[ -n "$(git ls-remote "$LANE_KEY_REMOTE" "$ref")" ] && pass "marker is $ref" \
  || fail "no marker at $ref"
expect_exit "marker: skip" 0 "$LANE_KEY" skip android-api-29
grep -q 'pipeline: 7' "$WORK/out" && grep -q 'branch: feat/x' "$WORK/out" \
  && pass "skip names the pipeline and branch that passed" \
  || fail "skip did not name the pipeline: $(cat "$WORK/out")"
expect_exit "a marker is per step: api-36 still runs" 1 "$LANE_KEY" skip android-api-36
expect_exit "a marker is per lane: e2e-web still runs" 1 "$LANE_KEY" skip e2e-web
FULL_RUN=1 expect_exit "FULL_RUN=1 runs despite a marker" 1 "$LANE_KEY" skip android-api-29
FULL_RUN=0 expect_exit "FULL_RUN=0 still skips" 0 "$LANE_KEY" skip android-api-29

marker=$(git ls-remote "$LANE_KEY_REMOTE" "$ref" | cut -f 1)
append "$(first e2e/tests)"
gitc commit -qam "e2e change"
expect_exit "another lane's change keeps the skip" 0 "$LANE_KEY" skip android-api-29
CI_PIPELINE_NUMBER=8 expect_exit "re-marking an existing key succeeds" 0 "$LANE_KEY" mark android-api-29
grep -q 'already marked' "$WORK/out" && pass "re-mark says it was already marked" \
  || fail "re-mark output: $(cat "$WORK/out")"
[ "$(git ls-remote "$LANE_KEY_REMOTE" "$ref" | cut -f 1)" = "$marker" ] \
  && pass "an existing marker is never overwritten" || fail "marker was overwritten"

append "$(first android)"
gitc commit -qam "android change"
expect_exit "own lane's change: run" 1 "$LANE_KEY" skip android-api-29

# The two emulator steps mark at the same moment in one shared clone; each must
# land under its own step, labelled as that step.
"$LANE_KEY" mark android-api-29 >"$WORK/out29" 2>&1 &
"$LANE_KEY" mark android-api-36 >"$WORK/out36" 2>&1 &
wait
for api in 29 36; do
  r="refs/ci/green/android-api-$api/$(key android)"
  if git fetch -q --no-tags "$LANE_KEY_REMOTE" "+$r:refs/test/marker-$api" 2>/dev/null \
    && git cat-file -p "refs/test/marker-$api" | grep -q "^android-api-$api passed"; then
    pass "concurrent mark: android-api-$api landed under its own name"
  else
    fail "concurrent mark: android-api-$api missing or mislabelled: $(cat "$WORK/out$api")"
  fi
done

# …and skip together, each printing its own marker.
"$LANE_KEY" skip android-api-29 >"$WORK/out29" 2>&1 &
"$LANE_KEY" skip android-api-36 >"$WORK/out36" 2>&1 &
wait
for api in 29 36; do
  if grep -q "SKIPPING android-api-$api" "$WORK/out$api" \
    && grep -q "^  android-api-$api passed" "$WORK/out$api"; then
    pass "concurrent skip: android-api-$api printed its own marker"
  else
    fail "concurrent skip: android-api-$api output: $(cat "$WORK/out$api")"
  fi
done

LANE_KEY_REMOTE="file://$WORK/no-such-remote.git" \
  expect_exit "unreachable remote: run, never skip" 1 "$LANE_KEY" skip android-api-29
grep -q WARNING "$WORK/out" && pass "unreachable remote warns" || fail "no warning"
LANE_KEY_REMOTE="file://$WORK/no-such-remote.git" \
  expect_exit "unreachable remote: mark does not fail the step" 0 "$LANE_KEY" mark android-api-29
grep -q WARNING "$WORK/out" && pass "failed mark warns" || fail "no warning"
[ -z "$(git tag -l 'lane-key-mark-*')" ] && pass "mark leaves no local tag" \
  || fail "mark left a local tag"

expect_exit "unknown step" 2 "$LANE_KEY" skip build-web
expect_exit "unknown lane" 2 "$LANE_KEY" key prod

# --- every referenced input is declared ---------------------------------------

gitc checkout -q "$SNAPSHOT"

# Paths a lane's steps read: repo paths named in its workflow, and the paths
# the e2e compose files reach outside e2e/ (`../x` from e2e/ is `x`).
lane_refs() {
  case "$1" in
    android) files=.woodpecker/android.yml ;;
    web) files=".woodpecker/web.yml e2e/docker-compose.test.yml e2e/docker-compose.android-apk.test.yml" ;;
  esac
  for f in $files; do
    case "$f" in
      e2e/*) grep -oE '\.\./[A-Za-z0-9_./-]+' "$f" | sed 's|^\.\./||' ;;
      *) grep -oE '(^|[^A-Za-z0-9_./-])(Dockerfile\.[A-Za-z0-9_-]+|(scripts|e2e|deploy|android|crates|frontend|contracts)/[A-Za-z0-9_./-]+)' "$f" \
        | sed -E 's|^[^A-Za-z0-9_./-]||' ;;
    esac
  done | sed 's|/$||' | sort -u
}

for lane in android web; do
  covered=$("$LANE_KEY" files "$lane")
  missing=""
  for p in $(lane_refs "$lane"); do
    echo "$NOT_INPUTS" | grep -qxF "$p" && continue
    echo "$covered" | grep -qE "^$(printf '%s' "$p" | sed 's/[.[\*^$]/\\&/g')(/|$)" && continue
    missing="$missing $p"
  done
  if [ -z "$missing" ]; then
    pass "$lane: every referenced path is in its key or declared not an input"
  else
    fail "$lane: referenced but not in its key:$missing — add to lane_extras in" \
      "scripts/lane-key.sh, or to NOT_INPUTS here if it truly cannot affect the tests"
  fi
done

exit "$failed"
