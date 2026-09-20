#!/usr/bin/env bash
# Validate authentik/blueprint-<env>.yaml without an Authentik instance.
#
# Why this exists — and it is the same reason as scripts/test-grafana-dashboard.sh:
# a repo-owned artifact that CI applies to shared infrastructure deserves a check
# that runs *without* that infrastructure. The blueprint deletes and recreates
# live credentials on every deploy; the only other thing that reads it is the
# apply itself, by which point login is already broken for everyone.
#
# So this enforces, on every push:
#   1. the file parses (including the `!Find` tags Authentik blueprints use);
#   2. it never names another environment — the per-environment split (#274) is
#      what keeps one environment's deploy off another's objects, and this is
#      what stops foreign content drifting back in;
#   3. it carries its own `instance_name`-worthy identity and the entries the
#      migration depends on (unified public provider, retired objects);
#   4. the provider declares the grants it needs. Authentik defaults a
#      blueprint-created provider to *no* grants, which rejects every authorize
#      request — #372 shipped exactly that and took dev login down while every
#      check that existed at the time stayed green.
#
# Dev is the only environment today (#392); #388 adds prod as a second file and
# every check below applies to it unchanged.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - "$@" <<'PY'
import sys
import yaml


class BlueprintLoader(yaml.SafeLoader):
    """Authentik blueprints use custom tags (`!Find`, `!KeyOf`, …)."""


BlueprintLoader.add_multi_constructor("!", lambda loader, suffix, node: None)

ENVIRONMENTS = ("dev",)
FILES = {env: f"authentik/blueprint-{env}.yaml" for env in ENVIRONMENTS}
# Environment names a blueprint must never mention — including ones that do not
# exist yet (#388), so prod content cannot creep into the dev file.
FOREIGN = {env: [other for other in ("dev", "prod") if other != env] for env in ENVIRONMENTS}

failures = []
loaded = {}


def check(condition, message):
    if condition:
        print(f"  ok   — {message}")
    else:
        print(f"  FAIL — {message}")
        failures.append(message)


print("==> every blueprint parses")
for env, path in FILES.items():
    try:
        with open(path) as handle:
            loaded[env] = yaml.load(handle, Loader=BlueprintLoader)
        check(True, f"{path} parses")
    except Exception as error:  # noqa: BLE001 - surfaced verbatim below
        check(False, f"{path} parses ({error})")

if failures:
    sys.exit(1)

print("==> no blueprint references another environment")
for env, path in FILES.items():
    with open(path) as handle:
        # Ignore comments: they legitimately explain why the split exists.
        body = "\n".join(
            line for line in handle.read().splitlines() if not line.lstrip().startswith("#")
        )
    for foreign in FOREIGN[env]:
        check(
            foreign not in body,
            f"{path} never mentions '{foreign}' outside comments",
        )

print("==> the #274 migration entries are present")
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
