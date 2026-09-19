#!/usr/bin/env bash
# Validate authentik/blueprint-{dev,prod}.yaml without an Authentik instance.
#
# Why this exists: #274 split one dev+prod blueprint into two per-environment
# files so no dev deploy can reach prod's Authentik objects. That split also cost
# the prod file its only regular exercise — before, every dev deploy parsed the
# prod entries too. `blueprint-prod.yaml` is now applied *only* by a manual prod
# deployment, so a typo in it would surface at the worst possible moment.
#
# So this enforces, on every push, what the split relies on:
#   1. both files parse (including the `!Find` tags Authentik blueprints use);
#   2. neither file mentions the other environment — the whole point of the split;
#   3. the two files stay structurally identical, which the "keep them in step"
#      comment otherwise asserts on the honour system;
#   4. each file carries its own `instance_name`-worthy identity and the entries
#      the migration depends on (unified public provider, retired objects);
#   5. the provider declares the grants it needs. Authentik defaults a
#      blueprint-created provider to *no* grants, which rejects every authorize
#      request — #372 shipped exactly that and took login down in both
#      environments.
#
# Mirrors scripts/test-grafana-dashboard.sh: a repo-owned artifact applied by CI
# to shared infrastructure deserves a check that runs without that infrastructure.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import sys
import yaml


class BlueprintLoader(yaml.SafeLoader):
    """Authentik blueprints use custom tags (`!Find`, `!KeyOf`, …)."""


BlueprintLoader.add_multi_constructor("!", lambda loader, suffix, node: None)

FILES = {env: f"authentik/blueprint-{env}.yaml" for env in ("dev", "prod")}
OTHER = {"dev": "prod", "prod": "dev"}

failures = []
loaded = {}


def check(condition, message):
    if condition:
        print(f"  ok   — {message}")
    else:
        print(f"  FAIL — {message}")
        failures.append(message)


print("==> both blueprints parse")
for env, path in FILES.items():
    try:
        with open(path) as handle:
            loaded[env] = yaml.load(handle, Loader=BlueprintLoader)
        check(True, f"{path} parses")
    except Exception as error:  # noqa: BLE001 - surfaced verbatim below
        check(False, f"{path} parses ({error})")

if failures:
    sys.exit(1)

print("==> no blueprint references the other environment")
for env, path in FILES.items():
    with open(path) as handle:
        # Ignore comments: they legitimately explain why the split exists.
        body = "\n".join(
            line for line in handle.read().splitlines() if not line.lstrip().startswith("#")
        )
    check(
        OTHER[env] not in body,
        f"{path} never mentions '{OTHER[env]}' outside comments",
    )

print("==> the two files stay structurally identical")


def shape(doc, env):
    """Entry identities with the environment name factored out."""
    out = []
    for entry in doc["entries"]:
        ident = ",".join(
            f"{k}={v}".replace(env, "<env>") for k, v in sorted(entry["identifiers"].items())
        )
        out.append(f"{entry.get('state', 'present')} {entry['model']} {ident}")
    return out


dev_shape, prod_shape = shape(loaded["dev"], "dev"), shape(loaded["prod"], "prod")
check(
    dev_shape == prod_shape,
    "dev and prod declare the same entries in the same order",
)
if dev_shape != prod_shape:
    only_dev = [line for line in dev_shape if line not in prod_shape]
    only_prod = [line for line in prod_shape if line not in dev_shape]
    for line in only_dev:
        print(f"         dev only:  {line}")
    for line in only_prod:
        print(f"         prod only: {line}")

print("==> the #274 migration entries are present in both")
for env, doc in loaded.items():
    entries = doc["entries"]

    providers = [
        e for e in entries if e["model"].endswith("oauth2provider") and e.get("state") != "absent"
    ]
    check(
        len(providers) == 1 and providers[0]["identifiers"]["name"] == f"v-note-{env}",
        f"{env}: exactly one live provider, named v-note-{env}",
    )
    if providers:
        attrs = providers[0]["attrs"]
        check(attrs.get("client_type") == "public", f"{env}: provider is a public client")
        check("client_secret" not in attrs, f"{env}: provider declares no client_secret")
        urls = [r["url"] for r in attrs.get("redirect_uris", [])]
        check(
            any(u.endswith("/auth/callback") for u in urls)
            and any(u.endswith("/auth/mobile/callback") for u in urls),
            f"{env}: provider carries both the SPA and Android redirect URIs",
        )
        check(
            all(r.get("matching_mode") == "strict" for r in attrs.get("redirect_uris", [])),
            f"{env}: every redirect URI is strict-matched",
        )

        # #372: a blueprint-created provider defaults to an empty grant list, and
        # an empty list makes Authentik reject *every* authorize request with
        # `invalid_request`. The field is silently absent rather than wrong, so
        # nothing but an explicit check catches it.
        grants = attrs.get("grant_types")
        check(
            isinstance(grants, list) and grants,
            f"{env}: provider declares a non-empty grant_types",
        )
        grants = grants if isinstance(grants, list) else []
        for required in ("authorization_code", "refresh_token"):
            check(required in grants, f"{env}: provider grants {required}")
        # A public client holds no secret, so any grant that authenticates the
        # client by secret — or hands out a token without the code exchange —
        # must stay off.
        for forbidden in (
            "implicit",
            "hybrid",
            "password",
            "client_credentials",
            "urn:ietf:params:oauth:grant-type:device_code",
        ):
            check(forbidden not in grants, f"{env}: provider does not grant {forbidden}")

    retired = {
        e["identifiers"].get("name") or e["identifiers"].get("slug")
        for e in entries
        if e.get("state") == "absent"
    }
    for name in (f"v-note-android-{env}", f"v-note-browser-{env}"):
        check(name in retired, f"{env}: {name} is retired (state: absent)")

    # The retirements must come last, or the application would be deleted while
    # still pointing at the provider being replaced.
    states = [e.get("state", "present") for e in entries]
    check(
        "present" not in states[states.index("absent") :] if "absent" in states else False,
        f"{env}: every `absent` entry is ordered after the `present` ones",
    )

if failures:
    print(f"\nauthentik blueprint validation FAILED ({len(failures)} check(s))")
    sys.exit(1)
print("\nauthentik blueprints OK")
PY
