#!/bin/sh
set -eu

# Holds lane-key.sh's .dockerignore matcher to Docker's own (#467). A lane key
# covers the files its build context admits; if the matcher admitted fewer
# files than Docker sends, a change to one of the missing files would skip tests
# it can affect. So for each lane this builds a throwaway image whose only job
# is to list the context BuildKit received, through the lane's real
# .dockerignore, and requires that list to equal `lane-key.sh context <lane>`.
#
# Runs against the tracked and untracked-but-not-ignored files of the working
# tree, committed into a scratch repository, so it checks what a push would
# check. Needs a docker daemon (the checks step mounts the socket).

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Same digest-pinned alpine as the checks steps.
LIST_IMAGE=alpine:3.21@sha256:48b0309ca019d89d40f670aa1bc06e426dc0931948452e8491e3d65087abc07d

mkdir "$WORK/repo"
(cd "$ROOT" && git -c safe.directory='*' ls-files -co --exclude-standard) \
  | (cd "$ROOT" && tar -cf - -T -) | tar -xf - -C "$WORK/repo"
cd "$WORK/repo"
git init -q
git add -A
git -c user.name=t -c user.email=t@t.invalid commit -qm snapshot

failed=0
for lane in android web; do
  ignore=$(case "$lane" in android) echo Dockerfile.android.dockerignore ;; web) echo Dockerfile.web.dockerignore ;; esac)
  mkdir "$WORK/df-$lane"
  # Outside the context, with the lane's ignore file beside it under the
  # Dockerfile-specific name BuildKit looks for.
  cat >"$WORK/df-$lane/Dockerfile" <<EOF
FROM $LIST_IMAGE AS list
COPY . /ctx
RUN cd /ctx && find . \\( -type f -o -type l \\) | sed 's|^\\./||' | LC_ALL=C sort > /files
FROM scratch
COPY --from=list /files /files
EOF
  cp "$ignore" "$WORK/df-$lane/Dockerfile.dockerignore"
  docker build -q -f "$WORK/df-$lane/Dockerfile" \
    --output "type=local,dest=$WORK/out-$lane" . >/dev/null

  "$ROOT/scripts/lane-key.sh" context "$lane" | LC_ALL=C sort >"$WORK/matcher-$lane"
  if diff -u "$WORK/out-$lane/files" "$WORK/matcher-$lane" >"$WORK/diff-$lane"; then
    echo "ok: $lane — lane-key.sh admits exactly Docker's context ($(wc -l <"$WORK/matcher-$lane") files)"
  else
    echo "FAIL: $lane — lane-key.sh context differs from Docker's (- docker, + lane-key.sh):"
    cat "$WORK/diff-$lane"
    failed=1
  fi
done

exit "$failed"
