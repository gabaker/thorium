#!/usr/bin/env python3
"""Choose the tags and versions a workflow run publishes the Thorium image and Helm charts under.

The Thorium Build workflow (ci.yml) calls this script for both the image and the charts, so they
follow the same rules:

* every push and pull request builds, but only some runs publish:
  - every branch on forks, so a fork can test its own image and charts
  - only the default branch on the core repo
  - release tags, on the core repo and forks
  - never pull requests
* a release tag is a SemVer version (`X.Y.Z` or `X.Y.Z-<prerelease>`), with or without a leading
  `v`, since Helm needs SemVer chart versions and Docker cannot use `+` build metadata in an image
  tag. Any other tag builds and is scanned but publishes nothing, and the run logs a warning
  saying why; the decision is made before any job pushes
* branch names are used in image tags and chart versions as-is when they are valid there. A name
  that has to be changed (characters replaced, a `branch-` prefix added, or shortened) gets `-` and
  the first 8 hex digits of its sha256 appended, so two branches never share a tag or version

Two subcommands write `key=value` lines to `$GITHUB_OUTPUT` (and to stdout for the job log):

* `image` - the Thorium image's reference and whether to publish it, plus which release image
  `latest` should point at:
  - `image`: `ghcr.io/<owner>/<repo>/infrastructure/thorium:<tag>`, where the tag is the release
    tag as pushed (a leading `v` kept), `pr-<number>`, or the branch or other tag name made into
    a valid Docker tag
  - `image-amd64` and `image-arm64`: the same repository's tag for each architecture's
    single-platform image, which the publish pushes before joining them under `image`: the tag
    plus `-amd64` or `-arm64`, the tag shortened first when the two would pass Docker's 128
    characters
  - `latest-image`: the same repository's `latest` tag
  - `latest-source`: the highest `X.Y.Z` or `vX.Y.Z` release reachable from the default branch,
    set on default-branch runs only, otherwise empty: a release tag publishes its image under the
    tag alone and never moves `latest`, `main`, or a branch tag
  - `publish`: `true` or `false`
* `chart <Chart.yaml>...` - the version to package the charts as, whether to publish them, and the
  image the thorium chart should deploy (the image the `image` subcommand picks for this ref):
  - `version`: the release tag without a leading `v`, `<version>-pr.<number>`, or the
    prerelease `<version>-<branch or tag>.<run number>.g<short sha>`
  - `publish`: `true` or `false`
  - `image-repository` and `image-tag`

The run is described by the runner's default `GITHUB_*` variables (event name, ref name and type,
repository, run number, sha) and two values from the event payload the workflow passes in:
`IS_FORK` (`github.event.repository.fork`) and `DEFAULT_BRANCH`
(`github.event.repository.default_branch`). Reading them from the environment keeps branch and tag
names out of workflow template expansion.

Only the Python standard library and `git` are used.
"""

import argparse
import hashlib
import os
import re
import subprocess
import sys
from dataclasses import dataclass

# a release tag: a SemVer 2.0.0 version without build metadata. Release patterns spell out
# [0-9] since \d also matches non-ASCII digits, and are used with fullmatch
RELEASE_RE = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-((?:0|[1-9][0-9]*|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9]*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
)
# the prefix a release tag may carry, which the chart version drops
RELEASE_TAG_PREFIX = "v"
# a final release (no prerelease), the only kind `latest` points at
FINAL_RELEASE_RE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
# a branch-derived image tag that could be mistaken for a release tag
RELEASE_PREFIX_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+")
# the version line of a Chart.yaml
CHART_VERSION_RE = re.compile(r"^version:\s*[\"']?([^\"'\s#]+)", re.MULTILINE)
# the longest image tag Docker accepts
MAX_IMAGE_TAG = 128
# the number of sha256 hex digits appended to a branch name that had to be changed
NAME_HASH_LENGTH = 8
# the architectures the Thorium image is built for, as Docker names them (`--platform linux/<arch>`)
ARCHITECTURES = ("amd64", "arm64")
# the longest branch identifier kept in a chart prerelease version, so the version stays well under
# the 128 character OCI tag limit
MAX_CHART_BRANCH = 64


class ReleaseRefError(Exception):
    """A run whose ref cannot be published under a valid tag or version."""


@dataclass
class Run:
    """The parts of a workflow run that decide its tags and versions."""

    # the triggering event, such as `push` or `pull_request`
    event: str
    # `branch` or `tag`
    ref_type: str
    # the branch or tag name (`<number>/merge` for pull requests)
    ref_name: str
    # the `owner/repo` the workflow runs in
    repository: str
    # the repository's default branch
    default_branch: str
    # whether the repository is a fork
    is_fork: bool
    # the workflow's run number
    run_number: str
    # the commit being built
    sha: str

    @classmethod
    def from_env(cls, env):
        """Build a run from the runner's environment.

        # Arguments

        * `env` - The environment, usually `os.environ`
        """
        # fail clearly when a variable the workflow should set is missing
        required = ["GITHUB_EVENT_NAME", "GITHUB_REF_NAME", "GITHUB_REPOSITORY", "DEFAULT_BRANCH"]
        missing = [name for name in required if not env.get(name)]
        if missing:
            raise ReleaseRefError(f"missing environment variables: {', '.join(missing)}")
        return cls(
            event=env["GITHUB_EVENT_NAME"],
            ref_type=env.get("GITHUB_REF_TYPE", "branch"),
            ref_name=env["GITHUB_REF_NAME"],
            repository=env["GITHUB_REPOSITORY"],
            default_branch=env["DEFAULT_BRANCH"],
            is_fork=env.get("IS_FORK", "false") == "true",
            run_number=env.get("GITHUB_RUN_NUMBER", "0"),
            sha=env.get("GITHUB_SHA", ""),
        )

    @property
    def is_pull_request(self):
        """Whether the run was started by a pull request."""
        # pull_request_target runs also build untrusted pull request refs, so they never publish
        return self.event in ("pull_request", "pull_request_target")

    @property
    def is_tag(self):
        """Whether the run builds a pushed tag."""
        # pull request refs are never tags, whatever the ref type says
        return not self.is_pull_request and self.ref_type == "tag"

    @property
    def pr_number(self):
        """The pull request number, taken from the `<number>/merge` ref name."""
        # the ref name is `<number>/merge`
        return self.ref_name.split("/", 1)[0]


def tag_release_version(tag):
    """Return the release version a tag names, or None when the tag is not a release tag.

    The version is the tag without a leading `v`; the tag itself, `v` included, is the image tag,
    so it must also fit Docker's tag length limit.

    # Arguments

    * `tag` - The pushed tag name
    """
    # the workflow's tag filter allows any suffix, so the full SemVer rule is enforced here
    version = tag.removeprefix(RELEASE_TAG_PREFIX)
    if RELEASE_RE.fullmatch(version) and len(tag) <= MAX_IMAGE_TAG:
        return version
    return None


def tag_problem(run):
    """Return why a tag run publishes nothing, or None when the run is not a tag or is a release.

    # Arguments

    * `run` - The workflow run
    """
    # only tags that are not release versions are refused
    if not run.is_tag or tag_release_version(run.ref_name) is not None:
        return None
    return (
        f"tag {run.ref_name!r} is not a release tag, so this run builds and scans it but publishes "
        f"nothing: a release tag is a SemVer version of at most {MAX_IMAGE_TAG} characters, X.Y.Z "
        "or vX.Y.Z, optionally followed by '-' and dot-separated prerelease identifiers "
        "(letters, digits, '-')"
    )


def should_publish(run):
    """Decide whether the run publishes its image and charts.

    # Arguments

    * `run` - The workflow run
    """
    # pull requests never publish; tags publish only when they are release versions
    if run.is_pull_request:
        return False
    if run.is_tag:
        return tag_release_version(run.ref_name) is not None
    # branches publish everywhere on forks but only from the default branch on the core repo
    return run.is_fork or run.ref_name == run.default_branch


def image_repository(run):
    """Return the Thorium image repository in the run's own GitHub container registry namespace.

    # Arguments

    * `run` - The workflow run
    """
    # registry paths must be lowercase, while GitHub owner and repo names may not be
    return f"ghcr.io/{run.repository}/infrastructure/thorium".lower()


def with_name_hash(name, branch, limit):
    """Append a hash of the original branch name to a changed name, shortening it to fit a limit.

    # Arguments

    * `name` - The name made from the branch, which may be empty
    * `branch` - The original branch name the hash is taken from
    * `limit` - The longest the result may be
    """
    # the hash keeps names that only differ in replaced characters apart
    digest = hashlib.sha256(branch.encode()).hexdigest()[:NAME_HASH_LENGTH]
    if not name:
        return digest
    return f"{name[:limit - NAME_HASH_LENGTH - 1]}-{digest}"


def branch_image_tag(branch):
    """Turn a branch name into a valid Docker image tag that cannot replace a release or `latest`.

    # Arguments

    * `branch` - The branch name
    """
    # replace characters Docker does not allow in tags and strip leading '.' and '-'
    tag = re.sub(r"[^a-zA-Z0-9._-]", "-", branch).lstrip(".-")
    # keep branches from overwriting `latest` or release tags
    if tag == "latest" or RELEASE_PREFIX_RE.match(tag):
        tag = f"branch-{tag}"
    # a changed or too long name gets a hash of the branch name so it stays valid and distinct
    if tag != branch or len(tag) > MAX_IMAGE_TAG:
        tag = with_name_hash(tag, branch, MAX_IMAGE_TAG)
    return tag


def other_tag_image_tag(tag):
    """Turn a tag that is not a release into a valid Docker image tag for a build never published.

    # Arguments

    * `tag` - The pushed tag name
    """
    # replace characters Docker does not allow in tags; the prefix and hash keep the name from
    # looking like a release, `latest`, or a branch's tag
    name = re.sub(r"[^a-zA-Z0-9._-]", "-", tag)
    return with_name_hash(f"tag-{name}", tag, MAX_IMAGE_TAG)


def image_tag(run):
    """Return the tag the run's Thorium image is built under.

    # Arguments

    * `run` - The workflow run
    """
    # pull request images stay on the runner, so they only need a readable name
    if run.is_pull_request:
        return f"pr-{run.pr_number}"
    # a release tag is the image tag as pushed; any other tag only needs a valid name
    if run.is_tag:
        if tag_release_version(run.ref_name) is not None:
            return run.ref_name
        return other_tag_image_tag(run.ref_name)
    return branch_image_tag(run.ref_name)


def arch_image_tag(tag, arch):
    """Return the tag one architecture's single-platform image is pushed under.

    The tag is shortened before the architecture is added so the result fits Docker's limit; the
    image index under the tag itself is what users pull, so the shortened name only has to tell
    the run's two architecture images apart.

    # Arguments

    * `tag` - The tag the run's multi-arch image is published under
    * `arch` - The architecture, one of ARCHITECTURES
    """
    # keep room for the separator and the architecture
    return f"{tag[: MAX_IMAGE_TAG - len(arch) - 1]}-{arch}"


def final_release_version(tag):
    """Return the final release version a tag names (`X.Y.Z` or `vX.Y.Z`), or None.

    # Arguments

    * `tag` - A tag name
    """
    # the leading `v` is optional, as for every release tag
    version = tag.removeprefix(RELEASE_TAG_PREFIX)
    return version if FINAL_RELEASE_RE.fullmatch(version) else None


def release_sort_key(tag):
    """Return a key that orders final release tags by version.

    # Arguments

    * `tag` - A tag `final_release_version` accepts
    """
    # compare each numeric component as an integer so 1.10.0 sorts above 1.9.0, and prefer the
    # tag without a `v` when both spellings of a version exist
    version = final_release_version(tag)
    return (tuple(int(part) for part in version.split(".")), tag == version)


def highest_release(ref):
    """Return the highest final release tag reachable from a git ref, or None.

    # Arguments

    * `ref` - The git ref to search from, such as `origin/main`
    """
    # list the tags merged into the ref; the checkout must include history and tags
    result = subprocess.run(["git", "tag", "--merged", ref], capture_output=True, text=True)
    if result.returncode != 0:
        raise ReleaseRefError(f"cannot list the release tags on {ref}: {result.stderr.strip()}")
    # keep only final releases, then pick the highest by version
    releases = [tag for tag in result.stdout.split() if final_release_version(tag)]
    return max(releases, key=release_sort_key) if releases else None


def latest_source(run, repository):
    """Return the release image `latest` should point at after this run, or an empty string.

    `latest` follows the highest release on the default branch. Only default-branch runs re-point
    it, so a release tag's run publishes the tag alone, and a release is promoted by the next
    default-branch run that publishes, whether its tag was pushed before or after it was merged.

    # Arguments

    * `run` - The workflow run
    * `repository` - The image repository
    """
    # only default-branch runs move `latest`; tags, other branches, and pull requests never do
    is_default = not run.is_pull_request and not run.is_tag and run.ref_name == run.default_branch
    if not is_default:
        return ""
    # checkout fetches the current default branch, so a re-run of an older commit still finds the
    # newest release
    highest = highest_release(f"origin/{run.default_branch}")
    if not highest:
        return ""
    return f"{repository}:{highest}"


def chart_branch_identifier(branch):
    """Turn a branch name into a single SemVer prerelease identifier.

    # Arguments

    * `branch` - The branch name
    """
    # identifiers may only hold letters, digits and '-'
    identifier = re.sub(r"[^a-zA-Z0-9-]", "-", branch)
    # a changed or too long name gets a hash of the branch name so it stays distinct, as does an
    # all-digit name, which the '-' before the hash keeps from being a numeric identifier with a
    # leading zero
    if identifier != branch or len(identifier) > MAX_CHART_BRANCH or identifier.isdigit():
        identifier = with_name_hash(identifier, branch, MAX_CHART_BRANCH)
    return identifier


def read_chart_version(path):
    """Read the `version` from a Chart.yaml.

    # Arguments

    * `path` - The Chart.yaml path
    """
    # Chart.yaml is read as text so no YAML library is needed
    with open(path, encoding="utf-8") as chart:
        match = CHART_VERSION_RE.search(chart.read())
    # a chart without a version cannot be packaged
    if not match:
        raise ReleaseRefError(f"{path} has no version")
    return match.group(1)


def chart_version(run, version):
    """Return the version the run packages the charts as.

    # Arguments

    * `run` - The workflow run
    * `version` - The version every chart's Chart.yaml declares
    """
    # pull request charts are never published, so they only need a readable version
    if run.is_pull_request:
        return f"{version}-pr.{run.pr_number}"
    # a release tag is the chart version without its leading `v`
    if run.is_tag:
        release = tag_release_version(run.ref_name)
        if release is not None:
            return release
    # any other tag, which never publishes, is versioned like a branch. The g prefix keeps an
    # all-digit short sha with a leading zero a valid identifier
    branch = chart_branch_identifier(run.ref_name)
    return f"{version}-{branch}.{run.run_number}.g{run.sha[:7]}"


def image_outputs(run):
    """Return the `image` subcommand's outputs.

    # Arguments

    * `run` - The workflow run
    """
    # build the image reference, its architecture images, and the latest pointer from one repository
    repository = image_repository(run)
    tag = image_tag(run)
    arch_images = {f"image-{arch}": f"{repository}:{arch_image_tag(tag, arch)}" for arch in ARCHITECTURES}
    return {
        "image": f"{repository}:{tag}",
        **arch_images,
        "latest-image": f"{repository}:latest",
        "latest-source": latest_source(run, repository),
        "publish": str(should_publish(run)).lower(),
    }


def chart_outputs(run, charts):
    """Return the `chart` subcommand's outputs.

    # Arguments

    * `run` - The workflow run
    * `charts` - The Chart.yaml paths, which must all declare the same version
    """
    # the charts are released together, so their versions must agree
    versions = {path: read_chart_version(path) for path in charts}
    if len(set(versions.values())) != 1:
        found = ", ".join(f"{path} ({version})" for path, version in versions.items())
        raise ReleaseRefError(f"chart versions differ: {found}")
    version = next(iter(versions.values()))
    # the chart deploys the image the image subcommand picks for this ref
    return {
        "version": chart_version(run, version),
        "publish": str(should_publish(run)).lower(),
        "image-repository": image_repository(run),
        "image-tag": image_tag(run),
    }


def write_outputs(outputs):
    """Print outputs to the job log and append them to `$GITHUB_OUTPUT` when it is set.

    # Arguments

    * `outputs` - The output names and values
    """
    # every value is a single line, so the simple key=value form is safe
    lines = [f"{key}={value}" for key, value in outputs.items()]
    print("\n".join(lines))
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a", encoding="utf-8") as output:
            output.write("\n".join(lines) + "\n")


def main():
    """Parse arguments, compute the requested outputs, and write them."""
    # each subcommand computes one workflow's outputs
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("image", help="the Thorium image's tags and publish decision")
    chart = commands.add_parser("chart", help="the Helm charts' version, publish decision and image")
    chart.add_argument("charts", nargs="+", help="Chart.yaml files that must share one version")
    args = parser.parse_args()
    # report problems as workflow errors so they show as annotations
    try:
        run = Run.from_env(os.environ)
        outputs = image_outputs(run) if args.command == "image" else chart_outputs(run, args.charts)
    except (ReleaseRefError, OSError) as error:
        print(f"::error::{error}")
        return 1
    # a tag that publishes nothing says why in a warning annotation
    problem = tag_problem(run)
    if problem:
        print(f"::warning::{problem}")
    write_outputs(outputs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
