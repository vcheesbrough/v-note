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
#      check that existed at the time stayed green;
#   5. the live smoke user (#179) stays least-privileged — external, one group,
#      never the admins group, password only as a secret placeholder — and
#      agrees with the smoke steps in .woodpecker/deploy.yml on its username
#      and email, and every step applying the file supplies the placeholder.
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

print("==> the #299 SQL console entries are present and safely scoped")
for env, doc in loaded.items():
    entries = doc["entries"]

    # The check that matters most in this file. The embedded outpost's
    # `providers` list is shared with every other service authentik protects on
    # this LAN, and a blueprint sets a list wholesale rather than appending — so
    # an outpost entry here would detach all of them. This file is applied on
    # every branch push, so it would not even wait for a merge.
    check(
        not any("outpost" in e["model"] for e in entries),
        f"{env}: declares no outpost entry (the shared provider list must not be rewritten)",
    )

    proxies = [
        e for e in entries if e["model"].endswith("proxyprovider") and e.get("state") != "absent"
    ]
    check(len(proxies) == 1, f"{env}: exactly one live proxy provider")
    if proxies:
        attrs = proxies[0]["attrs"]
        check(
            attrs.get("mode") == "forward_single",
            f"{env}: console provider is forward_single",
        )
        # authentik discards any path when building the callback URI, so a path
        # here would be silently ignored — and would tell the next reader that
        # the path scoping lives in authentik when it actually lives in the
        # Traefik routers.
        host = attrs.get("external_host", "")
        check(
            host.startswith("https://") and host.count("/") == 2,
            f"{env}: console external_host is a bare https host with no path (got {host!r})",
        )

    admins = f"v-note-{env}-admins"
    groups = {
        e["identifiers"].get("name")
        for e in entries
        if e["model"].endswith("group") and e.get("state") != "absent"
    }
    check(admins in groups, f"{env}: {admins} group is declared")

    # The console must be gated by the admins group, never by the ordinary user
    # group: "can use v-note" and "can read every row of everyone's notes" are
    # different facts and collapsing them is invisible until it matters.
    #
    # `!Find` tags load as None, so a binding's target and group cannot be
    # inspected from the parsed document. Assert the shape that is visible —
    # one binding per application — and pair it with a raw-text check that the
    # admins group is what the console binding names.
    bindings = [e for e in entries if e["model"].endswith("policybinding")]
    check(
        len(bindings) == 2,
        f"{env}: exactly two policy bindings — the app's and the console's",
    )
    with open(FILES[env]) as handle:
        raw = handle.read()
    check(
        f"[authentik_core.group, [name, {admins}]]" in raw,
        f"{env}: a binding resolves the {admins} group",
    )

print("==> the #179 live smoke user is least-privileged and matches the smoke step")
with open(".woodpecker/deploy.yml") as handle:
    deploy = yaml.load(handle, Loader=BlueprintLoader)
smoke_steps = {
    name: step for name, step in deploy["steps"].items() if name.startswith("smoke-web-live-")
}
check(
    {"smoke-web-live-auto-dev", "smoke-web-live-dev"} <= set(smoke_steps),
    "deploy.yml has smoke-web-live on both the push and deployment paths",
)
for env, doc in loaded.items():
    entries = doc["entries"]
    users = [e for e in entries if e["model"] == "authentik_core.user"]
    check(len(users) == 1, f"{env}: exactly one user entry (the smoke user)")
    if not users:
        continue
    user = users[0]
    attrs = user.get("attrs", {})
    username = f"v-note-smoke-{env}"
    check(user.get("state", "present") == "present", f"{env}: smoke user is present")
    check(
        user["identifiers"].get("username") == username and attrs.get("username") == username,
        f"{env}: smoke user is named {username}",
    )
    # Least privilege. `external` has no authentik user/admin interface;
    # superuser anywhere would make a credential CI holds an admin one.
    check(attrs.get("type") == "external", f"{env}: smoke user is type external")
    check(not attrs.get("is_superuser"), f"{env}: smoke user is not a superuser")
    check(
        (attrs.get("attributes") or {}).get("v-note/smoke-user") is True,
        f"{env}: smoke user carries the v-note/smoke-user marker",
    )
    # `!Find` loads as None, so the group is checked on the raw text: exactly one
    # group, and it is the ordinary users group — never the admins group, which
    # would hand the SQL console to a CI-held credential.
    groups = attrs.get("groups")
    check(isinstance(groups, list) and len(groups) == 1, f"{env}: smoke user is in exactly one group")
    with open(FILES[env]) as handle:
        raw = handle.read()
    user_block = raw[raw.index(f"username: {username}") :]
    # The entry ends at the first blank line.
    user_block = user_block.split("\n\n", 1)[0]
    check(
        f"!Find [authentik_core.group, [name, v-note-{env}-users]]" in user_block
        and f"v-note-{env}-admins" not in user_block,
        f"{env}: the smoke user's one group is v-note-{env}-users",
    )
    # The password is a placeholder the plugin fills from a Woodpecker secret —
    # a literal here would be a committed credential.
    placeholder = f"${{V_NOTE_{env.upper()}_SMOKE_PASSWORD}}"
    check(attrs.get("password") == placeholder, f"{env}: smoke password is the {placeholder} placeholder")
    check("password_hash" not in attrs, f"{env}: no password_hash alongside password")

    # The spec asserts /api/me returns this email and signs in with this name,
    # so the step and the blueprint must agree.
    for name in (f"smoke-web-live-auto-{env}", f"smoke-web-live-{env}"):
        environment = smoke_steps.get(name, {}).get("environment", {})
        check(
            environment.get("V_NOTE_SMOKE_USERNAME") == username,
            f"{env}: {name} V_NOTE_SMOKE_USERNAME matches the blueprint",
        )
        check(
            environment.get("V_NOTE_SMOKE_EMAIL") == attrs.get("email"),
            f"{env}: {name} V_NOTE_SMOKE_EMAIL matches the blueprint's email",
        )
    blueprint_vars = [
        s.get("settings", {}).get("vars", {})
        for s in deploy["steps"].values()
        if s.get("settings", {}).get("file") == FILES[env]
    ]
    check(
        blueprint_vars
        and all(f"V_NOTE_{env.upper()}_SMOKE_PASSWORD" in v for v in blueprint_vars),
        f"{env}: every step applying {FILES[env]} supplies V_NOTE_{env.upper()}_SMOKE_PASSWORD",
    )

if failures:
    print(f"\nauthentik blueprint validation FAILED ({len(failures)} check(s))")
    sys.exit(1)
print("\nauthentik blueprints OK")
PY
