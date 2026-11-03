#!/usr/bin/env python3
"""Offline tests of check_ci_name_limits.py on small generated checkouts.

Each test writes the harness files the script models and a few test sources into a temporary
directory, runs the script on it as CI does, and checks its findings and exit status.

Run from anywhere: python3 -m unittest discover -s .github/scripts -p 'test_*.py'
"""

import json
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

# the script under test, beside this file
SCRIPT = Path(__file__).resolve().parent / "check_ci_name_limits.py"
# the harness sources the script checks its model against, as path -> content
HARNESS = {
    "api/src/conf.rs": "pub const NAMESPACE_MAX_LEN: usize = 48;\n",
    "api/src/test_utilities/api.rs": textwrap.dedent(
        """\
        pub const TEST_NAMESPACE_PREFIX: &str = "testing_";
        pub fn generated_namespace(epoch_minutes: u64, random: u32) -> String {
            format!("{TEST_NAMESPACE_PREFIX}ci_{epoch_minutes:07x}_{random:08x}")
        }
        """
    ),
    "thoradm/tests/common/base_namespace.rs": textwrap.dedent(
        """\
        pub fn generated(epoch_minutes: u64, random: u32) -> String {
            format!("{TEST_PREFIX}ci_{epoch_minutes:07x}_{random:08x}")
        }
        pub fn base_namespace() -> String { String::new() }
        """
    ),
    "thoradm/tests/common/harness.rs": textwrap.dedent(
        """\
        pub const BUCKET_ROLES: &[&str] = &[
            "files",
            "reaction_cache",
        ];
        fn is_stamped(run: &str) -> bool {
            run.split_once('-').is_some_and(|(stamp, random)| {
                hex(stamp, 7) && hex(random, 5)
            })
        }
        pub fn namespace_conf(id: &str) {
            conf.thorium.namespace = format!("testing_{id}");
            set_bucket(
                format!(
                    "testing-{}-{}",
                    id.replace('_', "-"),
                    role.replace('_', "-")
                ),
            );
        }
        pub async fn start() {
            let id = format!("{}_{}", run_id(), binary_name());
        }
        """
    ),
}
# a thoradm test binary name of the longest length that fits: testing_<13>_<26> is 48 characters
# and testing-<13>-<26>-reaction-cache is 63
LONGEST_BINARY = "a" * 26


class NameLimitsTest(unittest.TestCase):
    """Runs the script on generated checkouts."""

    def setUp(self):
        """Make an empty checkout holding only the harness files."""
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        for rel, content in HARNESS.items():
            self.write(rel, content)

    def tearDown(self):
        """Remove the checkout."""
        self.tmp.cleanup()

    def write(self, rel, content):
        """Write one file into the checkout.

        # Arguments

        * `rel` - The path relative to the checkout
        * `content` - The file's content
        """
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(content), encoding="utf-8")

    def run_check(self, *extra):
        """Run the script on the checkout and return the finished process.

        # Arguments

        * `extra` - More command line arguments
        """
        return subprocess.run(
            [sys.executable, "-I", str(SCRIPT), "--root", str(self.root), *extra],
            capture_output=True,
            text=True,
            check=False,
        )

    def errors(self, result):
        """Return the finding lines of a run's output.

        # Arguments

        * `result` - The finished process
        """
        return [line for line in result.stdout.splitlines() if ": error[" in line]

    def test_clean_checkout_passes(self):
        """A checkout whose names all fit exits 0 with no findings."""
        self.write(f"thoradm/tests/{LONGEST_BINARY}.rs", "#[test]\nfn works() {}\n")
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.errors(result), [])

    def test_long_thoradm_binary_fails(self):
        """A thoradm test target one character too long fails for its keyspace and bucket."""
        self.write(f"thoradm/tests/{LONGEST_BINARY}b.rs", "#[test]\nfn works() {}\n")
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        errors = self.errors(result)
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(errors[0].startswith(f"thoradm/tests/{LONGEST_BINARY}b.rs:1: error["), errors)
        self.assertIn("error[keyspace-too-long]", result.stdout)
        self.assertIn("49 characters", result.stdout)
        self.assertIn("error[bucket-too-long]", result.stdout)
        self.assertIn("64 characters", result.stdout)

    def test_renamed_cargo_target_is_measured(self):
        """A `[[test]]` in thoradm's Cargo.toml names the binary instead of the file."""
        self.write("thoradm/tests/short.rs", "#[test]\nfn works() {}\n")
        self.write("thoradm/Cargo.toml", f'[[test]]\nname = "{LONGEST_BINARY}cc"\npath = "tests/short.rs"\n')
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(f"{LONGEST_BINARY}cc", result.stdout)

    def test_helper_suffix_is_measured_at_each_call(self):
        """A suffix passed to a keyspace helper fails at the call that makes it too long."""
        self.write(
            "api/tests/schema.rs",
            """\
            async fn fresh(session: &Session, suffix: &str) -> Conf {
                let keyspace = format!("{}_{suffix}", CONF.thorium.namespace);
                conf(keyspace)
            }

            #[tokio::test]
            async fn fits() {
                fresh(&session, "twenty_characters_ok").await;
            }

            #[tokio::test]
            async fn too_long() {
                fresh(&session, "twenty_one_characters").await;
            }
            """,
        )
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        errors = self.errors(result)
        self.assertEqual(len(errors), 1, errors)
        self.assertTrue(errors[0].startswith("api/tests/schema.rs:13: error[keyspace-too-long]"), errors)
        self.assertIn("49 characters", errors[0])

    def test_forwarded_suffix_through_shared_module(self):
        """A suffix forwarded through a shared helper's methods is measured at the outer call."""
        self.write(
            "thoradm/tests/common/world.rs",
            """\
            impl World {
                pub async fn new(suffix: &str) -> World {
                    World::build(suffix, false).await
                }
                async fn build(suffix: &str, full: bool) -> World {
                    let base = super::base_namespace::base_namespace();
                    let ns = format!("{base}_{suffix}");
                    World { ns }
                }
            }
            """,
        )
        self.write(
            "thoradm/tests/uses.rs",
            """\
            #[tokio::test]
            async fn a() {
                let world = World::new("twenty_characters_ok").await;
                let other = World::new(&format!("long_prefix_{}", "x")).await;
            }
            #[tokio::test]
            async fn b() {
                let world = World::new("twenty_one_characters").await;
            }
            """,
        )
        result = self.run_check()
        errors = self.errors(result)
        self.assertEqual(len(errors), 1, result.stdout)
        self.assertTrue(errors[0].startswith("thoradm/tests/uses.rs:8: error[keyspace-too-long]"), errors)

    def test_loop_values_and_nested_format(self):
        """Suffixes from a `for` over an array and from a `format!` of loop values are measured."""
        self.write(
            "api/tests/loops.rs",
            """\
            const SMALL: usize = 300;
            fn fresh(suffix: &str) -> String {
                format!("{}_{suffix}", CONF.thorium.namespace)
            }
            #[test]
            fn tuples() {
                for (suffix, expected) in [("short", 1), ("this_one_is_too_long_", 2)] {
                    fresh(suffix);
                }
            }
            #[test]
            fn nested() {
                for rows in [SMALL, SMALL * 4] {
                    fresh(&format!("rows_{rows}"));
                }
                for bad in [0_usize, 2] {
                    fresh(&format!("seventeen_chars_{bad}"));
                }
                let named = "abc";
                fresh(named);
            }
            """,
        )
        result = self.run_check("--list")
        errors = self.errors(result)
        self.assertEqual(len(errors), 1, result.stdout)
        self.assertTrue(errors[0].startswith("api/tests/loops.rs:8: error[keyspace-too-long]"), errors)
        # rows_1200 and the one-digit bad value both fit, and the let binding is followed
        self.assertIn("api/tests/loops.rs:14: keyspace 37/48", result.stdout)
        self.assertIn("api/tests/loops.rs:17: keyspace 45/48", result.stdout)
        self.assertIn("api/tests/loops.rs:20: keyspace 31/48", result.stdout)

    def test_thoradm_harness_namespace_uses_the_binary(self):
        """A keyspace built from a thoradm harness namespace counts the file's binary name."""
        self.write(
            "thoradm/tests/wipe.rs",
            """\
            #[tokio::test]
            async fn neighbors() {
                let ns = harness.conf.thorium.namespace.clone();
                let fits = format!("{ns}_twenty_two_chars_fit");
                let long = format!("{ns}_twenty_three_chars_long");
                let dev = format!("{ns}_dev");
                let bucket = format!("{}-files", dev.replace('_', "-"));
                let key = format!("{ns}_this_is_a_redis_key_not_a_keyspace:marker");
            }
            """,
        )
        result = self.run_check("--list")
        errors = self.errors(result)
        self.assertEqual(len(errors), 1, result.stdout)
        self.assertTrue(errors[0].startswith("thoradm/tests/wipe.rs:5: error[keyspace-too-long]"), errors)
        # testing_<13>_wipe_dev is 30 characters, and its bucket adds -files
        self.assertIn("thoradm/tests/wipe.rs:7: bucket 36/63", result.stdout)
        self.assertNotIn("wipe.rs:8:", result.stdout)

    def test_bucket_with_role_and_slice(self):
        """A bucket built from the namespace counts the longest role and fixed-length slices."""
        self.write(
            "api/tests/harness.rs",
            """\
            fn isolated(suffix: &str) -> Conf {
                let unique = uuid::Uuid::new_v4().simple().to_string();
                let prefix = CONF.thorium.namespace.replace('_', "-");
                let bucket = |role: &str| format!("{prefix}-{suffix}-{role}-{}", &unique[..8]);
                conf
            }
            #[test]
            fn a() {
                isolated("fourteen_chars");
                isolated("fifteen_chars_x");
            }
            """,
        )
        result = self.run_check()
        errors = self.errors(result)
        self.assertEqual(len(errors), 1, result.stdout)
        self.assertTrue(errors[0].startswith("api/tests/harness.rs:10: error[bucket-too-long]"), errors)
        self.assertIn("64 characters", errors[0])

    def test_unmeasurable_suffix_is_reported(self):
        """A suffix whose value can't be found statically is an error asking for a literal."""
        self.write(
            "api/tests/unknown.rs",
            """\
            fn fresh(suffix: &str) -> String {
                format!("{}_{suffix}", CONF.thorium.namespace)
            }
            #[test]
            fn a() {
                fresh(&uuid::Uuid::new_v4().to_string());
            }
            """,
        )
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("api/tests/unknown.rs:6: error[unmeasured]", result.stdout)

    def test_stale_model_is_reported(self):
        """A harness that no longer builds names as modelled fails until the script is updated."""
        self.write("thoradm/tests/common/harness.rs", HARNESS["thoradm/tests/common/harness.rs"].replace("7", "8"))
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("thoradm/tests/common/harness.rs:1: error[stale-model]", result.stdout)

    def test_namespace_limit_is_read_from_the_config(self):
        """The keyspace limit follows NAMESPACE_MAX_LEN in api/src/conf.rs."""
        self.write("api/src/conf.rs", "pub const NAMESPACE_MAX_LEN: usize = 40;\n")
        self.write("thoradm/tests/eighteen_characters.rs", "#[test]\nfn works() {}\n")
        result = self.run_check()
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("over 40", result.stdout)

    def test_comments_and_strings_are_not_code(self):
        """A `format!` or call inside a comment or string is not measured."""
        self.write(
            "api/tests/comments.rs",
            """\
            fn fresh(suffix: &str) -> String {
                // format!("{}_way_too_long_for_any_keyspace_at_all", CONF.thorium.namespace)
                format!("{}_{suffix}", CONF.thorium.namespace)
            }
            #[test]
            fn a() {
                /* fresh("way_too_long_for_any_keyspace_at_all"); */
                let text = "fresh(\\"way_too_long_for_any_keyspace_at_all\\")";
                fresh("ok");
            }
            """,
        )
        result = self.run_check()
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_sarif_output(self):
        """`--sarif` writes every finding with its rule, level, file, and line."""
        self.write(f"thoradm/tests/{LONGEST_BINARY}b.rs", "#[test]\nfn works() {}\n")
        sarif = self.root / "out" / "names.sarif"
        result = self.run_check("--sarif", str(sarif))
        self.assertEqual(result.returncode, 1, result.stdout)
        log = json.loads(sarif.read_text(encoding="utf-8"))
        results = log["runs"][0]["results"]
        self.assertEqual({item["ruleId"] for item in results}, {"keyspace-too-long", "bucket-too-long"})
        location = results[0]["locations"][0]["physicalLocation"]
        self.assertEqual(location["artifactLocation"]["uri"], f"thoradm/tests/{LONGEST_BINARY}b.rs")
        self.assertEqual(location["region"]["startLine"], 1)
        self.assertTrue(all(item["level"] == "error" for item in results))


if __name__ == "__main__":
    unittest.main()
