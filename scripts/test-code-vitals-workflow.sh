#!/usr/bin/env bash
# Validate .woodpecker/code-vitals.yml without running it.
#
# The workflow records this repo's trunk commits for code-vitals; its one
# promise to v-note is that it can never fail or hold up a build. This holds
# the shape that promise rests on:
#   1. it runs on `master` push / manual only (never cron, never a PR);
#   2. it is one step, `failure: ignore`, with no `depends_on` of its own;
#   3. no other workflow depends on it;
#   4. the collector's credential comes by name from the broker, never inline;
#   5. the step never redirects the shell's stdin (Woodpecker pipes the script in).
# What the step does is code-vitals' to test: the file is a copy of its
# deploy/measured-repo-workflow.yml, pinned by tests/test_measured_repo_workflow.py
# there. Negative cases mutate the parsed workflows to prove each rule bites.

set -euo pipefail

cd "$(dirname "$0")/.."

python3 - <<'PY'
import copy
import glob
import re
import sys

import yaml

CODE_VITALS = ".woodpecker/code-vitals.yml"
NAME = "code-vitals"


def as_list(value):
    if value is None:
        return []
    return [value] if isinstance(value, str) else list(value)


def check(workflows):
    problems = []
    workflow = workflows.get(CODE_VITALS)
    if workflow is None:
        return [f"{CODE_VITALS} is missing"]
    when = workflow.get("when")
    # No rules (absent or an empty list) means no limit: every event runs it.
    rules = when if isinstance(when, list) else [when or {}]
    if not rules:
        problems.append(f"{CODE_VITALS}: an empty `when` runs on every event")
    for rule in rules:
        if set(as_list(rule.get("event"))) != {"push", "manual"}:
            problems.append(f"{CODE_VITALS}: runs on {rule.get('event')!r}, not push/manual only")
        if rule.get("branch") != "master":
            problems.append(f"{CODE_VITALS}: not limited to master (branch {rule.get('branch')!r})")
    if workflow.get("depends_on"):
        problems.append(f"{CODE_VITALS}: waits on other workflows")
    steps = workflow.get("steps") or {}
    if len(steps) != 1:
        problems.append(f"{CODE_VITALS}: has {len(steps)} steps, not one")
    for name, step in steps.items():
        if step.get("failure") != "ignore":
            problems.append(f"{CODE_VITALS}: step {name} can fail the pipeline")
        if step.get("depends_on"):
            problems.append(f"{CODE_VITALS}: step {name} has depends_on")
        # Woodpecker pipes the script into the shell: `exec <…` drops the rest
        # of it and the step exits 0 having done nothing (pipeline 520).
        for command in step.get("commands") or []:
            for line in str(command).splitlines():
                # `exec` as any command on the line, not only the first.
                if re.search(r"(?:^|[;&|(])\s*exec\b.*<", line.split("#", 1)[0]):
                    problems.append(f"{CODE_VITALS}: step {name} redirects the shell's stdin")
        password = (step.get("environment") or {}).get("CODE_VITALS_WRITER_PASSWORD")
        if password != {"from_secret": "code_vitals_prod_writer_password"}:
            problems.append(f"{CODE_VITALS}: step {name} does not take the writer password from the broker")
    for path, other in workflows.items():
        if path != CODE_VITALS and NAME in as_list((other or {}).get("depends_on")):
            problems.append(f"{path}: depends on {NAME}")
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
report(not problems, "code-vitals runs on master pushes only and can never block a build")


def step(w):
    (only,) = w[CODE_VITALS]["steps"].values()
    return only


def mutated(change):
    copied = copy.deepcopy(workflows)
    change(copied)
    return check(copied)


def on_pull_requests(w):
    w[CODE_VITALS]["when"] = [{"event": ["push", "manual", "pull_request"], "branch": "master"}]


def any_branch(w):
    w[CODE_VITALS]["when"] = [{"event": ["push", "manual"]}]


def can_fail(w):
    step(w).pop("failure")


def second_step(w):
    w[CODE_VITALS]["steps"]["more"] = dict(step(w))


def empty_when(w):
    w[CODE_VITALS]["when"] = []


def waits_on_others(w):
    w[CODE_VITALS]["depends_on"] = ["checks"]


def step_waits(w):
    step(w)["depends_on"] = ["setup"]


def awaited(w):
    w[".woodpecker/verify-tag-deploy.yml"]["depends_on"] = ["checks", NAME]


def takes_stdin(w):
    step(w)["commands"] = ["set -eu\nexec < /dev/null\n" + step(w)["commands"][0]]


def takes_stdin_mid_line(w):
    step(w)["commands"] = ["set -eu; exec < /dev/null\n" + step(w)["commands"][0]]


def inline_password(w):
    step(w)["environment"]["CODE_VITALS_WRITER_PASSWORD"] = "hunter2"


for name, change in [
    ("a pull request trigger", on_pull_requests),
    ("a workflow not limited to master", any_branch),
    ("an empty when, which runs on every event", empty_when),
    ("a workflow that waits on others", waits_on_others),
    ("a step with depends_on", step_waits),
    ("a step that can fail the pipeline", can_fail),
    ("a second step", second_step),
    ("another workflow waiting on it", awaited),
    ("an inline password", inline_password),
    ("a step that redirects the shell's stdin", takes_stdin),
    ("the same after another command on the line", takes_stdin_mid_line),
]:
    report(bool(mutated(change)), f"refuses {name}")

sys.exit(1 if failed else 0)
PY
