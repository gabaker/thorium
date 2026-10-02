#!/usr/bin/env python3
"""Report GitHub Actions `uses:` references that are unpinned or behind the latest release.

Every `uses: owner/repo[/path]@ref` in the repo's workflows and local composite
actions is checked:

* refs that are not a full 40-character commit SHA are reported as unpinned
* SHA pins must carry a `# vX.Y.Z` comment naming the release they correspond to
* that version is compared with the repo's latest GitHub release (or highest
  semver tag when the repo publishes no releases), and older pins are reported

This script only reads workflow files and calls the GitHub REST API on
api.github.com; it uses only the Python standard library so it adds no
dependencies of its own. zizmor's `ref-version-mismatch` and `impostor-commit`
audits separately verify that each SHA really matches its comment and repo.

Exit status is 1 when any reference is unpinned, and 0 otherwise. Outdated pins
are reported as warnings and do not fail the run, since Dependabot opens the
corresponding update pull requests.
"""

import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

# the GitHub REST API root; the only host this script contacts
API_ROOT = "https://api.github.com"
# matches a `uses:` line and captures the action, its ref and an optional trailing comment
USES_RE = re.compile(
    r"""^\s*-?\s*uses:\s*["']?(?P<action>[^@\s"']+)@(?P<ref>[^\s"'#]+)["']?\s*(?:\#\s*(?P<comment>.*))?$"""
)
# a valid owner/repo pair, used to keep API paths well-formed
REPO_RE = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
# a full commit SHA
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
# a semantic version, with an optional leading `v` and an optional patch number
SEMVER_RE = re.compile(r"^v?(\d+)\.(\d+)(?:\.(\d+))?$")


def parse_version(text):
    """Parse a version string into a comparable tuple.

    # Arguments

    * `text` - The version string, such as `v4.38.2`

    Returns a `(major, minor, patch)` tuple, or None when `text` is not a version.
    """
    # pull out the numeric components, treating a missing patch as zero
    match = SEMVER_RE.match(text.strip()) if text else None
    if not match:
        return None
    return tuple(int(part or 0) for part in match.groups())


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


def api_get(path, token):
    """Fetch and decode a JSON document from the GitHub REST API.

    # Arguments

    * `path` - The API path, starting with `/`
    * `token` - An optional GitHub token used to raise the rate limit
    """
    # build the request with the recommended headers and optional auth
    request = urllib.request.Request(API_ROOT + path)
    request.add_header("Accept", "application/vnd.github+json")
    request.add_header("X-GitHub-Api-Version", "2022-11-28")
    if token:
        request.add_header("Authorization", f"Bearer {token}")
    # return None for missing resources so callers can fall back
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def latest_version(repo, token):
    """Find the newest released version of an action repository.

    # Arguments

    * `repo` - The `owner/repo` of the action
    * `token` - An optional GitHub token used to raise the rate limit

    Returns a `(tag, version_tuple)` pair, or None when no version could be found.
    """
    # prefer the latest non-prerelease GitHub release
    release = api_get(f"/repos/{repo}/releases/latest", token)
    if release and parse_version(release.get("tag_name", "")):
        return release["tag_name"], parse_version(release["tag_name"])
    # otherwise fall back to the highest semver tag
    tags = api_get(f"/repos/{repo}/tags?per_page=100", token) or []
    versions = [(tag["name"], parse_version(tag["name"])) for tag in tags]
    versions = [entry for entry in versions if entry[1]]
    return max(versions, key=lambda entry: entry[1]) if versions else None


def collect_uses(root):
    """Collect every remote `uses:` reference in workflows and local actions.

    # Arguments

    * `root` - The repository root
    """
    # gather workflow files and any local composite action definitions
    github_dir = root / ".github"
    files = sorted(github_dir.glob("workflows/*.y*ml")) + sorted(github_dir.glob("actions/**/action.y*ml"))
    found = []
    for path in files:
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
            match = USES_RE.match(line)
            # skip non-matching lines, local actions and docker image references
            if not match or match["action"].startswith(("./", "docker://")):
                continue
            # reduce `owner/repo/sub/path` to the repository that hosts it
            repo = "/".join(match["action"].split("/")[:2])
            comment = (match["comment"] or "").split()
            found.append(
                {
                    "file": str(path.relative_to(root)),
                    "line": number,
                    "action": match["action"],
                    "repo": repo,
                    "ref": match["ref"],
                    "pinned_tag": comment[0] if comment else "",
                }
            )
    return found


def main():
    """Check every action reference and write a report to stdout and the step summary."""
    # read the optional token and locate the repo root
    token = os.environ.get("GITHUB_TOKEN", "")
    root = Path(os.environ.get("GITHUB_WORKSPACE", ".")).resolve()
    uses = collect_uses(root)
    latest_cache = {}
    rows = []
    unpinned = 0
    # evaluate each reference against the pinning rules and the latest release
    for use in uses:
        location = f"{use['file']}:{use['line']}"
        if not REPO_RE.match(use["repo"]):
            status, latest = "unrecognized action reference", ""
        elif not SHA_RE.match(use["ref"]):
            unpinned += 1
            status, latest = "UNPINNED: not a full commit SHA", ""
            print(f"::error file={escape_property(use['file'])},line={use['line']}::{escape_annotation(use['action'] + '@' + use['ref'] + ' is not pinned to a commit SHA')}")
        else:
            # look up each repository's latest release only once
            if use["repo"] not in latest_cache:
                latest_cache[use["repo"]] = latest_version(use["repo"], token)
            newest = latest_cache[use["repo"]]
            pinned = parse_version(use["pinned_tag"])
            latest = newest[0] if newest else "unknown"
            if pinned is None:
                status = "no `# vX.Y.Z` version comment"
            elif newest is None:
                status = "could not determine latest release"
            elif pinned < newest[1]:
                status = "outdated" if pinned[0] == newest[1][0] else "outdated (new major)"
                print(f"::warning file={escape_property(use['file'])},line={use['line']}::{escape_annotation(use['repo'] + ' is pinned at ' + use['pinned_tag'] + '; latest release is ' + latest)}")
            else:
                status = "up to date"
        rows.append((location, use["action"], use["pinned_tag"] or use["ref"], latest, status))
    # render a markdown table for the log and the job summary
    lines = ["| Location | Action | Pinned | Latest | Status |", "|---|---|---|---|---|"]
    for row in rows:
        cells = [cell.replace("|", r"\|") for cell in row]
        lines.append("| " + " | ".join(cells) + " |")
    report = "## Action pin report\n\n" + "\n".join(lines) + "\n"
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as handle:
            handle.write(report)
    # fail only on unpinned references
    return 1 if unpinned else 0


if __name__ == "__main__":
    sys.exit(main())
