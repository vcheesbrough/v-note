#!/usr/bin/env bash
# Attach the SQL console's proxy provider to the shared Authentik outpost (#299).
#
# Why this is a script and not a blueprint entry — the whole point:
#
# A proxy provider does nothing until an outpost serves it. The forward-auth
# endpoint answers 404 for a host no attached provider claims, which is why
# `authentik/blueprint-dev.yaml` alone leaves /dbconsole unreachable.
#
# The obvious fix — declaring `authentik_outposts.outpost` in that blueprint —
# is a trap. The embedded outpost is SHARED: its `providers` list also carries
# the providers protecting glances, woodpecker, uptime-kuma, the Traefik
# dashboard and the rest. A blueprint sets a list wholesale rather than
# appending, so that entry would detach all of them, and the blueprint is
# applied on *every branch push*, not just master. One careless entry, LAN-wide
# auth outage.
#
# So this does an explicit read-modify-write instead: read the list, append our
# provider if it is missing, write it back — and refuse to write anything that
# would remove a provider it did not add. It is idempotent, so it runs on every
# deploy and is a no-op on all but the first.
#
# Required environment:
#   AUTHENTIK_URL     base URL, e.g. https://auth.desync.link
#   AUTHENTIK_TOKEN   API token with permission to read providers and patch outposts
#   PROVIDER_NAME     the proxy provider to attach, e.g. v-note-sql-console-dev
# Optional:
#   OUTPOST_NAME      defaults to "authentik Embedded Outpost"

set -euo pipefail

OUTPOST_NAME="${OUTPOST_NAME:-authentik Embedded Outpost}"

for name in AUTHENTIK_URL AUTHENTIK_TOKEN PROVIDER_NAME; do
  if [ -z "$(printenv "$name" || true)" ]; then
    echo "ERROR: $name is required" >&2
    exit 1
  fi
done

api() {
  method="$1"
  path="$2"
  shift 2
  curl -sS --fail-with-body -X "$method" \
    -H "Authorization: Bearer ${AUTHENTIK_TOKEN}" \
    -H "Content-Type: application/json" \
    "${AUTHENTIK_URL%/}/api/v3${path}" "$@"
}

# jq rather than python3 so the pipeline step needs only curl and jq. The
# outpost name contains a space, so this is not optional.
urlencode() {
  jq -rn --arg s "$1" '$s | @uri'
}

echo "==> looking up proxy provider '${PROVIDER_NAME}'"
provider_pk=$(
  api GET "/providers/proxy/?name__iexact=$(urlencode "$PROVIDER_NAME")" |
    jq -r '.results[0].pk // empty'
)
if [ -z "$provider_pk" ]; then
  echo "ERROR: no proxy provider named '${PROVIDER_NAME}'" >&2
  echo "       The blueprint apply that creates it must run before this step." >&2
  exit 1
fi
echo "    provider pk=${provider_pk}"

echo "==> looking up outpost '${OUTPOST_NAME}'"
outpost=$(
  api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
    jq -c '.results[0] // empty'
)
if [ -z "$outpost" ]; then
  echo "ERROR: no outpost named '${OUTPOST_NAME}'" >&2
  exit 1
fi

outpost_pk=$(printf '%s' "$outpost" | jq -r '.pk')
before=$(printf '%s' "$outpost" | jq -c '.providers // []')
echo "    outpost pk=${outpost_pk}, currently serving $(printf '%s' "$before" | jq 'length') provider(s)"

if printf '%s' "$before" | jq -e --argjson pk "$provider_pk" 'index($pk) != null' >/dev/null; then
  echo "==> already attached — nothing to do"
  exit 0
fi

after=$(printf '%s' "$before" | jq -c --argjson pk "$provider_pk" '. + [$pk]')

# Belt and braces on the one operation that could take down every protected
# service on the LAN: the list being written must be a strict superset of the
# list that was read. If a concurrent push changed it underneath us, or a jq
# expression above ever regresses, this refuses rather than shrinking it.
if ! printf '%s' "$after" | jq -e --argjson before "$before" \
  '($before - .) == []' >/dev/null; then
  echo "ERROR: refusing to write a provider list that drops existing entries" >&2
  echo "       before=${before}" >&2
  echo "       after=${after}" >&2
  exit 1
fi

echo "==> attaching provider ${provider_pk} to outpost ${outpost_pk}"
api PATCH "/outposts/instances/${outpost_pk}/" \
  --data "$(jq -nc --argjson providers "$after" '{providers: $providers}')" >/dev/null

# Read back rather than trusting the write: this is shared state, and "the PATCH
# returned 200" is not the same claim as "the provider is now served".
verified=$(
  api GET "/outposts/instances/?name__iexact=$(urlencode "$OUTPOST_NAME")" |
    jq -c '.results[0].providers // []'
)
if ! printf '%s' "$verified" | jq -e --argjson pk "$provider_pk" 'index($pk) != null' >/dev/null; then
  echo "ERROR: provider ${provider_pk} is still not attached after the patch" >&2
  echo "       outpost now serves: ${verified}" >&2
  exit 1
fi

echo "==> attached; outpost now serves $(printf '%s' "$verified" | jq 'length') provider(s)"
