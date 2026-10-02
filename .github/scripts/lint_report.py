#!/usr/bin/env python3
"""Turn linter and scanner output into reviewer-facing CI reports.

Supported inputs (`--format`):

* `sarif` - a SARIF 2.1.0 log, such as zizmor, hadolint or Trivy output
* `cargo` - the JSON lines written by `cargo check/clippy --message-format=json`
* `rustfmt` - the diff written by `cargo fmt --check`

The same script runs in GitHub Actions, GitLab CI, and locally. Every finding,
at every level, is reported:

* in the job log, as one readable line (or rustc's rendered diagnostic) per
  finding, in a collapsible group (GitHub) or section (GitLab)
* inline: as workflow annotations on GitHub, which show on the run and on a
  pull request's changed files (GitHub keeps only 10 per level per step, so
  findings in changed files come first), and as a GitLab Code Quality report
  with `--codequality`, which GitLab shows on the merge request
* as a summary of counts by level and by rule plus a table of findings, in the
  GitHub job summary or the GitLab job log
* in `<tool>-report.md` and `<tool>-report.txt` in `--output-dir`, for upload
  as an artifact

Files changed by the change under review are marked and listed first. They are
found with git: on GitHub pull_request runs against the merge commit's base
(`HEAD^1`, so the checkout needs `fetch-depth: 2`), and on GitLab against
CI_MERGE_REQUEST_DIFF_BASE_SHA in merge request pipelines or
CI_COMMIT_BEFORE_SHA in push pipelines (PARENT_COMMIT_BEFORE_SHA in child
pipelines, set by the parent's trigger job). When git or the commit is unavailable
the report is produced without that breakdown.

Findings matching an entry in `--ignore-file` (see .github/lint-ignore.toml)
are left out of all of those, out of the gate, and out of `--sarif-output`, the
filtered SARIF uploaded to code scanning. The summary lists each ignore entry
with how many findings it hid, and `<tool>-ignored.txt` lists them for audit.
On GitHub pull requests and GitLab merge request pipelines the ignore file is
read from the base commit, so a change cannot hide its own findings; ignores
take effect once merged. Other runs read it from the commit being checked.

Scanner output describes code under review, so every message is escaped before
it is used in a workflow command, the readable GitHub log section is wrapped in
`::stop-commands::` so it cannot issue workflow commands, and control
characters are stripped from GitLab log output so it cannot forge sections.

This script uses only the Python standard library so it adds no dependencies.

Exit status is `--findings-exit-code` (default 1) when any reported finding is
at or above `--fail-level` or has a severity listed in `--fail-severity`, 2
when the ignore file is invalid, and 0 otherwise. The default level, `none`,
never fails, for jobs that gate on the tool's own exit code instead.
"""

import argparse
import datetime
import fnmatch
import hashlib
import json
import os
import re
import secrets
import subprocess
import sys
import time
import tomllib
from collections import Counter
from pathlib import Path

# finding levels from most to least severe, matching SARIF's level names
LEVELS = ("error", "warning", "note")
# the workflow command used to annotate each level
ANNOTATION_COMMANDS = {"error": "error", "warning": "warning", "note": "notice"}
# GitHub keeps at most this many annotations of each level per step
MAX_ANNOTATIONS_PER_LEVEL = 10
# the most findings listed in the job summary table; the artifact has all of them
MAX_SUMMARY_ROWS = 300
# severities scanners such as Trivy record as SARIF rule tags
SEVERITY_TAGS = ("CRITICAL", "HIGH", "MEDIUM", "LOW", "UNKNOWN")
# GitLab Code Quality severity for each scanner severity or finding level
CODEQUALITY_SEVERITIES = {
    "CRITICAL": "critical",
    "HIGH": "major",
    "MEDIUM": "minor",
    "LOW": "info",
    "UNKNOWN": "info",
    "error": "major",
    "warning": "minor",
    "note": "info",
}
# terminal control characters, which could forge GitLab log sections
CONTROL_RE = re.compile(r"[\x00-\x08\x0b-\x1f\x7f]")
# a git commit id of all zeros, which GitLab uses when there is no previous commit
NULL_SHA_RE = re.compile(r"^0+$")
# rustc's closing tallies, which are not findings themselves
CARGO_TALLY_RE = re.compile(r"^(\d+ warnings? emitted|aborting due to)")
# the header rustfmt prints before each hunk of its diff
RUSTFMT_HUNK_RE = re.compile(r"^Diff in (?P<path>.+?):(?P<line>\d+):\s*$")


def escape_annotation(text):
    """Escape text for safe use in a GitHub workflow command message.

    # Arguments

    * `text` - The text to escape
    """
    # percent-encode the characters that would otherwise end or inject a command
    return text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def escape_property(text):
    """Escape text for safe use as a GitHub workflow command property value.

    # Arguments

    * `text` - The text to escape
    """
    # property values additionally need `:` and `,` encoded so they cannot add properties
    return escape_annotation(text).replace(":", "%3A").replace(",", "%2C")


def escape_cell(text):
    """Make text safe to place in a single markdown table cell.

    # Arguments

    * `text` - The text to escape
    """
    # keep the cell on one line and stop pipes from splitting it
    return " ".join(str(text).split()).replace("|", r"\|")


def relative_path(path, root):
    """Express a reported file path relative to the repository root.

    # Arguments

    * `path` - The path as reported by the tool, absolute or relative
    * `root` - The repository root
    """
    # strip file:// URIs, then drop the root prefix from absolute paths
    if path.startswith("file://"):
        path = path[len("file://"):]
    if os.path.isabs(path):
        try:
            return str(Path(path).resolve().relative_to(root))
        except ValueError:
            return path
    return path[2:] if path.startswith("./") else path


def rule_link(rule):
    """Build a documentation link for rules whose tool does not supply one.

    # Arguments

    * `rule` - The rule or lint identifier
    """
    # hadolint's own rules and the ShellCheck rules it embeds, then clippy lints
    if re.fullmatch(r"DL\d+", rule):
        return f"https://github.com/hadolint/hadolint/wiki/{rule}"
    if re.fullmatch(r"SC\d+", rule):
        return f"https://www.shellcheck.net/wiki/{rule}"
    if rule.startswith("clippy::"):
        return "https://rust-lang.github.io/rust-clippy/master/index.html#" + rule[len("clippy::"):]
    return ""


def parse_sarif(path, root):
    """Read findings from a SARIF log.

    # Arguments

    * `path` - The SARIF file
    * `root` - The repository root

    Returns a `(findings, suppressed_count)` pair.
    """
    log = json.loads(Path(path).read_text(encoding="utf-8"))
    findings = []
    suppressed = 0
    for run in log.get("runs", []):
        driver = run.get("tool", {}).get("driver", {})
        rules = {rule.get("id"): rule for rule in driver.get("rules", [])}
        for result in run.get("results", []):
            # suppressed results are counted but never reported or gated on
            if result.get("suppressions"):
                suppressed += 1
                continue
            rule_id = result.get("ruleId", "")
            rule = rules.get(rule_id, {})
            # SARIF's default level is warning, and `none` is informational
            level = result.get("level") or rule.get("defaultConfiguration", {}).get("level") or "warning"
            level = level if level in LEVELS else "note"
            # prefer an explicit severity tag (Trivy), otherwise the level
            tags = rule.get("properties", {}).get("tags", [])
            severity = next((tag for tag in tags if tag in SEVERITY_TAGS), level)
            # Trivy packs several `Key: value` lines into the message; keep its Message line
            text = result.get("message", {}).get("text", "")
            message = next((line[len("Message: "):] for line in text.splitlines() if line.startswith("Message: ")), text)
            location = (result.get("locations") or [{}])[0].get("physicalLocation", {})
            artifact = location.get("artifactLocation", {})
            # a uri with a base id (Trivy) is relative to the scan root, which is the
            # repo root here; the absolute base is not used, since the job that scanned
            # may have had the repo at a different path
            uri = artifact.get("uri", "")
            region = location.get("region", {})
            findings.append(
                {
                    "file": relative_path(uri, root) if uri else "",
                    "line": region.get("startLine", 0),
                    "column": region.get("startColumn", 0),
                    "level": level,
                    "severity": severity,
                    "rule": rule_id,
                    "message": message,
                    "help": rule.get("helpUri", ""),
                    "description": rule.get("shortDescription", {}).get("text", ""),
                    "rendered": "",
                    "result": result,
                }
            )
    return findings, suppressed


def parse_cargo(path, root):
    """Read diagnostics from cargo's `--message-format=json` output.

    # Arguments

    * `path` - The JSON lines file
    * `root` - The repository root

    Returns a `(findings, suppressed_count)` pair.
    """
    findings = []
    seen = set()
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        # cargo can interleave non-JSON lines, such as build script output
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if entry.get("reason") != "compiler-message":
            continue
        message = entry["message"]
        level = message.get("level", "")
        if level == "error: internal compiler error":
            level = "error"
        # skip rustc's closing tallies and anything that is not a diagnostic
        if level not in ("error", "warning", "note", "help") or CARGO_TALLY_RE.match(message.get("message", "")):
            continue
        level = level if level in LEVELS else "note"
        spans = message.get("spans", [])
        span = next((span for span in spans if span.get("is_primary")), spans[0] if spans else {})
        rule = (message.get("code") or {}).get("code", "")
        finding = {
            "file": relative_path(span.get("file_name", ""), root),
            "line": span.get("line_start", 0),
            "column": span.get("column_start", 0),
            "level": level,
            "severity": level,
            "rule": rule,
            "message": message.get("message", ""),
            "help": "",
            "description": "",
            "rendered": message.get("rendered") or "",
        }
        # the same diagnostic is reported once per target that compiles the file
        key = (finding["file"], finding["line"], finding["column"], rule, finding["message"])
        if key not in seen:
            seen.add(key)
            findings.append(finding)
    return findings, 0


def parse_rustfmt(path, root):
    """Read the hunks of a `cargo fmt --check` diff as findings.

    # Arguments

    * `path` - The diff file
    * `root` - The repository root

    Returns a `(findings, suppressed_count)` pair.
    """
    findings = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        # strip any terminal colors before matching
        line = re.sub(r"\x1b\[[0-9;]*m|\x1b\(B", "", line)
        match = RUSTFMT_HUNK_RE.match(line)
        if match:
            findings.append(
                {
                    "file": relative_path(match["path"], root),
                    "line": int(match["line"]),
                    "column": 0,
                    "level": "warning",
                    "severity": "warning",
                    "rule": "rustfmt",
                    "message": "code is not formatted; run `cargo +nightly fmt`",
                    "help": "",
                    "description": "",
                    "rendered": "",
                }
            )
        elif findings:
            # keep each hunk's diff with its finding for the text report
            findings[-1]["rendered"] += line + "\n"
    return findings, 0


def detect_context():
    """Describe the CI system this script runs in and the change under review.

    Returns a dict with:

    * `platform` - `github`, `gitlab`, or `local`
    * `change_base` - the commit to diff against to find changed files, or None
    * `change_label` - how to refer to the change, such as "this pull request"
    * `trusted_ref` - the commit to read the ignore file from, or None to read
      the working tree
    * `trusted_label` - how to refer to `trusted_ref`
    """
    env = os.environ
    context = {"platform": "local", "change_base": None, "change_label": "", "trusted_ref": None, "trusted_label": ""}
    if env.get("GITHUB_ACTIONS") == "true":
        context["platform"] = "github"
        # a pull_request checkout is the test merge commit, whose first parent is the base
        if env.get("GITHUB_EVENT_NAME") == "pull_request":
            context.update(change_base="HEAD^1", change_label="this pull request", trusted_ref="HEAD^1", trusted_label="the base branch")
    elif env.get("GITLAB_CI") == "true":
        context["platform"] = "gitlab"
        base = env.get("CI_MERGE_REQUEST_DIFF_BASE_SHA", "")
        # a child pipeline sees the push only through the before SHA its parent
        # trigger job forwards as PARENT_COMMIT_BEFORE_SHA
        before = next(
            (sha for sha in (env.get("CI_COMMIT_BEFORE_SHA", ""), env.get("PARENT_COMMIT_BEFORE_SHA", "")) if sha and not NULL_SHA_RE.match(sha)),
            "",
        )
        if base:
            context.update(change_base=base, change_label="this merge request", trusted_ref=base, trusted_label="the target branch")
        elif before:
            # a push pipeline can only compare with the commit the branch was at before the push
            context.update(change_base=before, change_label="this push")
    return context


def git(root, *args):
    """Run a git command in the repository and return its output.

    # Arguments

    * `root` - The repository root
    * `args` - The git arguments
    """
    return subprocess.run(["git", "-C", str(root), *args], check=True, capture_output=True, text=True).stdout


def changed_files(root, context):
    """List the files changed by the change under review.

    # Arguments

    * `root` - The repository root
    * `context` - The CI context from `detect_context`

    Returns a set of repo-relative paths, or None when there is no change to
    compare with or the base commit is unavailable.
    """
    if not context["change_base"]:
        return None
    try:
        return set(git(root, "diff", "--name-only", context["change_base"], "HEAD").splitlines())
    except (OSError, subprocess.CalledProcessError) as error:
        print(f"could not list files changed by {context['change_label']}: {error}", file=sys.stderr)
        return None


class IgnoreFileError(Exception):
    """The ignore file could not be parsed or has an invalid entry."""


def load_ignores(path, root, tool, context):
    """Load the ignore entries that apply to a tool.

    # Arguments

    * `path` - The repo-relative path of the ignore file
    * `root` - The repository root
    * `tool` - The tool name passed with `--tool`
    * `context` - The CI context from `detect_context`

    Returns `(entries, source)`, where `source` describes where the file was read from.
    """
    trusted = context["trusted_ref"]
    if trusted:
        # read the base commit's copy so a change cannot ignore its own findings
        try:
            files = git(root, "ls-tree", "--name-only", trusted, "--", path).splitlines()
            if path not in files:
                return [], f"not present on {context['trusted_label']}"
            text = git(root, "show", f"{trusted}:{path}")
        except (OSError, subprocess.CalledProcessError) as error:
            # failing open would let findings through unseen, so report everything instead
            ci_message(context, "warning", "lint ignores", f"could not read {path} from {context['trusted_label']}, so no findings are ignored")
            print(f"could not read the ignore file from {trusted}: {error}", file=sys.stderr)
            return [], f"unreadable on {context['trusted_label']}, so not applied"
        source = context["trusted_label"]
    else:
        file = root / path
        if not file.is_file():
            return [], "not present"
        text = file.read_text(encoding="utf-8")
        source = "this commit"
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise IgnoreFileError(f"{path} is not valid TOML: {error}") from error
    entries = []
    for number, entry in enumerate(document.get("ignore", []), start=1):
        # every entry must say what it hides and why
        for key in ("tool", "rule", "reason"):
            if not isinstance(entry.get(key), str) or not entry[key].strip():
                raise IgnoreFileError(f"{path}: ignore entry {number} needs a non-empty `{key}` string")
        paths = entry.get("paths", ["*"])
        if not isinstance(paths, list) or not paths or not all(isinstance(item, str) for item in paths):
            raise IgnoreFileError(f"{path}: ignore entry {number} has `paths` that is not a list of strings")
        expires = entry.get("expires")
        if expires is not None and not isinstance(expires, datetime.date):
            raise IgnoreFileError(f"{path}: ignore entry {number} has an `expires` that is not a date like 2027-01-31")
        unknown = set(entry) - {"tool", "rule", "paths", "reason", "expires"}
        if unknown:
            raise IgnoreFileError(f"{path}: ignore entry {number} has unknown keys: {', '.join(sorted(unknown))}")
        if entry["tool"] == tool:
            entries.append(
                {
                    "number": number,
                    "rule": entry["rule"],
                    "paths": paths,
                    "reason": entry["reason"],
                    "expires": expires,
                    "expired": expires is not None and expires < datetime.date.today(),
                    "matched": [],
                }
            )
    return entries, source


def apply_ignores(findings, entries):
    """Split findings into reported and ignored ones.

    # Arguments

    * `findings` - Every finding
    * `entries` - The ignore entries for this tool; matches are recorded on them

    Returns the findings that are still reported.
    """
    reported = []
    for finding in findings:
        # the first active entry whose rule and path patterns both match wins
        entry = next(
            (
                entry
                for entry in entries
                if not entry["expired"]
                and fnmatch.fnmatchcase(finding["rule"], entry["rule"])
                and any(fnmatch.fnmatchcase(finding["file"], pattern) for pattern in entry["paths"])
            ),
            None,
        )
        if entry:
            entry["matched"].append(finding)
        else:
            reported.append(finding)
    return reported


def write_filtered_sarif(source, destination, ignored):
    """Write a copy of a SARIF log without the ignored results.

    # Arguments

    * `source` - The original SARIF file
    * `destination` - Where to write the filtered SARIF
    * `ignored` - The ignored findings
    """
    log = json.loads(Path(source).read_text(encoding="utf-8"))
    # results are matched by content, since this re-reads the file parse_sarif read
    hidden = {json.dumps(finding["result"], sort_keys=True) for finding in ignored}
    for run in log.get("runs", []):
        run["results"] = [result for result in run.get("results", []) if json.dumps(result, sort_keys=True) not in hidden]
    Path(destination).parent.mkdir(parents=True, exist_ok=True)
    Path(destination).write_text(json.dumps(log, indent=2) + "\n", encoding="utf-8")


def location_of(finding):
    """Format a finding's `file:line:column` location.

    # Arguments

    * `finding` - The finding
    """
    parts = [finding["file"] or "(no file)"]
    if finding["line"]:
        parts.append(str(finding["line"]))
        if finding["column"]:
            parts.append(str(finding["column"]))
    return ":".join(parts)


def text_line(finding):
    """Format a finding as one readable line.

    # Arguments

    * `finding` - The finding
    """
    rule = f" [{finding['rule']}]" if finding["rule"] else ""
    return f"{location_of(finding)}: {finding['severity']}{rule} {finding['message']}"


def log_entry(finding, input_format):
    """Format a finding for the log and the text report.

    # Arguments

    * `finding` - The finding
    * `input_format` - The `--format` the finding was read from
    """
    rendered = finding["rendered"].rstrip("\n")
    # rustc's rendered diagnostic already names the location and shows the code
    if input_format == "cargo" and rendered:
        return rendered
    # rustfmt findings carry the diff hunk to apply
    if input_format == "rustfmt" and rendered:
        return text_line(finding) + "\n" + rendered
    return text_line(finding)


def ci_message(context, level, title, message):
    """Print an error or warning, as an annotation on GitHub and a plain line elsewhere.

    # Arguments

    * `context` - The CI context from `detect_context`
    * `level` - `error` or `warning`
    * `title` - A short title for the message
    * `message` - The message text
    """
    if context["platform"] == "github":
        print(f"::{level} title={escape_property(title)}::{escape_annotation(message)}", flush=True)
    else:
        print(f"{level.upper()}: {title}: {CONTROL_RE.sub('', message)}", flush=True)


def log_section(context, name, title, body, collapsed=True):
    """Print text in a collapsible log group (GitHub) or section (GitLab).

    # Arguments

    * `context` - The CI context from `detect_context`
    * `name` - A section id of letters, digits and underscores
    * `title` - The section heading
    * `body` - The text, which may contain tool-controlled content
    * `collapsed` - Whether GitLab shows the section collapsed
    """
    if context["platform"] == "github":
        # pause workflow commands so the body cannot issue any
        token = secrets.token_hex(16)
        print(f"::group::{title}")
        print(f"::stop-commands::{token}")
        print(body)
        print(f"::{token}::")
        print("::endgroup::")
    elif context["platform"] == "gitlab":
        # strip control characters so the body cannot open or close sections
        clean = "\n".join(CONTROL_RE.sub("", line) for line in body.split("\n"))
        flag = "[collapsed=true]" if collapsed else ""
        print(f"\x1b[0Ksection_start:{int(time.time())}:{name}{flag}\r\x1b[0K{title}")
        print(clean)
        print(f"\x1b[0Ksection_end:{int(time.time())}:{name}\r\x1b[0K")
    else:
        print(f"== {title}")
        print(body)
    sys.stdout.flush()


def write_codequality(path, tool, findings):
    """Write findings as a GitLab Code Quality (CodeClimate) report.

    # Arguments

    * `path` - Where to write the report
    * `tool` - The tool name, used in check names and fingerprints
    * `findings` - The reported findings
    """
    issues = []
    occurrences = Counter()
    for finding in findings:
        # fingerprints identify an issue across pipelines; repeats of the same
        # finding on one line are told apart by their order
        key = "\0".join((tool, finding["rule"], finding["file"], str(finding["line"]), finding["message"]))
        occurrences[key] += 1
        issues.append(
            {
                "type": "issue",
                "engine_name": tool,
                "check_name": f"{tool} {finding['rule']}".strip(),
                "description": finding["message"],
                "severity": CODEQUALITY_SEVERITIES.get(finding["severity"], CODEQUALITY_SEVERITIES[finding["level"]]),
                "fingerprint": hashlib.sha256(f"{key}\0{occurrences[key]}".encode()).hexdigest(),
                "location": {"path": finding["file"] or ".", "lines": {"begin": finding["line"] or 1}},
            }
        )
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    Path(path).write_text(json.dumps(issues, indent=2) + "\n", encoding="utf-8")


def annotate(findings, tool):
    """Emit workflow annotations, capped at GitHub's per-level limit.

    # Arguments

    * `findings` - The findings, already in priority order
    * `tool` - The tool name used in annotation titles

    Returns the number of findings left unannotated at each level.
    """
    emitted = Counter()
    omitted = Counter()
    for finding in findings:
        level = finding["level"]
        if emitted[level] >= MAX_ANNOTATIONS_PER_LEVEL:
            omitted[level] += 1
            continue
        emitted[level] += 1
        title = f"{tool} {finding['rule']}".strip()
        properties = [f"title={escape_property(title)}"]
        if finding["file"]:
            properties.insert(0, f"file={escape_property(finding['file'])}")
            if finding["line"]:
                properties.append(f"line={finding['line']}")
                if finding["column"]:
                    properties.append(f"col={finding['column']}")
        print(f"::{ANNOTATION_COMMANDS[level]} {','.join(properties)}::{escape_annotation(finding['message'])}")
    return omitted


def findings_table(findings, changed):
    """Render findings as markdown table lines.

    # Arguments

    * `findings` - The findings to list
    * `changed` - The files changed by the change under review, or None
    """
    header = "| Location | Severity | Rule | Message |"
    lines = [header, "|---|---|---|---|"]
    for finding in findings:
        location = location_of(finding)
        if changed is not None and finding["file"] in changed:
            location = "🔶 " + location
        rule = finding["rule"]
        link = finding["help"] or rule_link(rule)
        rule_cell = f"[{escape_cell(rule)}]({link})" if link and rule else escape_cell(rule)
        lines.append(f"| {escape_cell(location)} | {escape_cell(finding['severity'])} | {rule_cell} | {escape_cell(finding['message'])} |")
    return lines


def ignore_section(entries, ignore_file, source, changed, context):
    """Render the summary of ignore entries and what they hid.

    # Arguments

    * `entries` - The ignore entries for this tool
    * `ignore_file` - The repo-relative path of the ignore file
    * `source` - Where the ignore file was read from
    * `changed` - The files changed by the change under review, or None
    * `context` - The CI context from `detect_context`
    """
    lines = []
    if changed is not None and ignore_file in changed:
        change = context["change_label"].capitalize()
        if context["trusted_ref"]:
            effect = f"Ignores are read from {context['trusted_label']}, so the change takes effect after merge."
        else:
            effect = "The changed ignores apply to this run, so review them closely."
        lines += ["", f"> {change} changes `{ignore_file}`. {effect}"]
    if not entries:
        return lines
    hidden = sum(len(entry["matched"]) for entry in entries)
    lines += [
        "",
        f"<details><summary>{hidden} findings ignored by <code>{escape_cell(ignore_file)}</code> (read from {source})</summary>",
        "",
        "| Entry | Rule | Paths | Ignored | Reason |",
        "|---|---|---|---|---|",
    ]
    for entry in entries:
        if entry["expired"]:
            count = f"expired {entry['expires']}, not applied"
        elif not entry["matched"]:
            count = "0 (unused; consider removing)"
        else:
            count = str(len(entry["matched"])) + (f" (until {entry['expires']})" if entry["expires"] else "")
        paths = ", ".join(entry["paths"])
        lines.append(f"| {entry['number']} | {escape_cell(entry['rule'])} | {escape_cell(paths)} | {escape_cell(count)} | {escape_cell(entry['reason'])} |")
    return lines + ["", "</details>"]


def gate_description(fail_level, fail_severities):
    """Describe which findings fail the job.

    # Arguments

    * `fail_level` - The lowest level that fails the run
    * `fail_severities` - The severities that fail the run
    """
    parts = []
    if fail_level != "none":
        parts.append(f"`{fail_level}` and above")
    if fail_severities:
        parts.append("severity " + "/".join(f"`{severity}`" for severity in sorted(fail_severities)))
    return "fails on " + " or ".join(parts) if parts else "never fails on findings"


def build_markdown(tool, findings, suppressed, changed, gate, failed, omitted, ignores, change_label):
    """Render the markdown report used for the job summary and the artifact.

    # Arguments

    * `tool` - The tool name
    * `findings` - Every reported finding, in priority order
    * `suppressed` - The number of suppressed findings
    * `changed` - The files changed by the change under review, or None
    * `gate` - The description of which findings fail the job
    * `failed` - Whether the report gate failed
    * `omitted` - Findings left unannotated at each level
    * `ignores` - The rendered ignore section lines
    * `change_label` - How to refer to the change under review

    Returns `(summary, full)` markdown, where the summary caps the findings table.
    """
    levels = Counter(finding["level"] for finding in findings)
    counts = " · ".join(f"{levels[level]} {level}" for level in LEVELS)
    status = "❌ failed" if failed else "✅ passed"
    head = [f"## {tool}", "", f"**{len(findings)} findings**: {counts} ({gate}; report gate {status})"]
    if suppressed:
        head.append(f"\n{suppressed} findings were suppressed by inline ignore comments.")
    head += ignores
    if omitted:
        head.append(
            f"\nGitHub shows only {MAX_ANNOTATIONS_PER_LEVEL} annotations per level per step, so "
            f"{sum(omitted.values())} findings are listed here and in the `{tool}-report` artifact but not annotated inline."
        )
    if not findings:
        text = "\n".join(head + ["", "No findings."]) + "\n"
        return text, text
    # findings in files the change touches are the most relevant to a reviewer
    in_pr = [finding for finding in findings if changed is not None and finding["file"] in changed]
    body = []
    if changed is not None:
        body += ["", f"### Findings in files changed by {change_label} ({len(in_pr)})", ""]
        if in_pr:
            body += ["Pre-existing findings in these files are included; compare with a run on the base branch to see what is new.", ""]
            body += findings_table(in_pr[:MAX_SUMMARY_ROWS], None)
        else:
            body.append("None.")
    # group by rule so recurring problems stand out
    by_rule = Counter((finding["rule"], finding["severity"]) for finding in findings)
    examples = {}
    for finding in findings:
        examples.setdefault((finding["rule"], finding["severity"]), finding)
    body += ["", "### Findings by rule", "", "| Rule | Severity | Count | Description |", "|---|---|---|---|"]
    for (rule, severity), count in sorted(by_rule.items(), key=lambda item: (LEVELS.index(examples[item[0]]["level"]), -item[1])):
        example = examples[(rule, severity)]
        link = example["help"] or rule_link(rule)
        rule_cell = f"[{escape_cell(rule)}]({link})" if link and rule else escape_cell(rule or "(none)")
        description = example["description"] or example["message"]
        body.append(f"| {rule_cell} | {escape_cell(severity)} | {count} | {escape_cell(description)} |")
    legend = [f"🔶 marks files changed by {change_label}.", ""] if changed is not None else []
    summary_rows = findings[:MAX_SUMMARY_ROWS]
    truncated = f" (first {MAX_SUMMARY_ROWS}; see the `{tool}-report` artifact for all)" if len(findings) > MAX_SUMMARY_ROWS else ""

    def all_findings(rows, note):
        """Render the collapsible table of all findings."""
        return ["", f"<details><summary>All findings{note}</summary>", ""] + legend + findings_table(rows, changed) + ["", "</details>"]

    summary = "\n".join(head + body + all_findings(summary_rows, truncated)) + "\n"
    full = "\n".join(head + body + all_findings(findings, "")) + "\n"
    return summary, full


def main():
    """Parse the tool output, report it everywhere, and apply the gate."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tool", required=True, help="tool name used in titles, report file names and the ignore file")
    parser.add_argument("--format", required=True, choices=("sarif", "cargo", "rustfmt"))
    parser.add_argument("--input", required=True, help="the tool output to read")
    parser.add_argument("--output-dir", help="directory for the markdown and text reports")
    parser.add_argument("--fail-level", default="none", choices=LEVELS + ("none",))
    parser.add_argument(
        "--fail-severity", action="append", default=[], help="severities, such as CRITICAL or HIGH,CRITICAL, that fail the run"
    )
    parser.add_argument("--findings-exit-code", type=int, default=1, help="exit status when findings fail the run")
    parser.add_argument("--ignore-file", help="repo-relative path of the TOML ignore file")
    parser.add_argument("--sarif-output", help="where to write the SARIF input without ignored results")
    parser.add_argument("--codequality", help="where to write a GitLab Code Quality report")
    args = parser.parse_args()
    if args.sarif_output and args.format != "sarif":
        parser.error("--sarif-output needs --format sarif")
    context = detect_context()
    root = Path(os.environ.get("GITHUB_WORKSPACE") or os.environ.get("CI_PROJECT_DIR") or ".").resolve()
    parse = {"sarif": parse_sarif, "cargo": parse_cargo, "rustfmt": parse_rustfmt}[args.format]
    findings, suppressed = parse(args.input, root)
    changed = changed_files(root, context)
    # drop ignored findings before anything is reported or gated
    entries, source = [], ""
    if args.ignore_file:
        try:
            entries, source = load_ignores(args.ignore_file, root, args.tool, context)
        except IgnoreFileError as error:
            ci_message(context, "error", "lint ignores", str(error))
            return 2
        for entry in entries:
            if entry["expired"]:
                ci_message(context, "warning", "lint ignores", f"{args.ignore_file} entry {entry['number']} ({entry['rule']}) expired on {entry['expires']} and no longer applies")
    reported = apply_ignores(findings, entries)
    ignored = [finding for entry in entries for finding in entry["matched"]]
    if args.sarif_output:
        write_filtered_sarif(args.input, args.sarif_output, ignored)
    # order by relevance to the change under review, then severity, then location
    reported.sort(
        key=lambda finding: (
            not (changed is not None and finding["file"] in changed),
            LEVELS.index(finding["level"]),
            finding["file"],
            finding["line"],
        )
    )
    fail_severities = {part.strip().upper() for value in args.fail_severity for part in value.split(",") if part.strip()}
    failed = any(
        (args.fail_level != "none" and LEVELS.index(finding["level"]) <= LEVELS.index(args.fail_level))
        or finding["severity"] in fail_severities
        for finding in reported
    )
    # multi-line cargo and rustfmt entries are separated by a blank line
    separator = "\n" if args.format == "sarif" else "\n\n"
    text = separator.join(log_entry(finding, args.format) for finding in reported)
    section = re.sub(r"[^a-z0-9_]", "_", args.tool.lower())
    log_section(context, f"{section}_findings", f"{args.tool} findings ({len(reported)})", text or "No findings.")
    # GitHub shows annotations inline; GitLab shows the Code Quality report instead
    omitted = annotate(reported, args.tool) if context["platform"] == "github" else Counter()
    if args.codequality:
        write_codequality(args.codequality, args.tool, reported)
    gate = gate_description(args.fail_level, fail_severities)
    ignores = ignore_section(entries, args.ignore_file, source, changed, context) if args.ignore_file else []
    summary, full = build_markdown(args.tool, reported, suppressed, changed, gate, failed, omitted, ignores, context["change_label"])
    summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_path:
        with open(summary_path, "a", encoding="utf-8") as handle:
            handle.write(summary)
    elif context["platform"] == "gitlab":
        # GitLab has no job summary page, so the summary goes in an open log section
        log_section(context, f"{section}_summary", f"{args.tool} summary", summary, collapsed=False)
    if args.output_dir:
        output = Path(args.output_dir)
        output.mkdir(parents=True, exist_ok=True)
        (output / f"{args.tool}-report.md").write_text(full, encoding="utf-8")
        (output / f"{args.tool}-report.txt").write_text((text or "No findings.") + "\n", encoding="utf-8")
        if ignored:
            lines = [f"{text_line(finding)}  (ignore entry {entry['number']})" for entry in entries for finding in entry["matched"]]
            (output / f"{args.tool}-ignored.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    levels = Counter(finding["level"] for finding in reported)
    print(f"{args.tool}: {len(reported)} findings (" + ", ".join(f"{levels[level]} {level}" for level in LEVELS) + f"), {len(ignored)} ignored")
    if failed:
        print(f"{args.tool}: this check {gate}, and at least one finding meets that")
    return args.findings_exit_code if failed else 0


if __name__ == "__main__":
    sys.exit(main())
