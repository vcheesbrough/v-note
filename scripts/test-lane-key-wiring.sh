#!/usr/bin/env bash
# Validate how the lane-key gates are wired into the workflows (#467).
#
# test-lane-key.sh tests the script; this tests the few lines of YAML that
# decide when it is called, which matter more. A marker written for a run that
# never passed makes every later pipeline with that key skip silently. In every
# .woodpecker/*.yml step that calls scripts/lane-key.sh, this enforces:
#   1. the gated steps are exactly e2e-web and both Android instrumented lanes;
#   2. the first command is the skip guard, so a skip ends the step before
#      anything else runs;
#   3. `mark` is called exactly once, with the same step name as `skip`;
#   4. `mark` comes after every test-run line (`docker run`,
#      `docker compose … up`), and only on success: either as its own command
#      after test lines that cannot swallow a failure (step-level `set -e` stops
#      it), or guarded on the captured exit code before `exit $$RC`;
#   5. the step gets github_token, without which markers can be neither read
#      nor written.
# Negative cases mutate the parsed workflows to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import copy
import glob
import re
import sys

import yaml

GATED = {"e2e-web", "android-instrumented-api-29", "android-instrumented-api-36"}
LANE_KEY = "./scripts/lane-key.sh"


def is_test_run(line):
    return "docker run" in line or ("docker compose" in line and " up " in line)


def check(workflows):
    """Returns a list of problems; empty means the wiring is sound."""
    problems = []
    gated = set()
    for path, doc in sorted(workflows.items()):
        for name, step in (doc.get("steps") or {}).items():
            commands = [str(c) for c in step.get("commands") or []]
            if not any(LANE_KEY in c for c in commands):
                continue
            gated.add(name)
            where = f"{path} {name}"

            skip = re.fullmatch(
                re.escape(LANE_KEY) + r" skip (\S+); then exit 0; fi",
                commands[0].strip().removeprefix("if ") if commands else "",
            )
            if not (commands and commands[0].strip().startswith("if ") and skip):
                problems.append(f"{where}: first command is not the skip guard")
                continue
            arg = skip.group(1)

            lines = [(i, line.strip()) for i, c in enumerate(commands) for line in c.splitlines()]
            marks = [(n, i, line) for n, (i, line) in enumerate(lines) if f"{LANE_KEY} mark" in line]
            if len(marks) != 1:
                problems.append(f"{where}: calls mark {len(marks)} times, not once")
                continue
            n, i, line = marks[0]
            if not line.endswith(f"{LANE_KEY} mark {arg}"):
                problems.append(f"{where}: mark's step name differs from skip's ({arg})")

            runs = [(m, j, l) for m, (j, l) in enumerate(lines) if is_test_run(l)]
            if not runs:
                problems.append(f"{where}: no test-run line (docker run / docker compose up)")
                continue
            if n < max(m for m, _, _ in runs):
                problems.append(f"{where}: mark comes before the test run")

            standalone = commands[i].strip() == f"{LANE_KEY} mark {arg}"
            guarded = line == f'[ "$$RC" != 0 ] || {LANE_KEY} mark {arg}'
            if standalone:
                if any("||" in l for _, _, l in runs):
                    problems.append(f"{where}: a test-run line can swallow its failure (||) before mark")
            elif guarded:
                if not any(l.endswith("&& RC=0 || RC=$$?") for _, _, l in runs):
                    problems.append(f"{where}: mark is guarded on RC but no test run sets RC")
                rest = [l for m, (_, l) in enumerate(lines) if m > n]
                if "exit $$RC" not in rest:
                    problems.append(f"{where}: no `exit $$RC` after the guarded mark")
            else:
                problems.append(f"{where}: mark is neither its own command nor guarded on RC")

            token = (step.get("environment") or {}).get("GITHUB_TOKEN")
            if token != {"from_secret": "github_token"}:
                problems.append(f"{where}: GITHUB_TOKEN is not from_secret github_token")

    if gated != GATED:
        problems.append(f"gated steps are {sorted(gated)}, expected {sorted(GATED)}")
    return problems


workflows = {p: yaml.safe_load(open(p)) for p in glob.glob(".woodpecker/*.yml")}
failed = False


def report(ok, message):
    global failed
    print(("ok   " if ok else "FAIL ") + message)
    failed |= not ok


problems = check(workflows)
for p in problems:
    report(False, p)
report(not problems, "lane-key gates are wired: skip first, mark once, only after a passed test run")


def mutated(path, step, change):
    copied = copy.deepcopy(workflows)
    change(copied[path]["steps"][step])
    return check(copied)


def move_mark_first(step):
    mark = step["commands"].pop()
    step["commands"].insert(1, mark)


def drop_rc_guard(step):
    step["commands"] = [
        c.replace('[ "$$RC" != 0 ] || ./scripts', "./scripts") for c in step["commands"]
    ]


def drop_skip(step):
    step["commands"] = step["commands"][1:]


def swallow_failure(step):
    step["commands"] = [c.replace("docker run --rm --privileged v-note", "docker run --rm --privileged v-note || true #") for c in step["commands"]]


def mark_build(step):
    step["commands"] = list(step["commands"]) + ["./scripts/lane-key.sh mark e2e-web"]


def mark_other_step(step):
    step["commands"] = [c.replace("mark e2e-web", "mark android-api-29") for c in step["commands"]]


cases = [
    ("android mark moved before the emulator run", ".woodpecker/android.yml", "android-instrumented-api-36", move_mark_first),
    ("e2e mark without its RC guard", ".woodpecker/web.yml", "e2e-web", drop_rc_guard),
    ("skip guard removed", ".woodpecker/android.yml", "android-instrumented-api-29", drop_skip),
    ("emulator run that swallows its failure", ".woodpecker/android.yml", "android-instrumented-api-36", swallow_failure),
    ("mark added to an ungated step", ".woodpecker/web.yml", "build-web", mark_build),
    ("mark under another step's name", ".woodpecker/web.yml", "e2e-web", mark_other_step),
]
for label, path, step, change in cases:
    found = mutated(path, step, change)
    report(bool(found), f"catches: {label}" + (f" ({found[0]})" if found else ""))

if failed:
    sys.exit(1)
print("\nlane-key wiring OK")
PY
