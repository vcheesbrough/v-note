#!/usr/bin/env bash
# Every path a lane's tests read must be in that lane's key (#467, #476).
#
# The lane-skip plugin skips e2e-web and the emulator lanes when their key has
# already passed, so a key that misses an input is a green that never tested
# the change. The plugin owns the key itself (and its tests); this is v-note's
# policy over lanes.yml: every repo path a lane's workflow names,
# and every path its e2e compose files reach outside e2e/, must be covered.
#
# The covered files come from the plugin, never from a copy of its matcher:
# `lane-files` (checks.yml) runs the pinned lane-skip image in `files` mode and
# writes one list per lane to $LANE_FILES (default .lane/files/<lane>.txt).
# Locally:
#   for lane in web android; do
#     docker run --rm -v "$PWD:/w" -w /w -e PLUGIN_MODE=files -e PLUGIN_CONFIG=lanes.yml -e PLUGIN_LANE=$lane \
#       <lane-skip image from .woodpecker/checks.yml> >.lane/files/$lane.txt
#   done
#
# Paths assembled from variables cannot be seen by any text scan; declare those
# in lanes.yml by hand.

set -euo pipefail

cd "$(dirname "$0")/.."
LANE_FILES=${LANE_FILES:-.lane/files}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Referenced by a lane's steps but deliberately outside its key. Each entry
# needs a reason it cannot affect that lane's tests.
NOT_INPUTS=''

failed=0
pass() { echo "ok: $*"; }
fail() {
  echo "FAIL: $*"
  failed=1
}

git ls-files >"$WORK/tracked"

# Paths a lane's steps read: repo paths named in its workflow, and the paths
# the e2e compose files reach outside e2e/ (`../x` from e2e/ is `x`).
lane_refs() {
  case "$1" in
    android) files="$ANDROID_WORKFLOW" ;;
    web) files="$WEB_WORKFLOW e2e/docker-compose.test.yml e2e/docker-compose.android-apk.test.yml" ;;
  esac
  # Every path-shaped token is a candidate (compose paths are relative to e2e/,
  # so only their `../` escapes leave it). A candidate is a reference when it
  # names a tracked file exactly, or contains a `/` and names a tracked
  # directory, so justfile, rust-toolchain.toml or .cargo/… count, while a
  # bare word like `deploy` in a comment does not.
  for f in $files; do
    case "$f" in
      e2e/*) grep -oE '\.\./[A-Za-z0-9_.+/-]+' "$f" | sed 's|^\.\./||' ;;
      *) grep -oE '[A-Za-z0-9_.+/-]+' "$f" ;;
    esac
  done | sed -E -e 's|^(\./)+||' -e 's|[./:]+$||' | sort -u \
    | awk 'NR == FNR {
             file[$0] = 1
             n = split($0, part, "/"); d = ""
             for (i = 1; i < n; i++) { d = (i > 1 ? d "/" : "") part[i]; dir[d] = 1 }
             next
           }
           ($0 in file) || (index($0, "/") && ($0 in dir))' "$WORK/tracked" -
}

undeclared_refs() {
  covered="$LANE_FILES/$1.txt"
  [ -s "$covered" ] || {
    echo "MISSING-LIST"
    return
  }
  missing=""
  for p in $(lane_refs "$1"); do
    echo "$NOT_INPUTS" | grep -qxF "$p" && continue
    # Covered when a listed file is the path itself or lies under it. Literal
    # comparison: a path may hold regex metacharacters such as `+`.
    awk -v p="$p" '$0 == p || index($0, p "/") == 1 { found = 1; exit } END { exit !found }' "$covered" \
      && continue
    missing="$missing $p"
  done
  echo "$missing"
}

for lane in android web; do
  [ -s "$LANE_FILES/$lane.txt" ] && pass "$lane: the plugin listed $(wc -l <"$LANE_FILES/$lane.txt") covered files" \
    || fail "$lane: no covered-file list at $LANE_FILES/$lane.txt (run lane-skip in files mode first)"
done

# The guard itself: a root-level file the android context excludes, newly
# referenced by its workflow, must be reported.
cp .woodpecker/android.yml "$WORK/android.yml"
echo "# reads rust-toolchain.toml" >>"$WORK/android.yml"
ANDROID_WORKFLOW="$WORK/android.yml" WEB_WORKFLOW=.woodpecker/web.yml
case " $(undeclared_refs android) " in
  *" rust-toolchain.toml "*) pass "an undeclared root-level reference is caught" ;;
  *) fail "rust-toolchain.toml referenced by android.yml was not reported" ;;
esac

ANDROID_WORKFLOW=.woodpecker/android.yml
for lane in android web; do
  missing=$(undeclared_refs "$lane")
  if [ -z "$missing" ]; then
    pass "$lane: every referenced path is in its key or declared not an input"
  else
    fail "$lane: referenced but not in its key:$missing — add it to the lane's" \
      "inputs in lanes.yml, or to NOT_INPUTS here if it truly cannot affect the tests"
  fi
done

exit "$failed"
