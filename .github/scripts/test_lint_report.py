#!/usr/bin/env python3
"""Offline tests of lint_report.py's job log output, annotations, exit status, and SARIF output.

Each test runs the script as CI does, on a generated input in a temporary
directory, with a GitHub or GitLab environment and nothing else from the caller.

Run from anywhere: python3 -m unittest discover -s .github/scripts -p 'test_*.py'
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

# the script under test, beside this file
SCRIPT = Path(__file__).resolve().parent / "lint_report.py"
# more findings than GitHub's annotation cap of 10 per level, so some go unannotated
FINDING_COUNT = 13
# the clippy lint the generated findings report
RULE = "clippy::unnecessary_lazy_evaluations"
# the message the generated findings carry
MESSAGE = "unnecessary closure used to substitute value for `Option::None`"


def cargo_message(path, line):
    """Build one cargo `compiler-message` JSON line for a clippy warning.

    # Arguments

    * `path` - The repo-relative file the warning is in
    * `line` - The line the warning is on
    """
    rendered = f"warning: {MESSAGE}\n  --> {path}:{line}:9\n   |\n{line} |     value.ok_or_else(|| Error::Missing)\n"
    span = {"file_name": path, "line_start": line, "column_start": 9, "is_primary": True}
    message = {"level": "warning", "message": MESSAGE, "code": {"code": RULE}, "spans": [span], "rendered": rendered}
    return json.dumps({"reason": "compiler-message", "message": message})


def finding_location(index):
    """Return the `(path, line)` of the generated finding at an index.

    # Arguments

    * `index` - The finding's index
    """
    return f"api/src/file{index:02}.rs", 10 + index


class LintReportLog(unittest.TestCase):
    """The job log names every finding's location, beyond the annotation cap too."""

    def setUp(self):
        """Create a temporary repository root with cargo output holding FINDING_COUNT warnings."""
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        lines = [cargo_message(*finding_location(index)) for index in range(FINDING_COUNT)]
        lines.append(json.dumps({"reason": "build-finished", "success": True}))
        self.input = self.root / "clippy.json"
        self.input.write_text("\n".join(lines) + "\n", encoding="utf-8")

    def tearDown(self):
        """Remove the temporary repository root."""
        self.tmp.cleanup()

    def run_report(self, platform, input_path=None, input_format="cargo", extra=()):
        """Run lint_report.py as a CI job on one platform would, and return the finished process.

        # Arguments

        * `platform` - `github` or `gitlab`
        * `input_path` - The tool output to read, or None for the generated cargo output
        * `input_format` - The `--format` of the input
        * `extra` - More command line arguments
        """
        env = {"PATH": os.environ.get("PATH", "")}
        if platform == "github":
            summary = self.root / "summary.md"
            env.update(GITHUB_ACTIONS="true", GITHUB_EVENT_NAME="push", GITHUB_WORKSPACE=str(self.root), GITHUB_STEP_SUMMARY=str(summary))
        else:
            env.update(GITLAB_CI="true", CI_PROJECT_DIR=str(self.root))
        command = [sys.executable, "-I", str(SCRIPT), "--tool", "clippy", "--format", input_format]
        command += ["--input", str(input_path or self.input), "--output-dir", str(self.root / "out"), "--fail-level", "warning", *extra]
        return subprocess.run(command, cwd=self.root, env=env, capture_output=True, text=True, check=False)

    def expected_lines(self):
        """Return the plain log line expected for each generated finding."""
        return [f"{path}:{line}:9: warning[{RULE}]: {MESSAGE}" for path, line in map(finding_location, range(FINDING_COUNT))]

    def test_github_log_lists_every_location_outside_groups(self):
        """GitHub: every finding, past the annotation cap, has a plain location line outside any group."""
        result = self.run_report("github")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        out = result.stdout.splitlines()
        # the plain lines are printed before the first group opens, so they show without expanding anything
        before_group = out[: out.index(f"::group::clippy findings ({FINDING_COUNT})")]
        for line in self.expected_lines():
            self.assertIn(line, before_group)

    def test_github_keeps_capped_annotations_and_summary(self):
        """GitHub: annotations stay capped at 10 per level and the job summary still counts every finding."""
        result = self.run_report("github")
        annotations = [line for line in result.stdout.splitlines() if line.startswith("::warning ")]
        self.assertEqual(len(annotations), 10)
        self.assertIn("file=api/src/file00.rs,title=clippy", annotations[0])
        summary = (self.root / "summary.md").read_text(encoding="utf-8")
        self.assertIn(str(FINDING_COUNT), summary)
        self.assertIn(f"clippy: {FINDING_COUNT} findings (0 error, {FINDING_COUNT} warning, 0 note), 0 ignored", result.stdout)

    def test_github_groups_rendered_diagnostics(self):
        """GitHub: rustc's rendered diagnostics are in a collapsible group with workflow commands paused."""
        out = self.run_report("github").stdout.splitlines()
        start = out.index(f"::group::clippy findings ({FINDING_COUNT})")
        end = out.index("::endgroup::")
        group = out[start:end]
        self.assertTrue(group[1].startswith("::stop-commands::"))
        self.assertIn("  --> api/src/file12.rs:22:9", group)

    def test_github_plain_lines_cannot_issue_commands(self):
        """GitHub: a message that looks like a workflow command is printed with workflow commands paused."""
        path = self.root / "evil.json"
        evil = json.loads(cargo_message("api/src/lib.rs", 1))
        evil["message"]["message"] = "x\n::error::forged"
        path.write_text(json.dumps(evil) + "\n", encoding="utf-8")
        out = self.run_report("github", input_path=path).stdout.splitlines()
        self.assertNotIn("::error::forged", out)
        index = out.index("api/src/lib.rs:1:9: warning[clippy::unnecessary_lazy_evaluations]: x ::error::forged")
        self.assertTrue(out[index - 1].startswith("::stop-commands::"))
        self.assertEqual(out[index + 1], "::" + out[index - 1].split("::")[2] + "::")

    def test_gitlab_log_lists_every_location_and_uses_findings_exit_code(self):
        """GitLab: every finding has a plain location line, the rendered output is in an open section, and findings exit 42."""
        result = self.run_report("gitlab", extra=("--findings-exit-code", "42"))
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        out = result.stdout.splitlines()
        for line in self.expected_lines():
            self.assertIn(line, out)
        self.assertNotIn("::group::", result.stdout)
        self.assertFalse(any(line.startswith("::warning") for line in out))
        start = next(line for line in out if "section_start:" in line and ":clippy_findings" in line)
        self.assertNotIn("[collapsed=true]", start)
        self.assertIn("  --> api/src/file12.rs:22:9", out)

    def test_clean_input_passes(self):
        """A run with no findings prints no location lines and exits 0."""
        path = self.root / "clean.json"
        path.write_text(json.dumps({"reason": "build-finished", "success": True}) + "\n", encoding="utf-8")
        for platform in ("github", "gitlab"):
            result = self.run_report(platform, input_path=path, extra=("--findings-exit-code", "42"))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertNotIn("warning[", result.stdout)

    def test_sarif_findings_get_plain_lines(self):
        """SARIF input gets the same plain location line per finding."""
        path = self.root / "scan.sarif"
        result = {"ruleId": "DL3008", "level": "warning", "message": {"text": "Pin versions in apt get install"}}
        result["locations"] = [{"physicalLocation": {"artifactLocation": {"uri": "Dockerfile"}, "region": {"startLine": 4, "startColumn": 1}}}]
        run = {"tool": {"driver": {"name": "hadolint", "rules": [{"id": "DL3008"}]}}, "results": [result]}
        path.write_text(json.dumps({"version": "2.1.0", "runs": [run]}), encoding="utf-8")
        out = self.run_report("gitlab", input_path=path, input_format="sarif").stdout.splitlines()
        self.assertIn("Dockerfile:4:1: warning[DL3008]: Pin versions in apt get install", out)

    def write_actionlint(self):
        """Write actionlint JSON holding one expression error and one ShellCheck info finding, and return its path."""
        path = self.root / "actionlint.json"
        expression = {"message": 'property "foo" is not defined', "filepath": ".github/workflows/ci.yml", "line": 6, "column": 23, "kind": "expression", "snippet": "  ${{ github.foo }}", "end_column": 32}
        shellcheck = {"message": "shellcheck reported issue in this script: SC2086:info:1:6: Double quote to prevent globbing", "filepath": ".github/workflows/ci.yml", "line": 9, "column": 9, "kind": "shellcheck", "snippet": "  run: |", "end_column": 12}
        path.write_text(json.dumps([expression, shellcheck]), encoding="utf-8")
        return path

    def test_actionlint_checks_are_errors_and_shellcheck_keeps_its_severity(self):
        """actionlint input: its own checks are errors that fail an error gate, and ShellCheck info findings are notes."""
        path = self.write_actionlint()
        result = self.run_report("gitlab", input_path=path, input_format="actionlint", extra=("--tool", "actionlint", "--fail-level", "error"))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        out = result.stdout.splitlines()
        self.assertIn('.github/workflows/ci.yml:6:23: error[expression]: property "foo" is not defined', out)
        self.assertIn(".github/workflows/ci.yml:9:9: note[SC2086]: shellcheck reported issue in this script: SC2086:info:1:6: Double quote to prevent globbing", out)
        # a note alone does not meet an error gate
        clean = self.root / "notes.json"
        clean.write_text(json.dumps(json.loads(path.read_text(encoding="utf-8"))[1:]), encoding="utf-8")
        result = self.run_report("gitlab", input_path=clean, input_format="actionlint", extra=("--tool", "actionlint", "--fail-level", "error"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_non_sarif_input_is_written_as_sarif(self):
        """--sarif-output turns non-SARIF findings into a SARIF log code scanning can map to files."""
        sarif = self.root / "out" / "actionlint-filtered.sarif"
        extra = ("--tool", "actionlint", "--fail-level", "none", "--sarif-output", str(sarif))
        result = self.run_report("github", input_path=self.write_actionlint(), input_format="actionlint", extra=extra)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        log = json.loads(sarif.read_text(encoding="utf-8"))
        run = log["runs"][0]
        self.assertEqual(log["version"], "2.1.0")
        self.assertEqual(run["tool"]["driver"]["name"], "actionlint")
        self.assertEqual({rule["id"] for rule in run["tool"]["driver"]["rules"]}, {"expression", "SC2086"})
        self.assertEqual([(result["ruleId"], result["level"]) for result in run["results"]], [("expression", "error"), ("SC2086", "note")])
        location = run["results"][0]["locations"][0]["physicalLocation"]
        self.assertEqual(location, {"artifactLocation": {"uri": ".github/workflows/ci.yml"}, "region": {"startLine": 6, "startColumn": 23}})
        # cargo input converts the same way
        result = self.run_report("gitlab", extra=("--fail-level", "none", "--sarif-output", str(sarif)))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(json.loads(sarif.read_text(encoding="utf-8"))["runs"][0]["results"]), FINDING_COUNT)


if __name__ == "__main__":
    unittest.main(verbosity=2)
