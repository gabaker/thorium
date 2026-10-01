#!/usr/bin/env python3
"""Bump Thorium's version everywhere it is written, or check that every copy agrees.

The workspace version in the root Cargo.toml (`[workspace.package] version`) is the source of
truth. The version is written, and must match it, in:

* Cargo: the workspace version itself, any workspace crate that writes its version out instead of
  inheriting it (the Python crate, python/thorpy, does so maturin can read it), the version
  requirement of every `path` dependency on a workspace crate, and Cargo.lock's entry for each
  workspace crate. Crates in `INDEPENDENT_CRATES` keep their own version
* the web UI: ui/package.json and the two copies of the version in ui/package-lock.json
* the Helm charts: `version` and `appVersion` of every Thorium chart under deploy/charts, and each
  parent chart's dependency pin on a Thorium subchart. A chart is a Thorium chart when its `home`
  is `THORIUM_HOME`; the vendored upstream charts keep their own versions
* the chart version minithor and megathor install by default, and the version used in
  documentation examples (`PATTERN_PINS`)

Subcommands:

* `bump [VERSION]` writes VERSION into every location above, whatever its current value, so
  copies that have drifted apart are brought back in line by a single run. VERSION defaults to
  the current workspace version, which only syncs the stragglers
* `check` reports every copy that differs from the workspace version, plus crates whose version
  is neither inherited nor written out, Python packages whose version doesn't come from
  Cargo.toml, and independent crates whose dependents require a different version. It prints
  GitHub annotations when run in GitHub Actions, and exits 1 when anything is wrong

Files are edited in place, a version at a time, so their formatting is kept. Only the Python
standard library (3.11+ for tomllib) is used.
"""

import argparse
import os
import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

# the repository root, two levels above .github/scripts
ROOT = Path(__file__).resolve().parents[2]
# the directory holding the Helm charts
CHARTS_DIR = ROOT / "deploy" / "charts"
# the `home` that marks a chart as versioned with Thorium
THORIUM_HOME = "https://github.com/cisagov/thorium"
# workspace crates released on their own version, with the reason
INDEPENDENT_CRATES = {
    "cart-rs": "the cart library is versioned separately from Thorium",
}
# a version as written in prose and file names: X.Y.Z with an optional prerelease
VERSION_PATTERN = r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?"
# other files holding the version, as (path, pattern whose group 1 is the version, what it is).
# Every pattern must match at least once, so a moved or reworded example is reported
PATTERN_PINS = [
    ("minithor/minithor", rf'^CHART_VERSION="\$\{{MINITHOR_CHART_VERSION:-({VERSION_PATTERN})\}}"', "minithor default chart version"),
    ("minithor/minithor", rf"thorium-({VERSION_PATTERN})\.tgz", "minithor help example"),
    ("minithor/README.md", rf"thorium-({VERSION_PATTERN})\.tgz", "minithor README example"),
    ("minithor/README.md", rf"--chart-version ({VERSION_PATTERN})-<branch>", "minithor README example"),
    ("megathor/inventory/group_vars/all.yml", rf'^thorium_chart_version:\s*"({VERSION_PATTERN})"', "megathor default chart version"),
    ("megathor/roles/common/defaults/main.yml", rf'^thorium_chart_version:\s*"({VERSION_PATTERN})"', "megathor default chart version"),
    ("api/docs/src/admins/deploy/deploy-helm.md", rf"^VERSION=({VERSION_PATTERN})$", "Helm deploy docs example"),
]
# a TOML section header
TOML_SECTION_RE = re.compile(r"^\s*\[([^\]]+)\]")
# a TOML `version = "..."` line
TOML_VERSION_RE = re.compile(r'^\s*version\s*=\s*"([^"]+)"')
# an inline-table dependency line, with the table's contents in group 2
TOML_DEPENDENCY_RE = re.compile(r"^\s*([A-Za-z0-9_-]+)\s*=\s*\{(.*)\}")
# the version and path inside an inline dependency table
INLINE_VERSION_RE = re.compile(r'version\s*=\s*"([^"]+)"')
INLINE_PATH_RE = re.compile(r'path\s*=\s*"([^"]+)"')
# the top-level `version` of package.json and package-lock.json (indented by two spaces)
JSON_ROOT_VERSION_RE = re.compile(r'^ {2}"version":\s*"([^"]+)"')
# the root package entry in package-lock.json's `packages`, and its version
LOCK_ROOT_PACKAGE_RE = re.compile(r'^ {4}"":\s*\{')
LOCK_PACKAGE_VERSION_RE = re.compile(r'^ {6}"version":\s*"([^"]+)"')
# a top-level `key: value` line of a Chart.yaml, with the value in group 3
CHART_KEY_RE = re.compile(r"^(version|appVersion|home|name):\s*([\"']?)([^\"'\s#]+)\2")
# the start of a YAML list item
LIST_ITEM_RE = re.compile(r"^(\s*)-\s")
# a `name` or `version` key inside a dependency item, optionally after the item's dash
DEPENDENCY_KEY_RE = re.compile(r"^\s*(?:-\s+)?(name|version):\s*([\"']?)([^\"'\s#]+)\2")


@dataclass
class Pin:
    """One place the version is written, and where in its file."""

    # the file, relative to the repository root
    path: str
    # the 1-based line number
    line: int
    # the version's start column in the line
    start: int
    # the version's end column in the line
    end: int
    # the version written there
    value: str
    # what the version belongs to, for messages
    what: str


@dataclass
class Problem:
    """Something about the versions that is wrong."""

    # the file, relative to the repository root
    path: str
    # the 1-based line number, or 0 for the whole file
    line: int
    # what is wrong
    message: str


def relative(path):
    """Return a path relative to the repository root, as a string.

    # Arguments

    * `path` - An absolute path inside the repository
    """
    return str(path.relative_to(ROOT))


def read_lines(path):
    """Read a file's lines without their line endings.

    # Arguments

    * `path` - The file to read
    """
    return path.read_text(encoding="utf-8").splitlines()


def read_toml(path):
    """Parse a TOML file.

    # Arguments

    * `path` - The TOML file
    """
    # tomllib needs the file opened in binary mode
    with open(path, "rb") as file:
        return tomllib.load(file)


def pin_from_match(path, number, match, what, group=1):
    """Build a pin from a regex match on one line.

    # Arguments

    * `path` - The file the line is in
    * `number` - The 1-based line number
    * `match` - The match, with the version in `group`
    * `what` - What the version belongs to
    * `group` - The match group holding the version
    """
    return Pin(relative(path), number, match.start(group), match.end(group), match.group(group), what)


def toml_section_version(path, section, what):
    """Return the pin of the `version` key in one section of a TOML file, or None.

    # Arguments

    * `path` - The TOML file
    * `section` - The section name, such as `package`
    * `what` - What the version belongs to
    """
    # track the current section and stop at the first version line inside the requested one
    current = None
    for number, line in enumerate(read_lines(path), start=1):
        header = TOML_SECTION_RE.match(line)
        if header:
            current = header.group(1).strip()
            continue
        match = TOML_VERSION_RE.match(line)
        if current == section and match:
            return pin_from_match(path, number, match, what)
    return None


def workspace_members():
    """Return each workspace crate as (directory, Cargo.toml path, parsed package table)."""
    # the members are listed as directories relative to the root
    members = []
    for member in read_toml(ROOT / "Cargo.toml")["workspace"].get("members", []):
        manifest = ROOT / member / "Cargo.toml"
        members.append((ROOT / member, manifest, read_toml(manifest).get("package", {})))
    return members


def is_independent(directory, package):
    """Return whether a workspace crate keeps its own version.

    # Arguments

    * `directory` - The crate's directory
    * `package` - The crate's parsed `[package]` table
    """
    return relative(directory) in INDEPENDENT_CRATES or package.get("name") in INDEPENDENT_CRATES


def cargo_pins_and_problems():
    """Return the Cargo version pins, and problems that bumping cannot fix."""
    # the workspace version is the source of truth and is itself a pin
    root = ROOT / "Cargo.toml"
    pins = [toml_section_version(root, "workspace.package", "workspace version")]
    problems = []
    members = workspace_members()
    names = {}
    independent_versions = {}
    for directory, manifest, package in members:
        name = package.get("name", relative(directory))
        version = package.get("version")
        names[directory.resolve()] = name
        # independent crates keep their version, which their dependents must still require
        if is_independent(directory, package):
            independent_versions[directory.resolve()] = (name, version)
            continue
        # an inherited version needs nothing; a written-out one is a pin
        if isinstance(version, dict) and version.get("workspace") is True:
            pass
        elif isinstance(version, str):
            pins.append(toml_section_version(manifest, "package", f"crate {name} version"))
        else:
            problems.append(Problem(relative(manifest), 0, f"crate {name} must set version.workspace = true or a version string"))
        # a Python package must take its version from Cargo.toml, or repeat it
        pyproject = directory / "pyproject.toml"
        if pyproject.exists():
            project = read_toml(pyproject).get("project", {})
            if "version" not in project and "version" not in project.get("dynamic", []):
                problems.append(Problem(relative(pyproject), 0, f"{name}'s Python package sets no version; list it in project.dynamic"))
            elif "version" in project:
                pins.append(toml_section_version(pyproject, "project", f"{name} Python package version"))
    # version requirements on workspace crates in path dependencies
    for manifest in [root] + [manifest for _, manifest, _ in members]:
        for number, line in enumerate(read_lines(manifest), start=1):
            dependency = TOML_DEPENDENCY_RE.match(line)
            if not dependency:
                continue
            version = INLINE_VERSION_RE.search(line, dependency.start(2))
            path = INLINE_PATH_RE.search(line, dependency.start(2))
            if not version or not path:
                continue
            target = (manifest.parent / path.group(1)).resolve()
            if target in independent_versions:
                crate, expected = independent_versions[target]
                if version.group(1) != expected:
                    problems.append(Problem(relative(manifest), number, f"dependency on {crate} requires {version.group(1)}, but {crate} is at {expected}"))
            elif target in names:
                pins.append(pin_from_match(manifest, number, version, f"dependency on {names[target]}"))
    # Cargo.lock's entry for each workspace crate that follows the workspace version
    followers = {package.get("name") for directory, _, package in members if not is_independent(directory, package)}
    lock = ROOT / "Cargo.lock"
    lines = read_lines(lock)
    for index, line in enumerate(lines):
        name = re.match(r'^name = "([^"]+)"$', line)
        if not name or name.group(1) not in followers:
            continue
        # the entry runs to the next blank line; a `source` means a registry package of that name
        entry = []
        for offset in range(index + 1, len(lines)):
            if not lines[offset].strip():
                break
            entry.append((offset + 1, lines[offset]))
        if any(text.startswith("source = ") for _, text in entry):
            continue
        for number, text in entry:
            match = re.match(r'^version = "([^"]+)"$', text)
            if match:
                pins.append(pin_from_match(lock, number, match, f"Cargo.lock entry for {name.group(1)}"))
                break
    return [pin for pin in pins if pin], problems


def ui_pins():
    """Return the web UI's version pins in package.json and package-lock.json."""
    # package.json holds the version once, at the top level
    pins = []
    package = ROOT / "ui" / "package.json"
    lock = ROOT / "ui" / "package-lock.json"
    for path in (package, lock):
        for number, line in enumerate(read_lines(path), start=1):
            match = JSON_ROOT_VERSION_RE.match(line)
            if match:
                pins.append(pin_from_match(path, number, match, f"{relative(path)} version"))
                break
        else:
            raise SystemExit(f"{relative(path)} has no top-level version")
    # package-lock.json repeats it in the root entry of `packages`
    in_root = False
    for number, line in enumerate(read_lines(lock), start=1):
        if LOCK_ROOT_PACKAGE_RE.match(line):
            in_root = True
            continue
        match = LOCK_PACKAGE_VERSION_RE.match(line)
        if in_root and match:
            pins.append(pin_from_match(lock, number, match, "ui/package-lock.json root package version"))
            break
        if in_root and line.startswith("    }"):
            raise SystemExit("ui/package-lock.json's root package has no version")
    return pins


def chart_keys(path):
    """Return a Chart.yaml's top-level name, version, appVersion and home as pins.

    # Arguments

    * `path` - The Chart.yaml
    """
    # only unindented keys are top-level, so dependency versions are never picked up here
    keys = {}
    for number, line in enumerate(read_lines(path), start=1):
        match = CHART_KEY_RE.match(line)
        if match and match.group(1) not in keys:
            keys[match.group(1)] = pin_from_match(path, number, match, "", group=3)
    return keys


def dependency_pins(path, subcharts):
    """Return the version pins of a Chart.yaml's dependencies on the given subcharts.

    # Arguments

    * `path` - The parent Chart.yaml
    * `subcharts` - The names of the Thorium subcharts vendored under the parent's charts/
    """
    # collect each item of the top-level dependencies list, whatever order its keys are in
    items = []
    in_dependencies = False
    for number, line in enumerate(read_lines(path), start=1):
        if line and not line[0].isspace() and not line.startswith(("#", "-")):
            in_dependencies = line.startswith("dependencies:")
            continue
        if not in_dependencies:
            continue
        if LIST_ITEM_RE.match(line):
            items.append({})
        match = DEPENDENCY_KEY_RE.match(line)
        if match and items:
            items[-1][match.group(1)] = (number, match)
    # keep the version pin of every item naming a Thorium subchart
    pins = []
    for item in items:
        if "name" in item and "version" in item and item["name"][1].group(3) in subcharts:
            number, match = item["version"]
            pins.append(pin_from_match(path, number, match, f"dependency on {item['name'][1].group(3)}", group=3))
    return pins


def chart_pins():
    """Return every version pin in the Thorium charts.

    A Thorium chart is a Chart.yaml under deploy/charts whose `home` is `THORIUM_HOME`.
    """
    # find the Thorium charts, leaving vendored upstream charts out
    charts = []
    for path in sorted(CHARTS_DIR.rglob("Chart.yaml")):
        keys = chart_keys(path)
        if "home" in keys and keys["home"].value.rstrip("/") == THORIUM_HOME:
            charts.append((path, keys))
    names = {path.parent: keys["name"].value for path, keys in charts if "name" in keys}
    pins = []
    for path, keys in charts:
        name = names.get(path.parent, relative(path.parent))
        # a Thorium chart missing either version key cannot be kept in line
        for key in ("version", "appVersion"):
            if key not in keys:
                raise SystemExit(f"{relative(path)} has no top-level {key}")
            keys[key].what = f"chart {name} {key}"
            pins.append(keys[key])
        # pin the dependencies on Thorium subcharts vendored under this chart
        subcharts = {names[child] for child in names if child.parent == path.parent / "charts"}
        pins.extend(dependency_pins(path, subcharts))
    return pins


def pattern_pins():
    """Return the versions `PATTERN_PINS` finds in deployment defaults and documentation."""
    # each pattern must still match, so a moved or reworded example is reported instead of skipped
    pins = []
    for path, pattern, what in PATTERN_PINS:
        regex = re.compile(pattern)
        found = []
        for number, line in enumerate(read_lines(ROOT / path), start=1):
            found.extend(pin_from_match(ROOT / path, number, match, what) for match in regex.finditer(line))
        if not found:
            raise SystemExit(f"{path} no longer matches {pattern!r}; update PATTERN_PINS in {relative(Path(__file__))}")
        pins.extend(found)
    return pins


def write_pins(pins, version):
    """Replace each pin's version in its file, leaving files already at the version untouched.

    # Arguments

    * `pins` - The pins to rewrite
    * `version` - The version to write
    """
    # group the pins that need changing by file so each file is read and written once
    by_path = {}
    for pin in pins:
        if pin.value != version:
            by_path.setdefault(pin.path, []).append(pin)
    changed = []
    for path, file_pins in by_path.items():
        full = ROOT / path
        lines = full.read_text(encoding="utf-8").split("\n")
        # rewrite from the right so earlier spans on the same line stay valid
        for pin in sorted(file_pins, key=lambda pin: (pin.line, pin.start), reverse=True):
            line = lines[pin.line - 1]
            lines[pin.line - 1] = line[: pin.start] + version + line[pin.end :]
        full.write_text("\n".join(lines), encoding="utf-8")
        changed.extend(sorted(file_pins, key=lambda pin: (pin.line, pin.start)))
    return changed


def report(problems):
    """Print problems, as annotations when running in GitHub Actions.

    # Arguments

    * `problems` - The problems to print
    """
    # GitHub turns ::error lines into annotations on the file and line
    annotate = os.environ.get("GITHUB_ACTIONS") == "true"
    for problem in problems:
        location = f"{problem.path}:{problem.line}" if problem.line else problem.path
        print(f"{location}: {problem.message}")
        if annotate:
            line = f",line={problem.line}" if problem.line else ""
            print(f"::error file={problem.path}{line}::{problem.message}")


def main():
    """Parse arguments and run the requested subcommand."""
    # bump takes an optional version; check takes none
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    bump = commands.add_parser("bump", help="write one version everywhere the version is written")
    bump.add_argument("version", nargs="?", help="the version to write (default: the current workspace version)")
    commands.add_parser("check", help="report every copy of the version that differs from the workspace version")
    args = parser.parse_args()
    workspace_version = read_toml(ROOT / "Cargo.toml")["workspace"]["package"]["version"]
    cargo_pins, problems = cargo_pins_and_problems()
    pins = cargo_pins + ui_pins() + chart_pins() + pattern_pins()
    if args.command == "bump":
        # write the version, then report what changed and anything bumping cannot fix
        version = args.version or workspace_version
        if not re.fullmatch(VERSION_PATTERN, version):
            raise SystemExit(f"{version!r} is not an X.Y.Z or X.Y.Z-<prerelease> version")
        for pin in write_pins(pins, version):
            print(f"{pin.path}:{pin.line}: {pin.what} {pin.value} -> {version}")
        print(f"{len(pins)} version locations are at {version}")
        report(problems)
        return 1 if problems else 0
    # check every pin against the workspace version
    problems += [Problem(pin.path, pin.line, f"{pin.what} is {pin.value}, expected {workspace_version}") for pin in pins if pin.value != workspace_version]
    report(problems)
    if problems:
        print(f"{len(problems)} version problems; `python3 .github/scripts/versions.py bump` syncs every copy to Cargo.toml's {workspace_version}")
        return 1
    print(f"all {len(pins)} version locations are at {workspace_version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
