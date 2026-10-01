#!/bin/sh
set -eu

# Lane keys (#467): skip an expensive test step when exactly what it tests has
# already passed.
#
#   lane-key.sh key <lane>        the lane's key
#   lane-key.sh files <lane>      every tracked file the key covers
#   lane-key.sh context <lane>    the subset its Docker build context admits
#   lane-key.sh skip <step>       exit 0 (and say why) when <step> may skip
#   lane-key.sh mark <step>       record that <step> passed under its key
#
# Lanes are `android` and `web`. Steps are the test steps that consult a lane:
# android-api-29 and android-api-36 (android), e2e-web (web). Each step keeps its
# own marker, because a pass on API 29 says nothing about API 36.
#
# A lane's key is a sha256 over the mode, blob and path of every file it covers,
# read from HEAD's tree — so it is the commit's content, never the working tree,
# and a commit whose files are unchanged for a lane has the same key whatever
# else changed. The files are:
#
#   - the build context: every tracked file the lane's Dockerfile-specific
#     .dockerignore admits. Derived, never listed, so a file added to the
#     context is in the key by construction. test-lane-key-context.sh holds this
#     matcher to what Docker itself sends.
#   - the extras below: what the lane's steps read from outside that context.
#     test-lane-key.sh fails when a workflow or compose file references a path
#     that is neither covered nor declared deliberately not an input.
#
# Deliberately NOT in any key: the release tag (V_NOTE_RELEASE) — it differs on
# every pipeline, so a key that held it would never match. Images are still
# built every pipeline with their own tag; only the tests are skipped.
#
# Cargo.toml and Cargo.lock are in the android context whole, not just their
# version field: a dependency bump re-runs the emulators needlessly, but a
# hand-picked field is one more special case that could silently go stale. The
# workspace version moves once per iteration, which is when Android re-runs
# anyway.
#
# A marker is an annotated tag object at refs/ci/green/<step>/<key> on the
# remote, naming the commit, branch and pipeline that passed. It is pushed
# create-only, so a marker is never overwritten.
# FULL_RUN set to anything but "", 0 or false makes `skip` always run.
#
# Environment: CI_REPO and GITHUB_TOKEN (for a private remote); CI_COMMIT_BRANCH,
# CI_PIPELINE_NUMBER and CI_PIPELINE_URL label a marker. LANE_KEY_REMOTE
# overrides the remote URL (the tests point it at a local bare repository).

die() {
  echo "ERROR: $*" >&2
  exit 2
}

lane_dockerignore() {
  case "$1" in
    android) echo Dockerfile.android.dockerignore ;;
    web) echo Dockerfile.web.dockerignore ;;
    *) die "unknown lane '$1' (android|web)" ;;
  esac
}

# Paths (files or directories) a lane's steps read from outside its build
# context. Keep in step with .woodpecker/<lane>.yml and the e2e compose files.
lane_extras() {
  case "$1" in
    android)
      cat <<'EOF'
Dockerfile.android
Dockerfile.android.dockerignore
.woodpecker/android.yml
scripts/android-build-box-image.ref
scripts/check-image-metadata.sh
scripts/lane-key.sh
EOF
      ;;
    web)
      # e2e/ is the stack, specs and support images; deploy/grafana is an
      # additional build context of the playwright image (the dashboard spec).
      cat <<'EOF'
Dockerfile.web
Dockerfile.web.dockerignore
.woodpecker/web.yml
e2e
deploy/grafana
scripts/check-image-metadata.sh
scripts/test-container-health.sh
scripts/lane-key.sh
EOF
      ;;
    *) die "unknown lane '$1' (android|web)" ;;
  esac
}

step_lane() {
  case "$1" in
    android-api-29 | android-api-36) echo android ;;
    e2e-web) echo web ;;
    *) die "unknown step '$1' (android-api-29|android-api-36|e2e-web)" ;;
  esac
}

git_local() {
  git -c safe.directory='*' -c core.quotePath=false "$@"
}

# HEAD's tree, one "<mode> <type> <blob>\t<path>" line per file.
head_tree() {
  git_local ls-tree -r HEAD || die "git ls-tree HEAD failed"
}

# Prints the tree lines whose path the lane's .dockerignore admits, using
# Docker's rules: patterns are anchored at the context root, `*` and `?` do
# not cross `/`, `**` does, a pattern also excludes everything under a
# directory it matches, `!` re-admits, and the last matching pattern wins.
context_tree() {
  ignore=$(lane_dockerignore "$1")
  [ -f "$ignore" ] || die "$ignore not found"
  head_tree | awk -F '\t' -v ignore="$ignore" '
    function glob2re(p,   out, i, n, c, j, cls) {
      out = ""; n = length(p)
      for (i = 1; i <= n; i++) {
        c = substr(p, i, 1)
        if (c == "*") {
          if (substr(p, i + 1, 1) == "*") {
            i++
            if (substr(p, i + 1, 1) == "/") { i++; out = out "(.*/)?" }
            else out = out ".*"
          } else out = out "[^/]*"
        } else if (c == "?") out = out "[^/]"
        else if (c == "[") {
          j = index(substr(p, i), "]")
          if (j > 0) {
            cls = substr(p, i, j)
            if (substr(cls, 2, 1) == "!") cls = "[^" substr(cls, 3)
            out = out cls; i += j - 1
          } else out = out "\\["
        } else if (c == "\\") { i++; out = out "\\" substr(p, i, 1) }
        else if (index(".+(){}|^$", c)) out = out "\\" c
        else out = out c
      }
      # A match on a parent directory excludes everything under it.
      return "^" out "(/.*)?$"
    }
    BEGIN {
      n = 0
      while ((getline line < ignore) > 0) {
        gsub(/^[ \t]+|[ \t\r]+$/, "", line)
        if (line == "" || substr(line, 1, 1) == "#") continue
        neg = 0
        if (substr(line, 1, 1) == "!") { neg = 1; line = substr(line, 2) }
        sub(/^(\.\/|\/)+/, "", line)
        sub(/\/+$/, "", line)
        if (line == "") continue
        n++; re[n] = glob2re(line); readmit[n] = neg
      }
      close(ignore)
    }
    {
      admitted = 1
      for (k = 1; k <= n; k++) if ($2 ~ re[k]) admitted = readmit[k]
      if (admitted) print
    }'
}

# Context lines plus the extras, deduplicated, in path order.
covered_tree() {
  extras=$(lane_extras "$1")
  {
    context_tree "$1"
    head_tree | awk -F '\t' -v extras="$extras" '
      BEGIN { n = split(extras, e, "\n") }
      { for (k = 1; k <= n; k++) if ($2 == e[k] || index($2, e[k] "/") == 1) { print; next } }'
  } | sort -t "$(printf '\t')" -k2 -u
}

lane_key() {
  tree=$(covered_tree "$1")
  [ -n "$tree" ] || die "lane $1 covers no files"
  { echo "lane-key v1 $1"; echo "$tree"; } | sha256sum | cut -d ' ' -f 1
}

remote() {
  echo "${LANE_KEY_REMOTE:-https://github.com/${CI_REPO:?CI_REPO must be set}.git}"
}

# The token travels as a header, never in the URL, so a git error message
# cannot print it (as in release-version.sh).
git_remote() {
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    auth=$(printf 'x-access-token:%s' "$GITHUB_TOKEN" | base64 | tr -d '\n')
    git -c safe.directory='*' -c http.extraHeader="Authorization: Basic $auth" "$@"
  else
    git -c safe.directory='*' "$@"
  fi
}

marker_ref() {
  echo "refs/ci/green/$1/$2"
}

# Any failure here means "run": a marker that cannot be read is no marker.
skip() {
  step=$1
  lane=$(step_lane "$step")
  key=$(lane_key "$lane")
  case "${FULL_RUN:-}" in
    "" | 0 | false) ;;
    *)
      echo "lane-key: FULL_RUN=$FULL_RUN — running $step (key $key)"
      return 1
      ;;
  esac
  ref=$(marker_ref "$step" "$key")
  if ! listing=$(git_remote ls-remote "$(remote)" "$ref"); then
    echo "lane-key: WARNING: could not read markers — running $step (key $key)"
    return 1
  fi
  if [ -z "$listing" ]; then
    echo "lane-key: no green marker for $step at $lane key $key — running"
    return 1
  fi
  echo "lane-key: SKIPPING $step — $lane key $key has already passed:"
  if git_remote fetch -q --no-tags --depth=1 "$(remote)" "$ref" 2>/dev/null; then
    git_local cat-file -p FETCH_HEAD | sed -e '1,/^$/d' -e 's/^/  /'
  else
    echo "  ($ref)"
  fi
  echo "lane-key: set FULL_RUN=1 on a manual pipeline to run it anyway"
  return 0
}

# Never fails the step: the tests passed, and a missing marker only costs a
# re-run next time. It says so loudly instead.
mark() {
  step=$1
  lane=$(step_lane "$step")
  key=$(lane_key "$lane")
  ref=$(marker_ref "$step" "$key")
  tmp="lane-key-mark-$$"
  git_local -c user.name=v-note-ci -c user.email=ci@v-note.invalid \
    tag -f -a "$tmp" HEAD -m "$step passed at $lane key $key

commit: $(git_local rev-parse HEAD)
branch: ${CI_COMMIT_BRANCH:-unknown}
pipeline: ${CI_PIPELINE_NUMBER:-unknown} ${CI_PIPELINE_URL:-}" >/dev/null
  # Create-only: an empty lease is "the ref must not exist", checked by the
  # server. A plain push is not enough — a newer commit peels as a fast-forward
  # of the old marker's, so it would replace it.
  if git_remote push -q --force-with-lease="$ref:" "$(remote)" "refs/tags/$tmp:$ref" 2>/dev/null; then
    echo "lane-key: marked $step green at $lane key $key"
  elif [ -n "$(git_remote ls-remote "$(remote)" "$ref" 2>/dev/null)" ]; then
    echo "lane-key: $step at $lane key $key was already marked green"
  else
    echo "lane-key: WARNING: could not record the green marker $ref;" \
      "the next pipeline with this key will re-run $step"
  fi
  git_local tag -d "$tmp" >/dev/null
}

[ $# -eq 2 ] || die "usage: $0 key|files|context <lane> | skip|mark <step>"
case "$1" in
  key) lane_key "$2" ;;
  files) covered_tree "$2" | cut -f 2 ;;
  context) context_tree "$2" | cut -f 2 ;;
  skip) skip "$2" ;;
  mark) mark "$2" ;;
  *) die "usage: $0 key|files|context <lane> | skip|mark <step>" ;;
esac
