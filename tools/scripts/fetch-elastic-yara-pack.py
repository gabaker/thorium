"""Fetch and assemble the Elastic protections-artifacts YARA pack.

Downloads the elastic/protections-artifacts repo tarball, extracts only the
YARA rule files, and concatenates them into a single pack file suitable for
Thorium's `ai.yara_rules` setting (one namespace per pack file). Rules are
operator-fetched at deploy time and never vendored into the repo.

The downloaded tarball is untrusted data: members are validated against path
traversal, only `yara/rules/*.yar` leaves are read, and nothing from the
archive is ever executed.

Usage:
    python3 fetch-elastic-yara-pack.py --out <pack.yar> [--workdir <dir>]
        [--exclude <file.yar>]...

Validate the assembled pack with the engine-parity test before deploying it:
    THORPAL_YARA_PACK=<pack.yar> cargo test -p thorpal --lib detect::yara \
        -- --ignored --nocapture elastic_pack_compiles
"""

import argparse
import datetime
import json
import logging
import pathlib
import re
import sys
import tarfile
import tempfile
import urllib.request

# the repo the pack is assembled from
REPO = "elastic/protections-artifacts"
# the branch-tip tarball, used only when the commit SHA cannot be resolved
# (urllib honors https_proxy from the env)
BRANCH_TARBALL_URL = f"https://codeload.github.com/{REPO}/tar.gz/refs/heads/main"
# the API route that resolves the current commit of the default branch
COMMIT_URL = f"https://api.github.com/repos/{REPO}/commits/main"
# untrusted-input ceilings: the whole tarball, one member, and the extraction
MAX_TARBALL_BYTES = 200 * 1024 * 1024
MAX_MEMBER_BYTES = 10 * 1024 * 1024
MAX_EXTRACTED_BYTES = 300 * 1024 * 1024
# tarball members we keep: <root>/yara/rules/<leaf>.yar with no traversal
MEMBER_PATTERN = re.compile(r"^[^/]+/yara/rules/[^/]+\.yar$")
# a YARA rule declaration, for the summary rule count
RULE_PATTERN = re.compile(r"^\s*(?:private\s+|global\s+)*rule\s", re.MULTILINE)
# GitHub rejects requests without a user agent
USER_AGENT = {"User-Agent": "thorium-fetch-elastic-yara-pack"}


def resolve_commit():
    """Resolve the current commit SHA of the repo's default branch.

    Returns the full SHA string, or "unknown" when the API is unreachable so
    an API hiccup never blocks assembling the pack itself.
    """
    # ask the GitHub API for the tip commit of main
    request = urllib.request.Request(COMMIT_URL, headers=USER_AGENT)
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response).get("sha", "unknown")
    except Exception as error:
        logging.warning(f"could not resolve commit sha: {error}")
        return "unknown"


def download_tarball(workdir, commit):
    """Download the repo tarball into the working directory.

    The tarball is pinned to the resolved commit whenever one is known, so the
    provenance header can never name a different commit than the content; the
    moving branch tip is only used when the API lookup failed.

    Args:
        workdir: The fresh working directory to download into
        commit: The resolved commit SHA, or "unknown"

    Returns the path to the downloaded tarball.
    """
    # pin the content to the recorded commit when the API resolved one
    url = (
        f"https://codeload.github.com/{REPO}/tar.gz/{commit}"
        if commit != "unknown"
        else BRANCH_TARBALL_URL
    )
    # stream the tarball to disk inside the untrusted working directory,
    # refusing anything over the untrusted-input ceiling
    tarball = workdir / "protections-artifacts.tar.gz"
    logging.info(f"downloading {url}")
    request = urllib.request.Request(url, headers=USER_AGENT)
    with urllib.request.urlopen(request, timeout=300) as response:
        with tarball.open("wb") as sink:
            total = 0
            while chunk := response.read(1024 * 1024):
                total += len(chunk)
                if total > MAX_TARBALL_BYTES:
                    sys.exit(f"tarball exceeds {MAX_TARBALL_BYTES} bytes; refusing")
                sink.write(chunk)
    logging.info(f"downloaded {tarball.stat().st_size} bytes")
    return tarball


def extract_rules(tarball, workdir):
    """Extract only the YARA rule files from the tarball.

    Members are read through `extractfile` and rewritten by basename into a
    rules directory, so archive paths, permissions, and link members never
    touch the filesystem (no path traversal is possible).

    Args:
        tarball: The downloaded tarball path
        workdir: The working directory to extract into

    Returns the directory holding the extracted `.yar` files.
    """
    # rewrite each yara/rules leaf into a flat rules directory
    rules_dir = workdir / "rules"
    rules_dir.mkdir()
    extracted = 0
    total = 0
    with tarfile.open(tarball, "r:gz") as archive:
        for member in archive:
            # keep only plain-file yara rule leaves with sane paths
            if not member.isfile() or not MEMBER_PATTERN.match(member.name):
                continue
            # an oversized rule file is untrusted-input abuse, not a rule
            if member.size > MAX_MEMBER_BYTES:
                logging.warning(f"skipping oversized member {member.name} ({member.size} bytes)")
                continue
            # read the member's bytes (bounded) and rewrite under its basename
            handle = archive.extractfile(member)
            if handle is None:
                continue
            data = handle.read(MAX_MEMBER_BYTES)
            total += len(data)
            if total > MAX_EXTRACTED_BYTES:
                sys.exit(f"extraction exceeds {MAX_EXTRACTED_BYTES} bytes; refusing")
            leaf = pathlib.PurePosixPath(member.name).name
            (rules_dir / leaf).write_bytes(data)
            extracted += 1
    logging.info(f"extracted {extracted} rule files")
    return rules_dir


def assemble(rules_dir, out, commit, excludes):
    """Concatenate the extracted rule files into one pack file.

    Args:
        rules_dir: The directory of extracted `.yar` files
        out: The pack file path to write
        commit: The resolved source commit SHA
        excludes: Rule file basenames to leave out of the pack
    """
    # gather the rule files, dropping the operator's excludes; an exclude that
    # matches nothing is a typo that would silently ship the unwanted rules
    files = sorted(rules_dir.glob("*.yar"))
    unmatched = excludes - {path.name for path in files}
    if unmatched:
        sys.exit(f"--exclude matched no extracted file: {', '.join(sorted(unmatched))}")
    kept = [path for path in files if path.name not in excludes]
    skipped = [path.name for path in files if path.name in excludes]
    # build the provenance header the pack carries at its head
    today = datetime.date.today().isoformat()
    header = [
        "// elastic-protections-artifacts YARA pack (assembled, do not edit)",
        f"// source: https://github.com/{REPO} @ {commit}",
        f"// fetched: {today} by tools/scripts/fetch-elastic-yara-pack.py",
        "// license: Elastic License 2.0 (see LICENSE.txt in the source repo);",
        "//          operator-fetched at deploy time, never vendored or redistributed",
        f"// excluded: {', '.join(skipped) if skipped else 'none'}",
        "",
    ]
    # make sure the destination directory exists before writing
    out.parent.mkdir(parents=True, exist_ok=True)
    # concatenate every kept file under a source separator comment
    chunks = ["\n".join(header)]
    rules = 0
    for path in kept:
        text = path.read_text(encoding="utf-8", errors="replace")
        rules += len(RULE_PATTERN.findall(text))
        chunks.append(f"// ── source: {path.name}\n{text.rstrip()}\n")
    out.write_text("\n".join(chunks), encoding="utf-8")
    # report what was assembled so the operator can record provenance
    logging.info(f"included {len(kept)} files ({rules} rules), excluded {len(skipped)}")
    logging.info(f"wrote {out} ({out.stat().st_size} bytes)")


def main():
    """Fetch, extract, and assemble the pack."""
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    parser = argparse.ArgumentParser(
        description="Assemble the elastic/protections-artifacts YARA pack"
    )
    parser.add_argument("--out", required=True, help="pack file to write")
    parser.add_argument(
        "--workdir",
        help="fresh working directory for the untrusted download (default: a tempdir)",
    )
    parser.add_argument(
        "--exclude",
        action="append",
        default=[],
        metavar="FILE.yar",
        help="rule file basename to leave out (repeatable)",
    )
    args = parser.parse_args()
    # resolve the working directory, defaulting to a throwaway tempdir; a
    # reused directory could leak a prior fetch's stale rule files into the
    # pack, so an existing non-empty workdir is refused outright
    if args.workdir:
        workdir = pathlib.Path(args.workdir)
        if workdir.exists() and any(workdir.iterdir()):
            sys.exit(f"workdir {workdir} is not empty; pass a fresh directory")
        workdir.mkdir(parents=True, exist_ok=True)
    else:
        workdir = pathlib.Path(tempfile.mkdtemp(prefix="elastic-yara-"))
    logging.info(f"working directory: {workdir}")
    # fetch the source commit and its pinned tarball, then assemble the pack
    commit = resolve_commit()
    logging.info(f"source commit: {commit}")
    tarball = download_tarball(workdir, commit)
    rules_dir = extract_rules(tarball, workdir)
    assemble(rules_dir, pathlib.Path(args.out), commit, set(args.exclude))


if __name__ == "__main__":
    main()
