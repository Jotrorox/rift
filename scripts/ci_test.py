#!/usr/bin/env python3
"""Run `cargo test` for CI and rerun only the tests that failed.

Many tests here hold real sockets, child processes and wall-clock deadlines,
and a shared runner sometimes stalls one of them for longer than it allows. A
test that fails is run again, alone, up to `--retries` times:

* a test that fails every time fails the job, as before;
* a test that passes on a rerun is reported as flaky (a warning annotation and
  a line in the job summary) so it can be fixed, without failing the job.

Nothing is skipped: the first run executes every selected test, serially.

    python scripts/ci_test.py --release -p rift --lib
"""

import argparse
import os
import re
import subprocess
import sys

ANSI = re.compile(r"\x1b\[[0-9;]*m")
RERUN = re.compile(r"`((?:-p|--package|--lib|--bin|--test|--bench|--example|--doc)\b[^`]*)`")
FAILED_LINE = re.compile(r"^test (\S+) \.\.\. FAILED$")
FAILED_OUTPUT = re.compile(r"^---- (\S+) stdout ----$")
FAILED_LISTED = re.compile(r"^    (\S+)$")

# Options that choose what to test. A rerun names its target itself, so these
# are dropped from the original arguments; everything else (profile, features)
# is kept so the rerun uses the binaries that are already built.
SELECTION_FLAGS = {
    "--workspace", "--all", "--lib", "--bins", "--tests", "--benches",
    "--examples", "--all-targets", "--doc",
}
SELECTION_OPTIONS = {
    "-p", "--package", "--exclude", "--bin", "--test", "--bench", "--example",
}


def build_arguments(arguments):
    """The arguments that do not select packages or targets."""
    kept, skip = [], False
    for argument in arguments:
        if skip:
            skip = False
        elif argument in SELECTION_OPTIONS:
            skip = True
        elif argument in SELECTION_FLAGS or argument.split("=")[0] in SELECTION_OPTIONS:
            continue
        else:
            kept.append(argument)
    return kept


def parse_failures(lines):
    """Map each failed target (its `cargo test` arguments) to its failed tests.

    A target with no named test (the test process itself died) maps to an empty
    list, which reruns the whole target.
    """
    failures, names, listing = {}, [], False
    for raw in lines:
        line = ANSI.sub("", raw.rstrip("\r\n"))
        stripped = line.strip()
        if stripped.startswith("Running ") or stripped.startswith("Doc-tests "):
            names, listing = [], False
            continue
        target = RERUN.search(line) if "error" in line or line.startswith("    `") else None
        if target:
            known = failures.setdefault(target.group(1).strip(), [])
            known.extend(name for name in names if name not in known)
            names, listing = [], False
            continue
        if line == "failures:":
            listing = True
            continue
        match = FAILED_LINE.match(line) or FAILED_OUTPUT.match(line)
        if not match and listing:
            match = FAILED_LISTED.match(line)
            if not match and stripped and not stripped.startswith("----"):
                listing = False
        if match and match.group(1) not in names:
            names.append(match.group(1))
    return failures


def run(command):
    """Run a command, echo its output and return (exit code, output lines)."""
    print("+ " + " ".join(command), flush=True)
    process = subprocess.Popen(
        command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8", errors="replace",
    )
    lines = []
    for line in process.stdout:
        sys.stdout.write(line)
        sys.stdout.flush()
        lines.append(line)
    return process.wait(), lines


def test(arguments, retries, run=run):
    """Returns (exit code, flaky tests, tests that never passed)."""
    code, lines = run(
        ["cargo", "test", "--locked", "--no-fail-fast", *arguments, "--", "--test-threads=1"]
    )
    if code == 0:
        return 0, [], []
    pending = parse_failures(lines)
    if not pending:
        # Not a test failure (it did not compile, or cargo itself failed).
        return code, [], []
    base, flaky = build_arguments(arguments), []
    for attempt in range(1, retries + 1):
        remaining = {}
        for target, names in pending.items():
            print(f"\n=== Retry {attempt}/{retries}: {target} {' '.join(names)}", flush=True)
            filters = ["--exact", *names] if names else []
            code, lines = run(
                ["cargo", "test", "--locked", *base, *target.split(), "--", *filters, "--test-threads=1"]
            )
            if code == 0:
                still = []
            elif not names:
                still = None
            else:
                again = {name for failed in parse_failures(lines).values() for name in failed}
                # Without a parsed name, assume everything rerun failed again.
                still = [name for name in names if name in again] or names
            if still is None:
                remaining[target] = []
                continue
            if still:
                remaining[target] = still
            flaky.extend(
                (target, name, attempt)
                for name in (names or ["(whole target)"])
                if name not in still
            )
        pending = remaining
        if not pending:
            break
    broken = [(target, name) for target, names in pending.items() for name in (names or ["(whole target)"])]
    return (1 if broken else 0), flaky, broken


def report(flaky, broken, retries):
    summary = []
    for target, name, attempt in flaky:
        print(f"::warning title=Flaky test::{name} ({target}) failed, then passed on retry {attempt}")
        summary.append(f"- :warning: flaky: `{name}` (`{target}`) passed on retry {attempt}")
    for target, name in broken:
        print(f"::error title=Test failed::{name} ({target}) failed {retries + 1} times")
        summary.append(f"- :x: failed {retries + 1} times: `{name}` (`{target}`)")
    path = os.environ.get("GITHUB_STEP_SUMMARY")
    if path and summary:
        with open(path, "a", encoding="utf-8") as output:
            output.write("\n".join(summary) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--retries", type=int, default=2, help="reruns of a failed test (default 2)")
    options, arguments = parser.parse_known_args()
    code, flaky, broken = test(arguments, options.retries)
    report(flaky, broken, options.retries)
    return code


if __name__ == "__main__":
    sys.exit(main())
