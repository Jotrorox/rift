"""Check that CI reruns exactly the failed tests and reports flaky ones."""

import unittest

from scripts import ci_test

FIRST = """\
     Running unittests src/lib.rs (target/release/deps/rift-93f438365e669b77)
test auth::tests::ok_one ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
     Running unittests src/main.rs (target/release/deps/rift-0a1b2c3d4e5f6071)
test network::tests::passes ... ok
test network::tests::queue_command ... {"event":"backend_attempt_failed"}
FAILED

failures:

---- network::tests::queue_command stdout ----

thread 'network::tests::queue_command' (57909) panicked at src/network/tests.rs:559:9:
assertion failed: contains(b"Use the queue")

failures:
    network::tests::queue_command

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 38.80s

\x1b[1m\x1b[91merror\x1b[0m: test failed, to rerun pass `-p rift --bin rift`
     Running tests/managed.rs (target/release/deps/managed-1122334455667788)
test removal_interrupts_startup ... FAILED
test other_failure ... FAILED

failures:
    other_failure
    removal_interrupts_startup

test result: FAILED. 30 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 14.41s

error: test failed, to rerun pass `-p rift --test managed`
     Running tests/nodes.rs (target/release/deps/nodes-99aabbccddeeff00)
error: test failed, to rerun pass `-p rift --test nodes`

Caused by:
  process didn't exit successfully: `target/release/deps/nodes-99aabbccddeeff00` (signal: 11, SIGSEGV)
error: 3 targets failed:
    `-p rift --bin rift`
    `-p rift --test managed`
    `-p rift --test nodes`
"""

STILL_FAILING = """\
     Running tests/managed.rs (target/release/deps/managed-1122334455667788)
test other_failure ... FAILED
test removal_interrupts_startup ... ok

failures:
    other_failure

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 30 filtered out; finished in 0.41s

error: test failed, to rerun pass `-p rift --test managed`
"""


class Cargo:
    """Replays recorded `cargo test` output, one entry per expected command."""

    def __init__(self, *results):
        self.results = list(results)
        self.commands = []

    def __call__(self, command):
        self.commands.append(command)
        code, output = self.results.pop(0)
        return code, output.splitlines(keepends=True)


class CiTestTests(unittest.TestCase):
    def test_failures_are_grouped_by_the_target_cargo_names(self):
        self.assertEqual(
            ci_test.parse_failures(FIRST.splitlines(keepends=True)),
            {
                "-p rift --bin rift": ["network::tests::queue_command"],
                "-p rift --test managed": ["removal_interrupts_startup", "other_failure"],
                "-p rift --test nodes": [],
            },
        )

    def test_reruns_keep_the_profile_and_drop_the_original_selection(self):
        self.assertEqual(
            ci_test.build_arguments(
                ["--release", "--workspace", "--exclude", "rift", "-p", "x", "--features", "a", "--all-targets", "--test=y"]
            ),
            ["--release", "--features", "a"],
        )

    def test_a_passing_run_is_not_repeated(self):
        cargo = Cargo((0, ""))
        self.assertEqual(ci_test.test(["--release", "--all-targets"], 2, run=cargo), (0, [], []))
        self.assertEqual(
            cargo.commands,
            [["cargo", "test", "--locked", "--no-fail-fast", "--release", "--all-targets", "--", "--test-threads=1"]],
        )

    def test_a_build_failure_is_not_retried(self):
        cargo = Cargo((101, "error[E0425]: cannot find value `x` in this scope\n"))
        self.assertEqual(ci_test.test(["--lib"], 2, run=cargo), (101, [], []))
        self.assertEqual(len(cargo.commands), 1)

    def test_only_failed_tests_are_rerun_and_recoveries_are_flaky(self):
        cargo = Cargo((101, FIRST), (0, ""), (101, STILL_FAILING), (0, ""), (0, ""))
        code, flaky, broken = ci_test.test(["--release", "--all-targets"], 2, run=cargo)
        self.assertEqual((code, broken), (0, []))
        self.assertEqual(
            flaky,
            [
                ("-p rift --bin rift", "network::tests::queue_command", 1),
                ("-p rift --test managed", "removal_interrupts_startup", 1),
                ("-p rift --test nodes", "(whole target)", 1),
                ("-p rift --test managed", "other_failure", 2),
            ],
        )
        self.assertEqual(
            cargo.commands[1:],
            [
                ["cargo", "test", "--locked", "--release", "-p", "rift", "--bin", "rift",
                 "--", "--exact", "network::tests::queue_command", "--test-threads=1"],
                ["cargo", "test", "--locked", "--release", "-p", "rift", "--test", "managed",
                 "--", "--exact", "removal_interrupts_startup", "other_failure", "--test-threads=1"],
                ["cargo", "test", "--locked", "--release", "-p", "rift", "--test", "nodes",
                 "--", "--test-threads=1"],
                ["cargo", "test", "--locked", "--release", "-p", "rift", "--test", "managed",
                 "--", "--exact", "other_failure", "--test-threads=1"],
            ],
        )

    def test_a_test_that_never_passes_fails_the_job(self):
        cargo = Cargo((101, STILL_FAILING), (101, STILL_FAILING), (101, STILL_FAILING))
        code, flaky, broken = ci_test.test(["--test", "managed"], 2, run=cargo)
        self.assertEqual((code, flaky), (1, []))
        self.assertEqual(broken, [("-p rift --test managed", "other_failure")])
        self.assertEqual(len(cargo.commands), 3)


if __name__ == "__main__":
    unittest.main()
