#!/usr/bin/env python3
"""Check that Thorium's GitHub workflows keep to the CI change gating rules.

Builds and security scans run on every run, on every branch, tag, and pull request, while lints,
unit tests, and integration tests run only when the code they cover changed. This script reads
the workflows under .github/workflows and fails when any of these rules is broken:

* every job of every workflow is classified in `CATEGORIES` as build, security, lint, unit,
  integration, tool-build, or other, so a new job fails this check until it is classified
* build and security jobs never read the changed paths (`needs.changes`), and a workflow holding
  one has no `paths:` or `paths-ignore:` filter on its triggers; a workflow holding a build job
  builds on pushes to every branch and release tags and on every pull request. The one exception
  is ci.yml's publish jobs (`PUBLISH_JOBS`), which read only the changes job's `publish-image`
  output, so the core image publishes on the same changes it always did
* ci.yml calls the security workflows (`SECURITY_CALLS`) on every run
* every scan sits in the security group: ci.yml runs no scanner job of its own (its security jobs
  only call `SECURITY_CALLS`), and every job that runs a Trivy image or filesystem scan is in
  security-scan.yml, except the jobs in `SCAN_EXEMPTIONS`; dockerfile-lint.yml's Trivy config
  scan of the Dockerfiles is a lint and stays there
* no publish job (`PUBLISH_ROOTS`), nor any job it needs directly or through other jobs, needs a
  scan, lint, or test job, directly or through `needs.<job>` in its `if:`, and the image and
  chart publish (`PUBLISH_JOBS`) each need the image build and the chart package, which needs no
  other job so it starts with the builds, and the chart publish needs the image publish, so a
  chart is pushed only after the image it deploys
* the scans always run: ci.yml's security-scan.yml caller runs whatever the build concluded,
  reads nothing of the changes job, needs the image build and no publish job, passes the saved
  image's artifact, and no ci.yml job needs it; security-scan.yml's repo scans (`REPO_SCANS`)
  need no job and don't depend on the image input
* security-scan.yml rescans the published images, with no separate rescan workflow
  (`RETIRED_WORKFLOWS`) beside it: it has a schedule and workflow_dispatch, scans the build's
  image only when called and the published images only on its own runs, uploads both scans to
  code scanning (`IMAGE_SCAN_CATEGORIES`, the build's on pushes only), and gives
  `security-events: write` only to jobs that run no scanner
* every lint and test job in ci.yml runs only when its area changed (`GATED_JOBS`), and
  rust-check.yml's thoradm and thorctl tests follow the `thoradm` and `thorctl` inputs ci.yml
  passes it
* a lint or test job in any other workflow that runs on pushes or pull requests is gated by its
  workflow's trigger filters, and tools.yml builds tool images only from main, and when it has the
  toolbox check, checks the toolbox on pull requests that change tools/ or .github/
* ci.yml's "Choose the stages to run" step, run with bash in scratch git repositories, sets every
  area on the default branch, manual runs, new branches, and .github/ changes, sets none on
  release tags, and sets the expected areas for changes to single parts of the repository, and
  sets `publish-image` only on release tags, runs with nothing to compare, and changes to the
  paths the image is built from (a push to the default branch included)
* a release tag publishes as the tag itself: ci.yml runs on pushes of `X.Y.Z*` and `vX.Y.Z*`
  tags (`RELEASE_TAG_FILTERS`), and run with a stubbed git history, release_refs.py gives a
  release tag run (`TAG_RUNS`) the image `<repository>:<tag>` and a chart image tag of `<tag>`,
  the tag as pushed, and the chart version `<tag>` less a leading `v`, publishes it, and sets no
  `latest-source`, which only default-branch runs set (and never a branch's tag); a tag that is
  not SemVer (`NON_RELEASE_TAGS`) publishes neither image nor charts, is never built as a
  release, `latest`, or `main`, and gets a reason to log; in ci.yml the image publish writes only
  the build job's `image` output (and its architecture images), the step moving `latest` runs on branch runs only, and the
  chart package passes the chart version and image tag release_refs.py picks. Each release tag's
  architecture images (`IMAGE_ARCHES`) push as `<tag>-<arch>`; the publish pushes only the build
  job's image-<arch> outputs and, once both are pushed, joins them under its `image` output
* the core image builds for every architecture of `IMAGE_ARCHES` without emulation: in the
  Dockerfile only build-platform stages and the native final stages (`NATIVE_IMAGE_ARCHES`) run
  steps, TARGETARCH picks the server and final stages, each architecture's crane checksum is
  pinned, and each server-<arch> stage copies every server binary (`SERVER_BINARIES`) from that
  architecture's build; ci.yml's core image job needs that build, stages its artifact, and
  builds, checks (check_image_arch.sh), and saves each architecture's image
* ci.yml's macOS build (`MACOS_JOB`) is one job, so a run asks for one macOS runner; it needs
  no other job, so it runs in parallel with the other builds, builds each binary of
  `MACOS_BINARIES` once per macOS target, each binary and target in its own cargo run (one `-p`,
  one `--target`) with every variable of `MACOS_BUILD_ENV` set, builds nothing else, and uploads
  one artifact per binary and target; the core image build needs it and downloads its artifacts,
  and the Dockerfile copies each of those binaries into the image
* each of ci.yml's static Linux (musl) builds (`MUSL_JOBS`) is a plain job with a fixed name (no
  matrix) that needs no other job, builds every binary in `MUSL_BINARIES` for its target in one
  actions-rust-cross run through cross (`force-use-cross` where the runner could build the target
  itself) with the variables `MUSL_JOBS` lists for it (cargo's strip and a static liblzma), and
  uploads them; the core image build needs it, downloads its artifact
  and stages it where the Dockerfile copies each binary from into its binaries/linux directory
* every step that compiles Rust (`RUST_COMPILE_RE`, or actions-rust-cross) builds each of its
  targets with the target features gxhash needs (`RUSTFLAGS_BY_ARCH`): exactly `+aes,+sse2` on
  every x86_64 target, macOS included, and `+aes,+neon` on aarch64 Linux, from RUSTFLAGS or the
  target's CARGO_TARGET_<TRIPLE>_RUSTFLAGS; aarch64 macOS has AES (on by default for the target)
  and no SSE2. A step whose target or flags are a GitHub expression can't be checked and fails;
  no aarch64 target takes its flags from a workflow-wide RUSTFLAGS; the Linux musl and macOS
  targets build with `LZMA_API_STATIC=1` (`STATIC_LZMA_TARGETS`); and ci.yml builds every target
  of `RUSTFLAGS_TARGETS`, so the table can't go stale
* workflow-lint.yml runs zizmor, the action pin check, and actionlint (`WORKFLOW_LINT_TOOLS`) in
  one job (`WORKFLOW_LINT_JOB`) with a read-only token: each tool's run and report steps continue
  on failure, each tool reports through lint_report.py, and a last step reads every one of those
  steps' outcomes to set the job's result
* code scanning gets the security and supply-chain lints and nothing else: every lint_report.py
  `--tool` is listed in `SARIF_LINTS` or `NO_SARIF_LINTS`, every `SARIF_LINTS` tool writes
  `--sarif-output` (outside `SARIF_EXEMPT_JOBS`), every `SARIF_LINTS` category is uploaded once,
  and no other category is
* every job's display name starts with a category prefix (`JOB_NAME_PREFIXES`), and
  rerun-unassigned.yml's `BUILD_JOBS` pattern matches the display name of every ci.yml build job
  and of the changes job (`RERUN_WEIGHED_JOBS`), and of no other ci.yml job, including the jobs of
  the workflows ci.yml calls, which GitHub names `<caller> / <job>`
* a job holding `security-events: write` runs no scanner: its steps use only the artifact download
  and SARIF upload actions (`UPLOAD_JOB_ACTIONS`), check out nothing, and run no command
  `SCANNER_COMMAND_RE` matches

Each broken rule is printed as a `FAIL:` line and the script exits 1. Only the standard library,
PyYAML, git, and bash are used, and nothing is fetched from the network. `--root` checks another
checkout's .github, such as an export of an older commit.
"""

import argparse
import fnmatch
import importlib.util
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

# the repository root, two levels above .github/scripts
DEFAULT_ROOT = Path(__file__).resolve().parents[2]
# every job of every workflow, as workflow file -> job id -> category
CATEGORIES = {
    "ci.yml": {
        "changes": "other",
        "build-mdbook-docs": "build",
        "build-ui-bundle": "build",
        "build-glibc-binaries": "build",
        "build-cross-binaries": "build",
        "build-arm64-cross-binaries": "build",
        "build-arm64-glibc-binaries": "build",
        "build-windows-cross-binaries": "build",
        "build-macos-binaries": "build",
        "build-core-container": "build",
        "publish-core-container": "build",
        "helm-package": "build",
        "helm-publish": "build",
        "deploy-updated-pages": "build",
        "security-scan": "security",
        "workflow-lint": "security",
        "dockerfile-lint": "security",
        "rust-check": "lint",
        "versions": "lint",
        "helm-lint": "lint",
        "deploy-tools-shell": "lint",
        "megathor-values": "lint",
    },
    "rust-check.yml": {
        "check": "lint",
        "clippy": "lint",
        "fmt": "lint",
        "test": "unit",
        "thoradm-test": "unit",
        "thorctl-test": "unit",
        "integration-tests-check": "integration",
    },
    "security-scan.yml": {
        "trivy-secret": "security",
        "trivy-iac": "security",
        "trivy-deps": "security",
        "upload": "security",
        "scan-core-image": "security",
        "upload-core-image-scan": "security",
        "scan-published-images": "security",
        "upload-published-image-scan": "security",
    },
    "workflow-lint.yml": {
        "lint": "security",
        "upload": "security",
    },
    "dockerfile-lint.yml": {
        "hadolint": "security",
        "trivy": "security",
        "upload": "security",
        "tool-versions": "security",
    },
    "tools.yml": {
        "check-toolbox": "unit",
        "generate-tool-builds": "tool-build",
        "build-tools": "tool-build",
    },
    "rerun-unassigned.yml": {
        "rerun": "other",
    },
    # only in checkouts that have ThorPal
    "thorpal-harness.yml": {
        "stage-harness-sources": "build",
        "build-harness-image": "build",
        "publish-harness-image": "build",
    },
}
# the categories a job can be classified as
KNOWN_CATEGORIES = {"build", "security", "lint", "unit", "integration", "tool-build", "other"}
# the categories that run only when the code they cover changed
GATED_CATEGORIES = {"lint", "unit", "integration"}
# the area (or areas) of changed paths each lint and test job in ci.yml is gated on
GATED_JOBS = {
    "rust-check": "rust",
    "versions": ("versions", "ci"),
    "helm-lint": "charts",
    "deploy-tools-shell": "deploy-tools",
    "megathor-values": "deploy-tools",
}
# the reusable security workflows ci.yml must call on every run
SECURITY_CALLS = ["security-scan.yml", "workflow-lint.yml", "dockerfile-lint.yml"]
# rust-check.yml's per-crate test jobs and the input that gates each
CRATE_TEST_INPUTS = {"thoradm-test": "thoradm", "thorctl-test": "thorctl"}
# the paths tools.yml's pull request trigger must cover
TOOLS_PR_PATHS = [
    "tools/toolbox.json",
    "tools/config.toml",
    "tools/images/**",
    "tools/pipelines/**",
    "tools/scripts/**",
    ".github/**",
]
# the outputs of ci.yml's changes job, one per gated area
AREAS = ["rust", "thoradm", "thorctl", "charts", "deploy-tools", "versions", "ci"]
# the changes job's output that decides whether a ref that publishes pushes the core image
PUBLISH_OUTPUT = "publish-image"
# ci.yml's publish jobs, which may read only PUBLISH_OUTPUT of the changes job
PUBLISH_JOBS = ["publish-core-container", "helm-publish"]
# ci.yml's image publish, and its chart publish, which needs the image publish
IMAGE_PUBLISH_JOB = "publish-core-container"
HELM_PUBLISH_JOB = "helm-publish"
# ci.yml's jobs that publish something; none of them, nor any job they need, may need a check
PUBLISH_ROOTS = ["publish-core-container", "helm-publish", "deploy-updated-pages"]
# ci.yml's job that packages the Helm charts, which every job in PUBLISH_JOBS needs
HELM_PACKAGE_JOB = "helm-package"
# the workflow every scan job sits in, and the job in ci.yml that calls it
SECURITY_WORKFLOW = "security-scan.yml"
SECURITY_CALLER = "security-scan"
# security-scan.yml's scans of the repository, which wait for nothing
REPO_SCANS = ["trivy-secret", "trivy-iac", "trivy-deps"]
# security-scan.yml's scan of the core image ci.yml built, and its rescan of the published images
BUILD_IMAGE_SCAN = "scan-core-image"
PUBLISHED_IMAGE_SCAN = "scan-published-images"
# security-scan.yml's workflow_call inputs naming the saved core image's artifact and the image
IMAGE_INPUTS = ["core-image-artifact", "core-image"]
# the text of the condition that tells security-scan.yml's own runs from runs ci.yml called
STANDALONE = "contains(github.workflow_ref, '/.github/workflows/security-scan.yml@')"
# the code scanning categories of the build's image scan and the published images' rescan
IMAGE_SCAN_CATEGORIES = ["trivy-image-build", "trivy-image-published"]
# a separate published image rescan workflow, which must not exist beside security-scan.yml's rescan
RETIRED_WORKFLOWS = ["image-rescan.yml"]
# jobs outside SECURITY_WORKFLOW allowed to run a Trivy image or filesystem scan, by workflow
SCAN_EXEMPTIONS = {
    # only in checkouts that have ThorPal: it builds and scans the harness image in one job
    "thorpal-harness.yml": {"build-harness-image"},
}
# a Trivy image or filesystem scan, run from the pinned image or a trivy binary
TRIVY_SCAN = re.compile(r'(?:\$\{?TRIVY_IMAGE\}?"?|\btrivy)\s+(?:image|fs|filesystem|rootfs)\b')
# ci.yml's one job that builds the macOS binaries, the binaries it builds (each in its own cargo
# run, so thorctl compiles thorium-api without thoradm's server features), and the job that builds
# the core image from them
MACOS_JOB = "build-macos-binaries"
MACOS_BINARIES = ["thorctl", "thoradm"]
CORE_IMAGE_JOB = "build-core-container"
# the macOS targets the core image serves each macOS binary for
MACOS_TARGETS = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
# the variables the macOS cargo build step must set, so the binaries link no library the runner
# found outside the system ones (liblzma-sys links a pkg-config liblzma unless LZMA_API_STATIC is set)
MACOS_BUILD_ENV = {"LZMA_API_STATIC": "1"}
# ci.yml's static Linux (musl) build jobs, as job id -> (target, the variables its build step must
# set besides its RUSTFLAGS, the directory under binaries/linux the image serves the target's
# binaries from). Both builds run in cross, which the action never strips after, so cargo strips
# them, and cross passes LZMA_API_STATIC (a bundled liblzma, as on macOS) into its container only
# when told to
MUSL_JOBS = {
    "build-cross-binaries": (
        "x86_64-unknown-linux-musl",
        {
            "CARGO_PROFILE_RELEASE_STRIP": "symbols",
            "LZMA_API_STATIC": "1",
            "CROSS_BUILD_ENV_PASSTHROUGH": "LZMA_API_STATIC",
        },
        "x86-64",
    ),
    "build-arm64-cross-binaries": (
        "aarch64-unknown-linux-musl",
        {
            "CARGO_PROFILE_RELEASE_STRIP": "symbols",
            "LZMA_API_STATIC": "1",
            "CROSS_BUILD_ENV_PASSTHROUGH": "LZMA_API_STATIC",
        },
        "arm64",
    ),
}
# the Linux musl targets the runner could build without cross, so their build step must set
# actions-rust-cross's force-use-cross to build them in cross like the other musl targets
MUSL_FORCED_CROSS_TARGETS = {"x86_64-unknown-linux-musl"}
# the binaries the core image serves for each Linux musl target
MUSL_BINARIES = ["thorctl", "thorium-agent", "thorium-reactor", "thoradm", "thorium-operator"]
# the action that builds the cross-compiled binaries
RUST_CROSS_ACTION = "houseabsolute/actions-rust-cross@"
# the RUSTFLAGS every non-macOS target of an architecture builds with: gxhash needs AES, with SSE2
# on x86_64 and NEON on aarch64, and rustc rejects another architecture's features
RUSTFLAGS_BY_ARCH = {
    "x86_64": "-C target-feature=+aes,+sse2",
    "aarch64": "-C target-feature=+aes,+neon",
}
# the target features a target enables by default, which its flags need not name
DEFAULT_TARGET_FEATURES = {"aarch64-apple-darwin": {"aes", "neon"}}
# the targets ci.yml's release builds must each build in some step, so every architecture's flags
# are checked: the Linux glibc and musl, Linux arm64 glibc and musl, Windows, and both macOS targets
RUSTFLAGS_TARGETS = [
    "x86_64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
]
# the targets whose builds link liblzma statically (LZMA_API_STATIC=1): the binaries the image
# serves for Linux musl and macOS, whose hosts may have no liblzma the build found, and the arm64
# server binaries, whose cross image has no arm64 liblzma
STATIC_LZMA_TARGETS = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
]
# the architectures the core image is built for, as release_refs.py's image-<arch> outputs, the
# Dockerfile's server-<arch> and final-<arch> stages, and `docker build --platform linux/<arch>`
# name them, with the directory the Dockerfile copies the arch's server binaries from, the job
# that builds them, and the artifact it uploads them in
IMAGE_ARCHES = {
    "amd64": ("target/release", "build-glibc-binaries", "thorium-glibc-binaries"),
    "arm64": ("target/aarch64-unknown-linux-gnu/release", "build-arm64-glibc-binaries", "aarch64-unknown-linux-gnu-binaries"),
}
# the server binaries the core image runs, which each architecture's server stage copies
SERVER_BINARIES = [
    "thorium-api",
    "thorium-operator",
    "thoradm",
    "thorium-scaler",
    "thorium-search-streamer",
    "thorium-event-handler",
]
# the architectures whose final image stage may run steps: the build runners are x86_64, so any
# other architecture's RUN would need emulation
NATIVE_IMAGE_ARCHES = {"amd64"}
# a shell command that compiles the workspace, and so builds gxhash
RUST_COMPILE_RE = re.compile(r"\b(?:cargo(?:\s+\+\S+)?\s+(?:build|check|clippy|test|run|doc|nextest)|maturin\s+(?:build|develop))\b")
# the release tags check_tag_publish runs release_refs.py for, with the chart version each must
# package as: final releases and prereleases, with and without a leading `v`
TAG_RUNS = {"1.2.3": "1.2.3", "1.2.3-rc.1": "1.2.3-rc.1", "v1.2.3": "1.2.3", "v2.0.0-rc.1": "2.0.0-rc.1"}
# the chart version check_tag_publish's Chart.yaml declares; a release tag need not match it
TAG_CHART_VERSION = "1.2.3"
# tags ci.yml's tag filters let through that are not SemVer releases, so they publish nothing
NON_RELEASE_TAGS = ["1.2.3.4", "1.2.3_x", "v1.2.3+build", "v01.2.3", "1.2.3-"]
# the tag filters of ci.yml's push trigger: release tags with and without a leading `v`
RELEASE_TAG_FILTERS = ["[0-9]+.[0-9]+.[0-9]+*", "v[0-9]+.[0-9]+.[0-9]+*"]
# the highest release check_tag_publish stubs as reachable from the default branch, so a tag run
# that is that release would move `latest` if tag runs could
TAG_HIGHEST_RELEASE = "1.2.3"
# the name of the step in ci.yml's changes job that picks the areas
DECIDE_STEP = "Choose the stages to run"
# the category prefixes every job's display name starts with
JOB_NAME_PREFIXES = ("Build · ", "Publish · ", "Scan · ", "Lint · ", "Check · ", "Test · ")
# the ci.yml jobs outside the build category whose failures rerun-unassigned.yml still weighs
RERUN_WEIGHED_JOBS = {"changes"}
# a GitHub expression in a display name, replaced by a sample value when names are matched
EXPRESSION_RE = re.compile(r"\$\{\{.*?\}\}")
# the triggers whose path filters decide whether a workflow runs at all
PATH_TRIGGERS = ("push", "pull_request", "pull_request_target")
# workflow-lint.yml's one lint job, and the tools it runs, each found by a pattern in a run: step
WORKFLOW_LINT_JOB = "lint"
WORKFLOW_LINT_TOOLS = {
    "zizmor": r"\$ZIZMOR_IMAGE\b",
    "action-pins": r"check_action_pins\.py",
    "actionlint": r"\$ACTIONLINT_IMAGE\b",
}
# the security and supply-chain lints whose findings go to code scanning, as lint_report.py --tool
# name -> the upload categories of its SARIF (the image scan uploads one per image source)
SARIF_LINTS = {
    "zizmor": ["zizmor"],
    "action-pins": ["action-pins"],
    "actionlint": ["actionlint"],
    "hadolint": ["hadolint"],
    "trivy-config": ["trivy-config"],
    "tool-versions": ["tool-versions"],
    "trivy-secret": ["trivy-secret"],
    "trivy-iac": ["trivy-iac"],
    "trivy-deps": ["trivy-deps"],
    "trivy-image": ["trivy-image-build", "trivy-image-published"],
}
# the lint_report.py tools kept out of code scanning, so it holds only security and supply-chain
# findings: the compile and formatting lints, the schema change warnings, and the CI name limits
NO_SARIF_LINTS = {"cargo-check", "clippy", "rustfmt", "schema", "ci-name-limits"}
# jobs whose SARIF_LINTS report need not write SARIF, as (workflow, job) -> why. Only in checkouts
# that have ThorPal: the harness image scan runs in its build job, which has no upload job yet
SARIF_EXEMPT_JOBS = {
    ("thorpal-harness.yml", "build-harness-image"): "the ThorPal harness image scan reports in its build job only",
}
# the only actions a job holding `security-events: write` may use
UPLOAD_JOB_ACTIONS = ("actions/download-artifact@", "github/codeql-action/upload-sarif@")
# commands that would run a scanner, an interpreter, or repository code with an upload job's token:
# these command names, any path to a script or binary, and `.py` or `.sh` files
SCANNER_COMMAND_RE = re.compile(
    r"(docker|podman|python3?|node|npm|npx|bash|sh|cargo|helm|git|trivy|zizmor|actionlint|hadolint|shellcheck"
    r"|source|\.|eval|exec)|.*/.*|.*\.(py|sh)"
)
# what separates the simple commands of a shell line
COMMAND_SEPARATOR_RE = re.compile(r"&&|\|\||[;|&(]|\$\(|`")
# shell words that come before a command rather than being one
COMMAND_PREFIXES = {"if", "then", "elif", "else", "while", "until", "do", "!", "{", "time", "command", "env"}
# a lint_report.py command, with its backslash-continued arguments
LINT_REPORT_RE = re.compile(r"lint_report\.py\b((?:\\\n|[^\n])*)")


class Checker:
    """Collects the broken rules found while checking one checkout.

    # Arguments

    * `root` - The checkout whose .github/workflows is checked
    """

    def __init__(self, root):
        self.root = Path(root)
        self.failures = []
        self.checks = 0

    def expect(self, ok, message):
        """Record one rule, failing it when it doesn't hold.

        # Arguments

        * `ok` - Whether the rule holds
        * `message` - What is wrong when it doesn't
        """
        self.checks += 1
        if not ok:
            self.failures.append(message)


def triggers(workflow):
    """Return a workflow's triggers as a dict of trigger name to its settings.

    # Arguments

    * `workflow` - The parsed workflow, whose `on` key PyYAML reads as True
    """
    on = workflow.get("on", workflow.get(True))
    if isinstance(on, str):
        return {on: {}}
    if isinstance(on, list):
        return {name: {} for name in on}
    return {name: (settings or {}) for name, settings in (on or {}).items()}


def as_list(value):
    """Return a YAML value that may be a single item or a list as a list.

    # Arguments

    * `value` - The value, which may be missing
    """
    if value is None:
        return []
    return value if isinstance(value, list) else [value]


def load_workflows(checker):
    """Parse every workflow under .github/workflows.

    # Arguments

    * `checker` - The checker to record unreadable workflows on
    """
    workflows = {}
    for path in sorted((checker.root / ".github" / "workflows").glob("*.y*ml")):
        try:
            workflows[path.name] = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
        except yaml.YAMLError as error:
            checker.expect(False, f"{path.name} is not valid YAML: {error}")
    checker.expect("ci.yml" in workflows, "no .github/workflows/ci.yml")
    return workflows


def check_classified(checker, workflows):
    """Check every job of every workflow has a known category.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    for name, workflow in workflows.items():
        table = CATEGORIES.get(name, {})
        for job in workflow.get("jobs") or {}:
            category = table.get(job)
            checker.expect(
                category in KNOWN_CATEGORIES,
                f"{name}: job {job} is not classified in check_ci_gating.py's CATEGORIES",
            )


def check_ungated(checker, workflows):
    """Check build and security jobs, and their workflows' triggers, have no path conditions.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    for name, workflow in workflows.items():
        table = CATEGORIES.get(name, {})
        jobs = workflow.get("jobs") or {}
        ungated = [job for job in jobs if table.get(job) in ("build", "security")]
        for job in ungated:
            text = yaml.safe_dump(jobs[job], width=1000)
            publishes = name == "ci.yml" and job in PUBLISH_JOBS
            # a publish job may read whether the image publishes, and nothing else the changes job decides
            if publishes:
                text = text.replace(f"needs.changes.outputs.{PUBLISH_OUTPUT}", "")
            checker.expect(
                "needs.changes" not in text,
                f"{name}: {table[job]} job {job} reads the changed paths: if: {jobs[job].get('if')}",
            )
            checker.expect(
                publishes or "changes" not in as_list(jobs[job].get("needs")),
                f"{name}: {table[job]} job {job} needs the changes job",
            )
        if not ungated:
            continue
        on = triggers(workflow)
        for trigger in PATH_TRIGGERS:
            settings = on.get(trigger) or {}
            for key in ("paths", "paths-ignore"):
                checker.expect(
                    key not in settings,
                    f"{name}: runs build or security jobs ({', '.join(ungated)}) but its {trigger} trigger has a {key}: filter",
                )
        if any(table.get(job) == "build" for job in ungated):
            check_builds_every_ref(checker, name, on)


def check_builds_every_ref(checker, name, on):
    """Check a workflow holding build jobs builds every branch, release tag, and pull request.

    # Arguments

    * `checker` - The checker to record failures on
    * `name` - The workflow's file name
    * `on` - The workflow's triggers
    """
    push = on.get("push")
    checker.expect(push is not None, f"{name}: builds but has no push trigger")
    checker.expect("pull_request" in on, f"{name}: builds but has no pull_request trigger")
    if push is None:
        return
    branches = as_list(push.get("branches"))
    # a missing branches list with tags set would leave out every branch
    checker.expect(
        "**" in branches and all(branch == "**" or branch.startswith("!") for branch in branches),
        f"{name}: builds on pushes to only some branches ({branches}); it must build every branch",
    )
    checker.expect(bool(as_list(push.get("tags"))), f"{name}: builds but not on pushes of release tags")
    checker.expect(
        not as_list(push.get("branches-ignore")),
        f"{name}: builds but ignores pushes to some branches with branches-ignore",
    )


def check_ci_jobs(checker, workflows):
    """Check ci.yml calls the security workflows on every run and gates each lint and test.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    for called in SECURITY_CALLS:
        callers = [job for job, spec in jobs.items() if str(spec.get("uses", "")).endswith(f"/{called}")]
        checker.expect(bool(callers), f"ci.yml: does not call {called}")
        for job in callers:
            condition = str(jobs[job].get("if", ""))
            checker.expect(
                "needs.changes" not in condition,
                f"ci.yml: calls {called} (job {job}) only for some changes: if: {condition}",
            )
    for job in GATED_JOBS:
        checker.expect(job in jobs, f"ci.yml: has no {job} job")
    for job, spec in jobs.items():
        if CATEGORIES["ci.yml"].get(job) not in GATED_CATEGORIES:
            continue
        area = GATED_JOBS.get(job)
        checker.expect(area is not None, f"ci.yml: lint or test job {job} has no area in check_ci_gating.py's GATED_JOBS")
        if area is None:
            continue
        condition = str(spec.get("if", ""))
        # a job can run for any of several areas, such as the version job's gating check
        for name in (area,) if isinstance(area, str) else area:
            checker.expect(
                f"needs.changes.outputs.{name} ==" in condition,
                f"ci.yml: {job} is not gated on its area {name}: if: {condition or '(none)'}",
            )
    outputs = (jobs.get("changes") or {}).get("outputs") or {}
    for area in AREAS:
        checker.expect(area in outputs, f"ci.yml: the changes job has no {area} output")
    checker.expect(PUBLISH_OUTPUT in outputs, f"ci.yml: the changes job has no {PUBLISH_OUTPUT} output")
    publish = str((jobs.get("publish-core-container") or {}).get("if", ""))
    checker.expect(
        f"needs.changes.outputs.{PUBLISH_OUTPUT} == 'true'" in publish,
        f"ci.yml: publish-core-container does not publish only on {PUBLISH_OUTPUT}: if: {publish or '(none)'}",
    )
    rust_check = jobs.get("rust-check") or {}
    passed = rust_check.get("with") or {}
    for input_name in CRATE_TEST_INPUTS.values():
        checker.expect(
            f"needs.changes.outputs.{input_name}" in str(passed.get(input_name, "")),
            f"ci.yml: rust-check does not pass the {input_name} area as its {input_name} input",
        )


def check_rust_check_inputs(checker, workflows):
    """Check rust-check.yml's thoradm and thorctl tests follow their inputs.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    workflow = workflows.get("rust-check.yml")
    checker.expect(workflow is not None, "no .github/workflows/rust-check.yml")
    if workflow is None:
        return
    call = triggers(workflow).get("workflow_call") or {}
    inputs = call.get("inputs") or {}
    jobs = workflow.get("jobs") or {}
    for job, input_name in CRATE_TEST_INPUTS.items():
        spec = inputs.get(input_name) or {}
        checker.expect(
            spec.get("type") == "string" and str(spec.get("default")) == "true",
            f"rust-check.yml: no string workflow_call input {input_name} defaulting to 'true'",
        )
        condition = str((jobs.get(job) or {}).get("if", ""))
        checker.expect(
            f"inputs.{input_name} != 'false'" in condition,
            f"rust-check.yml: {job} does not follow its input: if: {condition or '(none)'}",
        )


def check_other_gates(checker, workflows):
    """Check lint and test jobs outside ci.yml are gated by their triggers, and tools.yml's gates.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    for name, workflow in workflows.items():
        if name == "ci.yml":
            continue
        table = CATEGORIES.get(name, {})
        gated = [job for job in workflow.get("jobs") or {} if table.get(job) in GATED_CATEGORIES]
        if not gated:
            continue
        on = triggers(workflow)
        push = on.get("push")
        if push is not None:
            checker.expect(
                "paths" in push or as_list(push.get("branches")) == ["main"],
                f"{name}: runs lint or test jobs ({', '.join(gated)}) on pushes to any branch with no paths: filter",
            )
        for trigger in ("pull_request", "pull_request_target"):
            if trigger in on:
                checker.expect(
                    "paths" in on[trigger],
                    f"{name}: runs lint or test jobs ({', '.join(gated)}) on every {trigger} with no paths: filter",
                )
    tools = workflows.get("tools.yml")
    checker.expect(tools is not None, "no .github/workflows/tools.yml")
    if tools is None:
        return
    on = triggers(tools)
    # only a tools.yml with the toolbox check runs on pull requests, and then for every path in
    # TOOLS_PR_PATHS
    if "check-toolbox" in (tools.get("jobs") or {}):
        paths = as_list((on.get("pull_request") or {}).get("paths"))
        for path in TOOLS_PR_PATHS:
            checker.expect(path in paths, f"tools.yml: its pull_request paths do not include {path}")
    checker.expect(
        as_list((on.get("push") or {}).get("branches")) == ["main"],
        "tools.yml: does not build on pushes to main only",
    )
    checker.expect("schedule" in on, "tools.yml: has no schedule")
    checker.expect("workflow_dispatch" in on, "tools.yml: has no workflow_dispatch")
    plan = ((tools.get("jobs") or {}).get("generate-tool-builds") or {}).get("if", "")
    checker.expect("refs/heads/main" in str(plan), "tools.yml: tool images are not built from main only")


def check_macos_binaries(checker, workflows):
    """Check the one macOS build runs beside the other builds and its binaries reach the core image.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    core = jobs.get(CORE_IMAGE_JOB) or {}
    # the core image's artifact downloads, by artifact name or by a pattern merged into one path
    downloads = [
        step.get("with") or {}
        for step in core.get("steps") or []
        if str(step.get("uses", "")).startswith("actions/download-artifact@")
    ]
    dockerfile = checker.root / "Dockerfile"
    copies = dockerfile.read_text(encoding="utf-8") if dockerfile.is_file() else ""
    checker.expect(bool(copies), "no Dockerfile at the repository root")
    # a second job on macOS would ask for another of the scarce macOS runners in the same run
    others = [
        job_id
        for job_id, job in jobs.items()
        if job_id != MACOS_JOB and "macos" in str((job or {}).get("runs-on", "")).lower()
    ]
    checker.expect(not others, f"ci.yml: {others} run on macOS; {MACOS_JOB} must be the only macOS job")
    job = jobs.get(MACOS_JOB)
    checker.expect(job is not None, f"ci.yml: has no {MACOS_JOB} job")
    if job is None:
        return
    # any need would hold the scarce macOS runner request back until that job finishes
    checker.expect(
        not as_list(job.get("needs")),
        f"ci.yml: {MACOS_JOB} needs {as_list(job.get('needs'))}; it must start with the other builds",
    )
    steps = job.get("steps") or []
    build_steps = [step for step in steps if "cargo" in str(step.get("run", "")) and " build" in str(step.get("run", ""))]
    # one cargo run per binary and target, so each binary resolves its own features and the runner
    # compiles one target at a time
    built = []
    for step in build_steps:
        build = " ".join(str(step.get("run", "")).replace("\\\n", " ").split())
        runs = len(re.findall(r"\bcargo\b", build))
        checker.expect(
            runs == 1,
            f"ci.yml: {MACOS_JOB} ({step.get('name')}) runs cargo {runs} times; each binary and target needs its own step",
        )
        packages = re.findall(r"(?:^|\s)(?:-p|--package)[ =](\S+)", build)
        targets = re.findall(r"--target[ =](\S+)", build)
        checker.expect(
            len(packages) == 1 and packages[0] in MACOS_BINARIES,
            f"ci.yml: {MACOS_JOB} ({step.get('name')}) builds {packages}; each cargo run must build one of {MACOS_BINARIES}",
        )
        checker.expect(
            len(targets) == 1,
            f"ci.yml: {MACOS_JOB} ({step.get('name')}) builds {len(targets)} targets in one cargo run; it needs one",
        )
        built += [(package, target) for package in packages for target in targets]
        build_env = {**(job.get("env") or {}), **(step.get("env") or {})}
        for name, value in MACOS_BUILD_ENV.items():
            checker.expect(
                str(build_env.get(name)) == value,
                f"ci.yml: {MACOS_JOB}'s cargo build ({step.get('name')}) does not set {name}={value}",
            )
    expected = sorted((binary, target) for binary in MACOS_BINARIES for target in MACOS_TARGETS)
    checker.expect(
        sorted(built) == expected,
        f"ci.yml: {MACOS_JOB} builds {sorted(built)}; it must build each of {MACOS_BINARIES} for each of {MACOS_TARGETS} once",
    )
    uploads = {
        str((step.get("with") or {}).get("name")): str((step.get("with") or {}).get("path", ""))
        for step in steps
        if str(step.get("uses", "")).startswith("actions/upload-artifact@")
    }
    checker.expect(MACOS_JOB in as_list(core.get("needs")), f"ci.yml: {CORE_IMAGE_JOB} does not need {MACOS_JOB}")
    for binary in MACOS_BINARIES:
        for target in MACOS_TARGETS:
            artifact = f"{target}-{binary}"
            built_path = f"target/{target}/release/{binary}"
            checker.expect(artifact in uploads, f"ci.yml: {MACOS_JOB} uploads no {artifact} artifact")
            checker.expect(
                built_path in uploads.get(artifact, "").split(),
                f"ci.yml: {MACOS_JOB}'s {artifact} artifact does not hold {built_path}",
            )
            # a pattern download needs merge-multiple, or each artifact lands in its own directory
            # and the staging step's move into target/<triple>/release misses the binaries
            fetched = any(
                str(spec.get("name")) == artifact
                or (
                    spec.get("pattern") is not None
                    and fnmatch.fnmatchcase(artifact, str(spec.get("pattern")))
                    and spec.get("merge-multiple") is True
                )
                for spec in downloads
            )
            checker.expect(fetched, f"ci.yml: {CORE_IMAGE_JOB} does not download {artifact} (by name, or by a merged pattern)")
            checker.expect(
                re.search(rf"^COPY \./{re.escape(built_path)} binaries/darwin/\S+/{binary}$", copies, re.M) is not None,
                f"Dockerfile: does not copy {built_path} into the image",
            )


def check_musl_binaries(checker, workflows):
    """Check each Linux musl build is a plain job beside the other builds whose binaries reach the core image.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    core = jobs.get(CORE_IMAGE_JOB) or {}
    core_steps = core.get("steps") or []
    downloads = {
        str((step.get("with") or {}).get("name")): str((step.get("with") or {}).get("path", ""))
        for step in core_steps
        if str(step.get("uses", "")).startswith("actions/download-artifact@")
    }
    core_runs = "\n".join(str(step.get("run", "")) for step in core_steps)
    dockerfile = checker.root / "Dockerfile"
    copies = dockerfile.read_text(encoding="utf-8") if dockerfile.is_file() else ""
    for job_id, (target, build_vars, directory) in MUSL_JOBS.items():
        job = jobs.get(job_id)
        checker.expect(job is not None, f"ci.yml: has no {job_id} job")
        if job is None:
            continue
        # a matrix job's name and artifacts come from its matrix, which the checks below can't follow
        checker.expect("strategy" not in job, f"ci.yml: {job_id} has a strategy; it must be a plain job")
        checker.expect("${{" not in str(job.get("name", "${{")), f"ci.yml: {job_id} needs a fixed name")
        checker.expect(
            not as_list(job.get("needs")),
            f"ci.yml: {job_id} needs {as_list(job.get('needs'))}; it must start with the other builds",
        )
        steps = job.get("steps") or []
        build_steps = [step for step in steps if str(step.get("uses", "")).startswith(RUST_CROSS_ACTION)]
        checker.expect(
            len(build_steps) == 1,
            f"ci.yml: {job_id} has {len(build_steps)} {RUST_CROSS_ACTION} steps; it needs exactly one",
        )
        build = (build_steps[0].get("with") or {}) if build_steps else {}
        build_env = {**(job.get("env") or {}), **((build_steps[0].get("env") or {}) if build_steps else {})}
        checker.expect(str(build.get("target")) == target, f"ci.yml: {job_id} does not build for {target}")
        if target in MUSL_FORCED_CROSS_TARGETS:
            checker.expect(
                str(build.get("force-use-cross")).lower() == "true",
                f"ci.yml: {job_id} does not set force-use-cross, so it does not build {target} in cross",
            )
        for name, value in build_vars.items():
            checker.expect(
                str(build_env.get(name)) == value, f"ci.yml: {job_id}'s build does not set {name} to {value!r}"
            )
        args = f" {build.get('args', '')} "
        checker.expect(
            "--features vendored-openssl" in args, f"ci.yml: {job_id} does not build with vendored-openssl"
        )
        uploads = {
            str((step.get("with") or {}).get("name")): str((step.get("with") or {}).get("path", ""))
            for step in steps
            if str(step.get("uses", "")).startswith("actions/upload-artifact@")
        }
        artifact = f"{target}-binaries"
        checker.expect(artifact in uploads, f"ci.yml: {job_id} uploads no {artifact} artifact")
        checker.expect(job_id in as_list(core.get("needs")), f"ci.yml: {CORE_IMAGE_JOB} does not need {job_id}")
        checker.expect(artifact in downloads, f"ci.yml: {CORE_IMAGE_JOB} does not download {artifact}")
        staged = re.search(
            rf"^\s*mv {re.escape(downloads.get(artifact, '-'))}/\* target/{re.escape(target)}/release/\.?$",
            core_runs,
            re.M,
        )
        checker.expect(
            staged is not None, f"ci.yml: {CORE_IMAGE_JOB} does not move {artifact} into target/{target}/release"
        )
        for binary in MUSL_BINARIES:
            checker.expect(f" -p {binary} " in args, f"ci.yml: {job_id} does not build {binary} (-p {binary})")
            built = f"target/{target}/release/{binary}"
            checker.expect(
                built in uploads.get(artifact, "").split(),
                f"ci.yml: {job_id}'s {artifact} artifact does not hold {built}",
            )
            checker.expect(
                re.search(rf"^COPY \./{re.escape(built)} binaries/linux/{directory}/{re.escape(binary)}$", copies, re.M)
                is not None,
                f"Dockerfile: does not copy {built} into binaries/linux/{directory}",
            )


def dockerfile_stages(text):
    """Return a Dockerfile's stages as (FROM flags and image, alias, instruction keywords) tuples.

    # Arguments

    * `text` - The Dockerfile's contents
    """
    stages = []
    # join continued lines, then read one instruction per line, skipping comments
    for line in re.sub(r"\\\n", " ", text).splitlines():
        words = line.split()
        if not words or words[0].startswith("#"):
            continue
        keyword = words[0].upper()
        if keyword == "FROM":
            alias = words[-1] if len(words) >= 3 and words[-2].upper() == "AS" else None
            stages.append((" ".join(words[1:-2] if alias else words[1:]), alias, []))
        elif stages:
            stages[-1][2].append(keyword)
    return stages


def check_multiarch_image(checker, workflows):
    """Check the core image builds for every architecture of IMAGE_ARCHES without emulation, from
    that architecture's server binaries, and is checked before it is saved.

    In the Dockerfile, only stages on the build platform (`--platform=$BUILDPLATFORM`) and the
    final stage of a native architecture (`NATIVE_IMAGE_ARCHES`) run steps; the server and final
    stages are picked by TARGETARCH; each architecture's crane checksum is pinned; and each
    server-<arch> stage copies every server binary from the architecture's directory. In ci.yml,
    the job building those binaries uploads them, and the core image job needs it, stages the
    artifact in that directory, builds and checks each architecture's image under its
    image-<arch> output, and saves each.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    dockerfile = checker.root / "Dockerfile"
    text = dockerfile.read_text(encoding="utf-8") if dockerfile.is_file() else ""
    stages = dockerfile_stages(text)
    for image, alias, keywords in stages:
        native = alias in {f"final-{arch}" for arch in NATIVE_IMAGE_ARCHES}
        checker.expect(
            "RUN" not in keywords or "--platform=$BUILDPLATFORM" in image or native,
            f"Dockerfile: stage {alias or image} runs steps on the target platform, which needs emulation for a foreign architecture",
        )
    targets = {image: alias for image, alias, _ in stages}
    checker.expect("server-${TARGETARCH}" in targets, "Dockerfile: no stage is FROM server-${TARGETARCH}")
    checker.expect(bool(stages) and stages[-1][0] == "final-${TARGETARCH}", "Dockerfile: the last stage is not FROM final-${TARGETARCH}")
    aliases = {alias for _, alias, _ in stages}
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    core = jobs.get(CORE_IMAGE_JOB) or {}
    core_steps = core.get("steps") or []
    downloads = {
        str((step.get("with") or {}).get("name")): str((step.get("with") or {}).get("path", ""))
        for step in core_steps
        if str(step.get("uses", "")).startswith("actions/download-artifact@")
    }
    core_runs = [(step.get("env") or {}, str(step.get("run", ""))) for step in core_steps]
    all_runs = "\n".join(run for _, run in core_runs)
    for arch, (directory, job_id, artifact) in IMAGE_ARCHES.items():
        for stage in (f"server-{arch}", f"final-{arch}"):
            checker.expect(stage in aliases, f"Dockerfile: has no {stage} stage")
        checker.expect(
            re.search(rf"^ARG CRANE_SHA256_{arch.upper()}=[0-9a-f]{{64}}$", text, re.M) is not None,
            f"Dockerfile: CRANE_SHA256_{arch.upper()} is not pinned to a sha256",
        )
        for binary in SERVER_BINARIES:
            checker.expect(
                re.search(rf"^COPY \./{re.escape(directory)}/{re.escape(binary)} /{re.escape(binary)}$", text, re.M) is not None,
                f"Dockerfile: server-{arch} does not copy {directory}/{binary}",
            )
        job = jobs.get(job_id) or {}
        uploads = "\n".join(
            str((step.get("with") or {}).get("name"))
            for step in job.get("steps") or []
            if str(step.get("uses", "")).startswith("actions/upload-artifact@")
        )
        checker.expect(artifact in uploads.split(), f"ci.yml: {job_id} uploads no {artifact} artifact")
        checker.expect(job_id in as_list(core.get("needs")), f"ci.yml: {CORE_IMAGE_JOB} does not need {job_id}")
        checker.expect(
            re.search(rf"^\s*mv {re.escape(downloads.get(artifact, '-'))}/\* {re.escape(directory)}/\.?$", all_runs, re.M) is not None,
            f"ci.yml: {CORE_IMAGE_JOB} does not move {artifact} into {directory}",
        )
        variable = f"IMAGE_{arch.upper()}"
        output = f"${{{{ steps.image.outputs.image-{arch} }}}}"
        built = [run for env, run in core_runs if env.get(variable) == output and re.search(rf"docker build --platform linux/{arch} .*--tag \"\${variable}\"", run)]
        checker.expect(bool(built), f"ci.yml: {CORE_IMAGE_JOB} does not build linux/{arch} under its image-{arch} output")
        checked = [run for env, run in core_runs if env.get(variable) == output and f'check_image_arch.sh "${variable}" {arch}' in run]
        checker.expect(bool(checked), f"ci.yml: {CORE_IMAGE_JOB} does not check the {arch} image's architecture with check_image_arch.sh")
        checker.expect(
            f'thorium-image-{arch}.tar" "${variable}"' in all_runs,
            f"ci.yml: {CORE_IMAGE_JOB} does not save the {arch} image as thorium-image-{arch}.tar",
        )


def runner_target(runs_on):
    """Return the Rust target of a GitHub-hosted runner, which a build without --target builds for.

    # Arguments

    * `runs_on` - The job's `runs-on` value
    """
    label = " ".join(as_list(runs_on)).lower()
    if "macos" in label:
        # macos-13 is the last Intel image; macos-latest and later images run on Apple silicon
        return "x86_64-apple-darwin" if re.search(r"macos-1[0-3]\b|intel", label) else "aarch64-apple-darwin"
    if "windows" in label:
        return "x86_64-pc-windows-msvc"
    return "aarch64-unknown-linux-gnu" if "arm" in label else "x86_64-unknown-linux-gnu"


def rustflags_problem(target, flags):
    """Return why a target's RUSTFLAGS can't build gxhash or don't match its architecture, or None.

    # Arguments

    * `target` - The Rust target being built
    * `flags` - The RUSTFLAGS the target builds with, or None when nothing sets them
    """
    # a GitHub expression is only known at run time, so its value can't be checked here
    if "${{" in target or "${{" in (flags or ""):
        return f"builds {target} with RUSTFLAGS {flags!r}, which a GitHub expression sets and so can't be checked"
    arch = target.split("-")[0]
    features = set(re.findall(r"\+([\w.-]+)", flags or "")) | DEFAULT_TARGET_FEATURES.get(target, set())
    expected = RUSTFLAGS_BY_ARCH.get(arch)
    # every x86_64 target, macOS included, and every aarch64 target off macOS names exactly its flags
    if arch == "x86_64" or "apple-darwin" not in target:
        if expected is not None and flags != expected:
            return f"builds {target} with RUSTFLAGS {flags!r}, not {expected!r}"
        return None
    if "aes" not in features:
        return f"builds {target} without AES, which gxhash needs"
    if arch == "aarch64" and "sse2" in features:
        return f"builds {target} with +sse2, an x86_64 feature"
    return None


def check_rustflags(checker, workflows):
    """Check every step that compiles Rust builds each of its targets with its architecture's gxhash
    features, that aarch64 never takes a workflow-wide RUSTFLAGS, that the arm64 Linux and macOS
    targets link liblzma statically, and that ci.yml builds every target of RUSTFLAGS_TARGETS.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    built = set()
    for name, workflow in workflows.items():
        for job_id, job in (workflow.get("jobs") or {}).items():
            job = job or {}
            for index, step in enumerate(job.get("steps") or []):
                run = str(step.get("run", "")).replace("\\\n", " ")
                uses = str(step.get("uses", ""))
                if not (RUST_COMPILE_RE.search(run) or uses.startswith(RUST_CROSS_ACTION)):
                    continue
                # cargo reads RUSTFLAGS before a target's own CARGO_TARGET_<TRIPLE>_RUSTFLAGS
                workflow_env = workflow.get("env") or {}
                env = {**workflow_env, **(job.get("env") or {}), **(step.get("env") or {})}
                targets = re.findall(r"--target[ =](\$\{\{.*?\}\}|[\w.-]+)", run)
                if uses.startswith(RUST_CROSS_ACTION):
                    targets = [str((step.get("with") or {}).get("target", ""))]
                step_name = step.get("name") or f"step {index + 1}"
                for target in targets or [runner_target(job.get("runs-on"))]:
                    flags = env.get("RUSTFLAGS", env.get(f"CARGO_TARGET_{target.upper().replace('-', '_')}_RUSTFLAGS"))
                    problem = rustflags_problem(target, None if flags is None else str(flags))
                    checker.expect(problem is None, f"{name}: {job_id} ({step_name}) {problem}")
                    if name == "ci.yml":
                        built.add(target)
                    # a workflow-wide RUSTFLAGS is set for its x86_64 jobs, so aarch64 sets its own closer in
                    inherited = "RUSTFLAGS" in workflow_env and env.get("RUSTFLAGS") == workflow_env["RUSTFLAGS"]
                    checker.expect(
                        not (target.startswith("aarch64-") and inherited),
                        f"{name}: {job_id} ({step_name}) builds {target} with the workflow-wide RUSTFLAGS",
                    )
                    if target in STATIC_LZMA_TARGETS:
                        checker.expect(
                            str(env.get("LZMA_API_STATIC", "")) == "1",
                            f"{name}: {job_id} ({step_name}) builds {target} without LZMA_API_STATIC=1",
                        )
    # every target the table names is built, so a dropped build can't leave its flags unchecked
    for target in RUSTFLAGS_TARGETS:
        checker.expect(target in built, f"ci.yml: no step builds {target}, so its RUSTFLAGS go unchecked")


def runs_scan(job):
    """Return whether a job runs a Trivy image or filesystem scan.

    # Arguments

    * `job` - The parsed job
    """
    for step in job.get("steps") or []:
        # a shell command continued with a backslash is read as one line
        run = str(step.get("run", "")).replace("\\\n", " ")
        if TRIVY_SCAN.search(run):
            return True
        # trivy-action scans an image unless told another scan type
        if str(step.get("uses", "")).startswith("aquasecurity/trivy-action@"):
            scan_type = str((step.get("with") or {}).get("scan-type", "image"))
            if scan_type in ("image", "fs", "filesystem", "rootfs"):
                return True
    return False


def needed_jobs(spec):
    """Return the jobs a job needs, by its `needs:` and by the `needs.<job>` its `if:` reads.

    # Arguments

    * `spec` - The parsed job
    """
    read = re.findall(r"needs\.([A-Za-z0-9_-]+)", str(spec.get("if", "")))
    return list(dict.fromkeys([str(job) for job in as_list(spec.get("needs"))] + read))


def check_scans_grouped(checker, workflows):
    """Check every scan sits in the security group, with ci.yml only calling the security workflows.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    for job, spec in jobs.items():
        if CATEGORIES["ci.yml"].get(job) != "security":
            continue
        uses = str((spec or {}).get("uses", ""))
        checker.expect(
            any(uses.endswith(f"/{called}") for called in SECURITY_CALLS),
            f"ci.yml: security job {job} runs its own scan instead of calling one of {', '.join(SECURITY_CALLS)}",
        )
    for name, workflow in workflows.items():
        if name == SECURITY_WORKFLOW:
            continue
        exempt = SCAN_EXEMPTIONS.get(name, set())
        for job, spec in (workflow.get("jobs") or {}).items():
            if job in exempt:
                continue
            checker.expect(
                not runs_scan(spec or {}),
                f"{name}: job {job} runs a Trivy image or filesystem scan; scans belong in {SECURITY_WORKFLOW}",
            )


def check_publish_independent(checker, workflows):
    """Check no publish job, nor any job it needs directly or through others, needs a check.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    table = CATEGORIES["ci.yml"]
    # the scans, lints, and tests, including the jobs calling the security and lint workflows
    checks = GATED_CATEGORIES | {"security"}
    for root in PUBLISH_ROOTS:
        checker.expect(root in jobs, f"ci.yml: has no {root} job")
    # walk every job the publish jobs need, through each job's own needs
    seen = set()
    pending = list(PUBLISH_ROOTS)
    while pending:
        job = pending.pop()
        if job in seen or job not in jobs:
            continue
        seen.add(job)
        for other in needed_jobs(jobs[job] or {}):
            checker.expect(
                table.get(other) not in checks,
                f"ci.yml: {job} (a publish job or a job one needs) needs the {table.get(other)} job {other}; "
                "no publish may wait for a scan, lint, or test",
            )
            pending.append(other)


def check_publish_order(checker, workflows):
    """Check the publishes need the image build and the chart package, and the chart publish the image's.

    The chart package needs nothing, so it starts with the builds.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    for job in PUBLISH_JOBS:
        needed = as_list((jobs.get(job) or {}).get("needs"))
        for need in (CORE_IMAGE_JOB, HELM_PACKAGE_JOB):
            checker.expect(
                need in needed,
                f"ci.yml: {job} does not need {need}; it publishes after the image build and the chart package",
            )
    # the charts are pushed after the image they deploy, on runs that publish one
    chart_needs = as_list((jobs.get(HELM_PUBLISH_JOB) or {}).get("needs"))
    checker.expect(
        IMAGE_PUBLISH_JOB in chart_needs,
        f"ci.yml: {HELM_PUBLISH_JOB} does not need {IMAGE_PUBLISH_JOB}; a chart must not be pushed before its image",
    )
    package = jobs.get(HELM_PACKAGE_JOB)
    checker.expect(package is not None, f"ci.yml: has no {HELM_PACKAGE_JOB} job")
    if package is not None:
        checker.expect(
            not as_list(package.get("needs")),
            f"ci.yml: {HELM_PACKAGE_JOB} needs {as_list(package.get('needs'))}; it must start with the builds",
        )


def load_release_refs(checker):
    """Import the checked checkout's .github/scripts/release_refs.py as a module, or None.

    # Arguments

    * `checker` - The checker to record a missing or broken script on
    """
    path = checker.root / ".github" / "scripts" / "release_refs.py"
    checker.expect(path.exists(), "no .github/scripts/release_refs.py")
    if not path.exists():
        return None
    # load it under its own name so its dataclass resolves, without touching sys.path
    spec = importlib.util.spec_from_file_location("release_refs", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules["release_refs"] = module
    try:
        spec.loader.exec_module(module)
    except Exception as error:  # noqa: BLE001 - any import failure is a broken rule
        checker.expect(False, f"release_refs.py does not import: {error}")
        return None
    finally:
        sys.modules.pop("release_refs", None)
    return module


def release_run(module, ref_name, ref_type="branch", is_fork=False):
    """Return a release_refs Run for a push to the core repo or a fork.

    # Arguments

    * `module` - The imported release_refs module
    * `ref_name` - The branch or tag name
    * `ref_type` - `branch` or `tag`
    * `is_fork` - Whether the run is in a fork
    """
    # the core repo's default branch is main, as on cisagov/thorium
    return module.Run(
        event="push",
        ref_type=ref_type,
        ref_name=ref_name,
        repository="cisagov/thorium",
        default_branch="main",
        is_fork=is_fork,
        run_number="7",
        sha="0123456789abcdef0123456789abcdef01234567",
    )


def check_tag_publish(checker, workflows):
    """Check a release tag publishes as the tag itself and never moves `latest` or a branch tag.

    release_refs.py runs with its git lookup stubbed to TAG_HIGHEST_RELEASE, for each tag of
    TAG_RUNS and for default-branch and feature-branch pushes; ci.yml is read for the jobs that
    use its outputs.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    module = load_release_refs(checker)
    if module is not None:
        # every tag run would be the highest release, so only the ref type keeps latest still
        module.highest_release = lambda ref: TAG_HIGHEST_RELEASE
        repository = "ghcr.io/cisagov/thorium/infrastructure/thorium"
        with tempfile.TemporaryDirectory() as scratch:
            chart = Path(scratch) / "Chart.yaml"
            chart.write_text(f"apiVersion: v2\nname: thorium\nversion: {TAG_CHART_VERSION}\n", encoding="utf-8")
            for tag, chart_version in TAG_RUNS.items():
                for is_fork in (False, True):
                    where = "a fork" if is_fork else "the core repo"
                    run = release_run(module, tag, "tag", is_fork)
                    try:
                        image = module.image_outputs(run)
                        charts = module.chart_outputs(run, [str(chart)])
                    except Exception as error:  # noqa: BLE001 - a refused release tag is a broken rule
                        checker.expect(False, f"release_refs.py: tag {tag} in {where} is refused: {error}")
                        continue
                    checker.expect(image["image"] == f"{repository}:{tag}", f"release_refs.py: tag {tag} in {where} publishes {image['image']}, not {repository}:{tag}")
                    for arch in IMAGE_ARCHES:
                        checker.expect(
                            image.get(f"image-{arch}") == f"{repository}:{tag}-{arch}",
                            f"release_refs.py: tag {tag} in {where} pushes its {arch} image as {image.get(f'image-{arch}')}, not {repository}:{tag}-{arch}",
                        )
                    checker.expect(image["publish"] == "true", f"release_refs.py: tag {tag} in {where} does not publish its image")
                    checker.expect(image["latest-source"] == "", f"release_refs.py: tag {tag} in {where} moves latest to {image['latest-source']}")
                    checker.expect(charts["version"] == chart_version, f"release_refs.py: tag {tag} in {where} packages the charts as {charts['version']}, not {chart_version}")
                    checker.expect(charts["image-tag"] == tag, f"release_refs.py: tag {tag} in {where} charts deploy image tag {charts['image-tag']}, not {tag}")
                    checker.expect(charts["publish"] == "true", f"release_refs.py: tag {tag} in {where} does not publish its charts")
            # a tag that is not a release builds and packages, but publishes nothing and says why
            for tag in NON_RELEASE_TAGS:
                for is_fork in (False, True):
                    where = "a fork" if is_fork else "the core repo"
                    run = release_run(module, tag, "tag", is_fork)
                    try:
                        image = module.image_outputs(run)
                        charts = module.chart_outputs(run, [str(chart)])
                        problem = module.tag_problem(run) if hasattr(module, "tag_problem") else None
                    except Exception as error:  # noqa: BLE001 - a failed build is a broken rule
                        checker.expect(False, f"release_refs.py: tag {tag} in {where} fails instead of building unpublished: {error}")
                        continue
                    built = image["image"].rsplit(":", 1)[1]
                    checker.expect(image["publish"] == "false", f"release_refs.py: tag {tag} in {where} publishes its image though it is not SemVer")
                    checker.expect(charts["publish"] == "false", f"release_refs.py: tag {tag} in {where} publishes its charts though it is not SemVer")
                    checker.expect(built not in (tag, "latest", "main", TAG_HIGHEST_RELEASE), f"release_refs.py: tag {tag} in {where} builds its image as {built}")
                    checker.expect(image["latest-source"] == "", f"release_refs.py: tag {tag} in {where} moves latest")
                    checker.expect(bool(problem), f"release_refs.py: tag {tag} in {where} publishes nothing without saying why (tag_problem)")
        # the default branch keeps pointing latest at the highest release; other branches never do,
        # and no branch publishes under latest or a release's tag
        main = module.image_outputs(release_run(module, "main"))
        checker.expect(main["latest-source"] == f"{repository}:{TAG_HIGHEST_RELEASE}", f"release_refs.py: the default branch points latest at {main['latest-source']!r}")
        for branch in ("feature", "latest", TAG_HIGHEST_RELEASE):
            outputs = module.image_outputs(release_run(module, branch, is_fork=True))
            tag = outputs["image"].rsplit(":", 1)[1]
            checker.expect(outputs["latest-source"] == "", f"release_refs.py: branch {branch} moves latest")
            checker.expect(tag not in ("latest", TAG_HIGHEST_RELEASE), f"release_refs.py: branch {branch} publishes as {tag}")
    ci = workflows.get("ci.yml") or {}
    tag_filters = as_list((triggers(ci).get("push") or {}).get("tags"))
    for pattern in RELEASE_TAG_FILTERS:
        checker.expect(pattern in tag_filters, f"ci.yml: push does not run on release tags matching '{pattern}'")
    jobs = ci.get("jobs") or {}
    steps = (jobs.get(IMAGE_PUBLISH_JOB) or {}).get("steps") or []
    # each architecture's image is pushed under its own output, and only the build job's image
    # output is written as their index
    arch_env = {f"IMAGE_{arch.upper()}": f"${{{{ needs.build-core-container.outputs.image-{arch} }}}}" for arch in IMAGE_ARCHES}
    pushes = [step for step in steps if "docker push" in str(step.get("run", ""))]
    pushed = set()
    for step in pushes:
        env = step.get("env") or {}
        run = str(step.get("run", "")).strip()
        arch = next((arch for arch in IMAGE_ARCHES if env == {f"IMAGE_{arch.upper()}": arch_env[f"IMAGE_{arch.upper()}"]} and run == f'docker push "$IMAGE_{arch.upper()}"'), None)
        checker.expect(arch is not None, f"ci.yml: {IMAGE_PUBLISH_JOB} step '{step.get('name')}' pushes more than one of the build job's image-<arch> outputs")
        pushed.add(arch)
    for arch in IMAGE_ARCHES:
        checker.expect(arch in pushed, f"ci.yml: {IMAGE_PUBLISH_JOB} has no step pushing the build job's image-{arch} output")
    joins = [step for step in steps if "imagetools create" in str(step.get("run", "")) and "latest" not in str(step.get("env") or "")]
    checker.expect(len(joins) == 1, f"ci.yml: {IMAGE_PUBLISH_JOB} has {len(joins)} steps joining the architecture images; it needs exactly one")
    sources = " ".join(f'"$IMAGE_{arch.upper()}"' for arch in IMAGE_ARCHES)
    for step in joins:
        checker.expect(
            step.get("env") == {"IMAGE": "${{ needs.build-core-container.outputs.image }}", **arch_env}
            and str(step.get("run", "")).strip() == f'docker buildx imagetools create --tag "$IMAGE" {sources}',
            f"ci.yml: {IMAGE_PUBLISH_JOB} step '{step.get('name')}' writes more than the build job's image output from its image-<arch> outputs",
        )
        checker.expect(
            all(steps.index(push) < steps.index(step) for push in pushes),
            f"ci.yml: {IMAGE_PUBLISH_JOB} step '{step.get('name')}' joins the architecture images before both are pushed",
        )
    movers = [step for step in steps if "latest" in str(step.get("env") or "")]
    for step in movers:
        checker.expect(
            "github.ref_type == 'branch'" in str(step.get("if", "")),
            f"ci.yml: {IMAGE_PUBLISH_JOB} step '{step.get('name')}' could move latest on a tag run; its if: must require github.ref_type == 'branch'",
        )
    package = "\n".join(str(step.get("env") or "") for step in (jobs.get(HELM_PACKAGE_JOB) or {}).get("steps") or [])
    for output in ("version", "image-tag", "image-repository"):
        checker.expect(
            f"steps.version.outputs.{output}" in package,
            f"ci.yml: {HELM_PACKAGE_JOB} does not package with release_refs.py's {output} output",
        )


def check_scans_always_run(checker, workflows):
    """Check the security scans run on every run, wait only for the image build, and nothing waits for them.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    caller = jobs.get(SECURITY_CALLER) or {}
    checker.expect(
        str(caller.get("uses", "")).endswith(f"/{SECURITY_WORKFLOW}"),
        f"ci.yml: job {SECURITY_CALLER} does not call {SECURITY_WORKFLOW}",
    )
    condition = str(caller.get("if", ""))
    needed = as_list(caller.get("needs"))
    checker.expect(
        "!cancelled()" in condition,
        f"ci.yml: {SECURITY_CALLER} does not run whatever the build concluded: if: {condition or '(none)'}",
    )
    checker.expect(
        "changes" not in needed and "needs.changes" not in yaml.safe_dump(caller, width=1000),
        f"ci.yml: {SECURITY_CALLER} reads the changes job; the scans run on every run",
    )
    checker.expect(
        CORE_IMAGE_JOB in needed, f"ci.yml: {SECURITY_CALLER} does not need {CORE_IMAGE_JOB}, whose image it scans"
    )
    publishing = [job for job in needed if job in PUBLISH_ROOTS]
    checker.expect(
        not publishing, f"ci.yml: {SECURITY_CALLER} needs the publish job(s) {publishing}; it runs beside the publish"
    )
    passed = str((caller.get("with") or {}).get(IMAGE_INPUTS[0], ""))
    checker.expect(
        f"needs.{CORE_IMAGE_JOB}.outputs.saved" in passed,
        f"ci.yml: {SECURITY_CALLER} does not pass the saved image's artifact as {IMAGE_INPUTS[0]}",
    )
    for job, spec in jobs.items():
        if job == SECURITY_CALLER:
            continue
        text = yaml.safe_dump(spec or {}, width=1000)
        checker.expect(
            SECURITY_CALLER not in as_list((spec or {}).get("needs")) and f"needs.{SECURITY_CALLER}." not in text,
            f"ci.yml: {job} waits for the security scans ({SECURITY_CALLER}); nothing may",
        )
    scan_jobs = (workflows.get(SECURITY_WORKFLOW) or {}).get("jobs") or {}
    for scan in REPO_SCANS:
        spec = scan_jobs.get(scan)
        checker.expect(spec is not None, f"{SECURITY_WORKFLOW}: has no {scan} job")
        if spec is None:
            continue
        checker.expect(
            not as_list(spec.get("needs")),
            f"{SECURITY_WORKFLOW}: {scan} needs {as_list(spec.get('needs'))}; the repo scans wait for nothing",
        )
        checker.expect(
            "inputs." not in str(spec.get("if", "")),
            f"{SECURITY_WORKFLOW}: {scan} depends on an input: if: {spec.get('if')}",
        )


def check_published_rescan(checker, workflows):
    """Check security-scan.yml scans the build's image when called and the published images on its own.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    for retired in RETIRED_WORKFLOWS:
        checker.expect(
            retired not in workflows, f"{retired}: still exists; {SECURITY_WORKFLOW} rescans the published images"
        )
    workflow = workflows.get(SECURITY_WORKFLOW)
    checker.expect(workflow is not None, f"no .github/workflows/{SECURITY_WORKFLOW}")
    if workflow is None:
        return
    on = triggers(workflow)
    for trigger in ("schedule", "workflow_dispatch"):
        checker.expect(trigger in on, f"{SECURITY_WORKFLOW}: has no {trigger} trigger to rescan the published images")
    inputs = (on.get("workflow_call") or {}).get("inputs") or {}
    for name in IMAGE_INPUTS:
        checker.expect(
            (inputs.get(name) or {}).get("type") == "string",
            f"{SECURITY_WORKFLOW}: no string workflow_call input {name}",
        )
    jobs = workflow.get("jobs") or {}
    build = jobs.get(BUILD_IMAGE_SCAN) or {}
    condition = str(build.get("if", ""))
    checker.expect(
        runs_scan(build) and f"!{STANDALONE}" in condition and f"inputs.{IMAGE_INPUTS[0]}" in condition,
        f"{SECURITY_WORKFLOW}: {BUILD_IMAGE_SCAN} does not scan the build's image only when called with one: "
        f"if: {condition or '(none)'}",
    )
    published = jobs.get(PUBLISHED_IMAGE_SCAN) or {}
    condition = str(published.get("if", ""))
    checker.expect(
        runs_scan(published) and STANDALONE in condition and f"!{STANDALONE}" not in condition,
        f"{SECURITY_WORKFLOW}: {PUBLISHED_IMAGE_SCAN} does not scan the published images on its own runs: "
        f"if: {condition or '(none)'}",
    )
    # the code scanning category each upload step sends, and the job sending it
    uploads = {}
    for job, spec in jobs.items():
        for step in (spec or {}).get("steps") or []:
            if str(step.get("uses", "")).startswith("github/codeql-action/upload-sarif@"):
                uploads[str((step.get("with") or {}).get("category"))] = job
    for category in IMAGE_SCAN_CATEGORIES:
        checker.expect(category in uploads, f"{SECURITY_WORKFLOW}: uploads no {category} code scanning category")
    build_upload = str((jobs.get(uploads.get(IMAGE_SCAN_CATEGORIES[0])) or {}).get("if", ""))
    checker.expect(
        "github.event_name != 'pull_request'" in build_upload,
        f"{SECURITY_WORKFLOW}: the {IMAGE_SCAN_CATEGORIES[0]} upload is not for pushes only: "
        f"if: {build_upload or '(none)'}",
    )
    for job, spec in jobs.items():
        permissions = (spec or {}).get("permissions") or {}
        if not isinstance(permissions, dict) or permissions.get("security-events") != "write":
            continue
        runs = [str(step.get("run", "")) for step in spec.get("steps") or []]
        scanner = [run for run in runs if "docker run" in run]
        checker.expect(
            not scanner and not runs_scan(spec),
            f"{SECURITY_WORKFLOW}: {job} holds security-events: write and runs scanner code",
        )


def commands_of(script):
    """Return the command name of every simple command in a shell script, roughly.

    The script is split at newlines and shell separators, quoted strings are dropped, and the first
    word that is not a variable assignment or a keyword such as `if` is taken as the command.

    # Arguments

    * `script` - The `run:` script
    """
    commands = []
    # quoted text holds no command names, only arguments and messages
    script = re.sub(r"'[^']*'|\"(?:[^\"\\]|\\.)*\"", '""', script.replace("\\\n", " "))
    for line in script.splitlines():
        for part in COMMAND_SEPARATOR_RE.split(line.split(" #")[0]):
            words = [word for word in part.split() if not re.fullmatch(r"\w+=\S*", word)]
            start = next((index for index, word in enumerate(words) if word not in COMMAND_PREFIXES), len(words))
            words = words[start:]
            if words and not words[0].startswith(("#", '""', ">", "<", "[", "]", "}")):
                commands.append(words[0])
    return commands


def job_steps(workflows):
    """Yield every step of every job that runs steps, with its workflow, job id, and job.

    # Arguments

    * `workflows` - The parsed workflows by file name
    """
    for name, workflow in workflows.items():
        for job_id, job in (workflow.get("jobs") or {}).items():
            for step in (job or {}).get("steps") or []:
                yield name, job_id, job, step or {}


def check_workflow_lint(checker, workflows):
    """Check workflow-lint.yml runs every workflow lint in one read-only job that reports each.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    jobs = (workflows.get("workflow-lint.yml") or {}).get("jobs") or {}
    job = jobs.get(WORKFLOW_LINT_JOB)
    checker.expect(job is not None, f"workflow-lint.yml: has no {WORKFLOW_LINT_JOB} job")
    if job is None:
        return
    # the lint job's token reads the repository and nothing more
    permissions = job.get("permissions") or {}
    checker.expect(
        isinstance(permissions, dict) and all(value in ("read", "none") for value in permissions.values()),
        f"workflow-lint.yml: {WORKFLOW_LINT_JOB} has write permissions {permissions}; it must be read-only",
    )
    steps = job.get("steps") or []
    for tool, pattern in WORKFLOW_LINT_TOOLS.items():
        # the tool runs in this job and in no other job of the workflow
        runs = [step for step in steps if re.search(pattern, str(step.get("run", "")))]
        checker.expect(
            len(runs) == 1, f"workflow-lint.yml: {WORKFLOW_LINT_JOB} runs {tool} in {len(runs)} steps; it needs one"
        )
        elsewhere = [
            other
            for other, body in jobs.items()
            if other != WORKFLOW_LINT_JOB
            and any(re.search(pattern, str(step.get("run", ""))) for step in (body or {}).get("steps") or [])
        ]
        checker.expect(
            not elsewhere, f"workflow-lint.yml: {tool} also runs in {elsewhere}; it belongs in {WORKFLOW_LINT_JOB}"
        )
        report = re.compile(rf"lint_report\.py\b.*--tool {re.escape(tool)}\b", re.S)
        reports = [step for step in steps if report.search(str(step.get("run", "")))]
        checker.expect(
            len(reports) == 1,
            f"workflow-lint.yml: {WORKFLOW_LINT_JOB} reports {tool} through lint_report.py in {len(reports)} steps; "
            "it needs one",
        )
        # every tool runs and reports even when another tool failed
        for step in runs[:1] + reports[:1]:
            checker.expect(
                step.get("continue-on-error") is True and bool(step.get("id")),
                f"workflow-lint.yml: {WORKFLOW_LINT_JOB} step '{step.get('name')}' needs an id and continue-on-error: true",
            )
    # the last step turns the steps' outcomes into the job's result
    continuing = [str(step.get("id")) for step in steps if step.get("continue-on-error") is True]
    last = steps[-1] if steps else {}
    text = yaml.safe_dump(last, width=1000)
    unread = [step_id for step_id in continuing if f"steps.{step_id}.outcome" not in text]
    checker.expect(
        bool(continuing) and not unread and "!cancelled()" in str(last.get("if", "")),
        f"workflow-lint.yml: {WORKFLOW_LINT_JOB}'s last step does not run whatever came before and read the "
        f"outcome of {unread or 'its lint steps'}",
    )


def check_job_names(checker, workflows):
    """Check every job's display name has a category prefix and the re-run pattern follows them.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    # every job, called or not, starts its display name with a category
    for name, workflow in workflows.items():
        for job_id, job in (workflow.get("jobs") or {}).items():
            display = str(job.get("name") or "")
            checker.expect(
                any(display.startswith(prefix) and display[len(prefix):].strip() for prefix in JOB_NAME_PREFIXES),
                f"{name}: job {job_id}'s name {display!r} does not start with one of {', '.join(repr(p) for p in JOB_NAME_PREFIXES)}",
            )
    # read the re-run workflow's pattern of build and publish job names
    rerun = workflows.get("rerun-unassigned.yml") or {}
    pattern = None
    for job in (rerun.get("jobs") or {}).values():
        pattern = pattern or (job.get("env") or {}).get("BUILD_JOBS")
    checker.expect(bool(pattern), "rerun-unassigned.yml: no BUILD_JOBS pattern")
    if not pattern:
        return
    build_jobs = re.compile(pattern)
    ci_jobs = (workflows.get("ci.yml") or {}).get("jobs") or {}
    categories = CATEGORIES.get("ci.yml", {})
    for job_id, job in ci_jobs.items():
        display = EXPRESSION_RE.sub("x", str(job.get("name") or ""))
        uses = str(job.get("uses") or "")
        # a called workflow's jobs show as `<caller> / <job>` and are never weighed
        if uses.startswith("./.github/workflows/"):
            called = workflows.get(uses.rsplit("/", 1)[1]) or {}
            for inner_id, inner in (called.get("jobs") or {}).items():
                shown = f"{display} / {EXPRESSION_RE.sub('x', str(inner.get('name') or ''))}"
                checker.expect(not build_jobs.search(shown), f"rerun-unassigned.yml: BUILD_JOBS matches {shown!r} ({job_id}/{inner_id}), which is no build job")
            continue
        weighed = categories.get(job_id) == "build" or job_id in RERUN_WEIGHED_JOBS
        matched = bool(build_jobs.search(display))
        checker.expect(matched == weighed, f"rerun-unassigned.yml: BUILD_JOBS {'misses' if weighed else 'matches'} ci.yml job {job_id} ({display!r})")


def check_sarif_uploads(checker, workflows):
    """Check code scanning gets every security and supply-chain lint, nothing else, from scanner-free jobs.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    uploaded = []
    for name, job_id, job, step in job_steps(workflows):
        run = str(step.get("run", ""))
        # every report names a classified tool, and the code scanning ones write SARIF
        for match in LINT_REPORT_RE.finditer(run):
            arguments = match.group(1)
            tool = re.search(r"--tool[ =]+(\S+)", arguments)
            tool = tool.group(1) if tool else "(none)"
            checker.expect(
                tool in SARIF_LINTS or tool in NO_SARIF_LINTS,
                f"{name}: {job_id} reports --tool {tool}, which check_ci_gating.py lists in neither SARIF_LINTS "
                "nor NO_SARIF_LINTS",
            )
            checker.expect(
                tool not in SARIF_LINTS or "--sarif-output" in arguments or (name, job_id) in SARIF_EXEMPT_JOBS,
                f"{name}: {job_id} reports {tool}, a code scanning lint, without --sarif-output",
            )
        if str(step.get("uses", "")).startswith("github/codeql-action/upload-sarif@"):
            category = str((step.get("with") or {}).get("category", ""))
            # a matrix category uploads once per matrix value
            matrix = ((job.get("strategy") or {}).get("matrix")) or {}
            key = re.fullmatch(r"\$\{\{\s*matrix\.([\w-]+)\s*\}\}", category)
            categories = [str(value) for value in as_list(matrix.get(key.group(1)))] if key else [category]
            uploaded += [(name, job_id, value) for value in categories]
    wanted = {category for categories in SARIF_LINTS.values() for category in categories}
    names = [category for _, _, category in uploaded]
    for category in sorted(wanted):
        checker.expect(
            names.count(category) == 1,
            f"code scanning category {category} is uploaded {names.count(category)} times; it needs one upload",
        )
    for name, job_id, category in uploaded:
        checker.expect(
            category in wanted, f"{name}: {job_id} uploads category {category!r}, which is not a SARIF_LINTS category"
        )
    # the token that writes code scanning results never meets scanner or repository code
    for name, workflow in workflows.items():
        for job_id, job in (workflow.get("jobs") or {}).items():
            permissions = (job or {}).get("permissions") or {}
            if not isinstance(permissions, dict) or permissions.get("security-events") != "write" or "steps" not in job:
                continue
            for step in job.get("steps") or []:
                uses = str(step.get("uses", ""))
                checker.expect(
                    not uses or uses.startswith(UPLOAD_JOB_ACTIONS),
                    f"{name}: {job_id} holds security-events: write and uses {uses}; only artifact downloads and "
                    "SARIF uploads may",
                )
                commands = commands_of(str(step.get("run", "")))
                found = [command for command in commands if SCANNER_COMMAND_RE.fullmatch(command)]
                checker.expect(
                    not found,
                    f"{name}: {job_id} holds security-events: write and runs {found} in '{step.get('name')}'",
                )


def decide_script(workflows):
    """Return the `run:` script of ci.yml's step that picks the areas, or None.

    # Arguments

    * `workflows` - The parsed workflows by file name
    """
    changes = ((workflows.get("ci.yml") or {}).get("jobs") or {}).get("changes") or {}
    for step in changes.get("steps") or []:
        if step.get("name") == DECIDE_STEP:
            return step.get("run")
    return None


def git(repo, *args):
    """Run git in a scratch repository and return its stripped output.

    # Arguments

    * `repo` - The repository to run in
    * `args` - The git arguments
    """
    result = subprocess.run(["git", *args], cwd=repo, env=git_env(), check=True, capture_output=True, text=True)
    return result.stdout.strip()


def git_env():
    """Return an environment that keeps git away from the user's and system's configuration."""
    env = {key: value for key, value in os.environ.items() if key in ("PATH", "LANG", "LC_ALL", "TMPDIR")}
    env.update(
        {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "check",
            "GIT_AUTHOR_EMAIL": "check@example.invalid",
            "GIT_COMMITTER_NAME": "check",
            "GIT_COMMITTER_EMAIL": "check@example.invalid",
            "HOME": tempfile.gettempdir(),
        }
    )
    return env


def commit(repo, paths, message):
    """Write each path with new content and commit them, returning the commit's sha.

    # Arguments

    * `repo` - The repository to commit in
    * `paths` - The repository-relative paths to write
    * `message` - The commit message
    """
    for path in paths:
        target = repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        with target.open("a", encoding="utf-8") as handle:
            handle.write(f"{message}\n")
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)
    return git(repo, "rev-parse", "HEAD")


def run_decide(script, scratch, changed, event, ref_name, ref_type="branch", new_branch=False, pull_request=False):
    """Run the step's script in a fresh repository for one change and return the areas it set.

    # Arguments

    * `script` - The step's `run:` script
    * `scratch` - The directory to make the repository in
    * `changed` - The paths the change touches
    * `event` - The GitHub event name
    * `ref_name` - The branch or tag name
    * `ref_type` - `branch` or `tag`
    * `new_branch` - Whether the push created the branch, so it has no previous commit
    * `pull_request` - Whether to check out a merge commit whose first parent is the base
    """
    repo = Path(tempfile.mkdtemp(dir=scratch))
    git(repo, "init", "-q", "-b", "main")
    before = commit(repo, ["README.md"], "base")
    if pull_request:
        git(repo, "checkout", "-q", "-b", "feature")
        commit(repo, changed, "change")
        git(repo, "checkout", "-q", "main")
        git(repo, "merge", "-q", "--no-ff", "-m", "merge", "feature")
    else:
        commit(repo, changed, "change")
    runner_temp = Path(tempfile.mkdtemp(dir=scratch))
    output = runner_temp / "output"
    summary = runner_temp / "summary"
    script_path = runner_temp / "decide.sh"
    script_path.write_text(script, encoding="utf-8")
    env = git_env()
    env.update(
        {
            "GITHUB_EVENT_NAME": event,
            "GITHUB_REF_TYPE": ref_type,
            "GITHUB_REF_NAME": ref_name,
            "BEFORE": "0" * 40 if new_branch else (before if event == "push" else ""),
            "IS_FORK": "false",
            "DEFAULT_BRANCH": "main",
            "GITHUB_OUTPUT": str(output),
            "GITHUB_STEP_SUMMARY": str(summary),
            "RUNNER_TEMP": str(runner_temp),
        }
    )
    result = subprocess.run(["bash", str(script_path)], cwd=repo, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        return None, result.stderr.strip()
    values = {}
    for line in output.read_text(encoding="utf-8").splitlines():
        key, _, value = line.partition("=")
        values[key] = value
    return values, ""


def check_decide(checker, workflows):
    """Run ci.yml's area step for one change per scenario and check the areas it sets.

    # Arguments

    * `checker` - The checker to record failures on
    * `workflows` - The parsed workflows by file name
    """
    script = decide_script(workflows)
    checker.expect(script is not None, f"ci.yml: the changes job has no '{DECIDE_STEP}' step")
    if script is None:
        return
    every = dict.fromkeys(AREAS, "true")
    none = dict.fromkeys(AREAS, "false")
    publish = {PUBLISH_OUTPUT: "true"}
    keep = {PUBLISH_OUTPUT: "false"}
    # (description, changed paths, run_decide keyword arguments, expected areas)
    scenarios = [
        ("a push to the default branch", ["ui/src/app.tsx"], {"event": "push", "ref_name": "main"}, every | publish),
        (
            "a push to the default branch changing only deploy/charts/",
            ["deploy/charts/thorium/values.yaml"],
            {"event": "push", "ref_name": "main"},
            every | keep,
        ),
        (
            "a push to the default branch changing only a workflow README",
            [".github/workflows/README.md"],
            {"event": "push", "ref_name": "main"},
            every | keep,
        ),
        (
            "a push to the default branch changing ci.yml",
            [".github/workflows/ci.yml"],
            {"event": "push", "ref_name": "main"},
            every | publish,
        ),
        (
            "a manual run",
            ["deploy/charts/thorium/values.yaml"],
            {"event": "workflow_dispatch", "ref_name": "feature"},
            every | publish,
        ),
        (
            "a new branch",
            ["deploy/charts/thorium/values.yaml"],
            {"event": "push", "ref_name": "feature", "new_branch": True},
            every | publish,
        ),
        ("a feature push changing ui/", ["ui/src/app.tsx"], {"event": "push", "ref_name": "feature"}, none | publish),
        (
            "a feature push changing thorctl/",
            ["thorctl/src/main.rs"],
            {"event": "push", "ref_name": "feature"},
            {"thorctl": "true", "rust": "true", "thoradm": "false", "charts": "false", "ci": "false"} | publish,
        ),
        (
            "a feature push changing thoradm/",
            ["thoradm/src/main.rs"],
            {"event": "push", "ref_name": "feature"},
            {"thoradm": "true", "rust": "true", "thorctl": "false", "charts": "false", "ci": "false"},
        ),
        (
            "a feature push changing minithor/",
            ["minithor/minithor"],
            {"event": "push", "ref_name": "feature"},
            {"deploy-tools": "true", "rust": "false", "thoradm": "false", "thorctl": "false", "ci": "false"} | keep,
        ),
        ("a feature push changing .github/", [".github/scripts/new.py"], {"event": "push", "ref_name": "feature"}, every),
        (
            "a feature push changing an unlisted path",
            ["newcrate/src/lib.rs"],
            {"event": "push", "ref_name": "feature"},
            {"rust": "true", "thoradm": "true", "thorctl": "true", "ci": "false"},
        ),
        (
            "a release tag",
            ["thorctl/src/main.rs"],
            {"event": "push", "ref_name": "1.2.3", "ref_type": "tag"},
            none | publish,
        ),
        (
            "a pull request changing deploy/charts/",
            ["deploy/charts/thorium/values.yaml"],
            {"event": "pull_request", "ref_name": "7/merge", "pull_request": True},
            {"charts": "true", "deploy-tools": "true", "rust": "false", "thoradm": "false", "thorctl": "false"},
        ),
    ]
    scratch = tempfile.mkdtemp(prefix="check-ci-gating-")
    try:
        for description, changed, kwargs, expected in scenarios:
            values, error = run_decide(script, scratch, changed, **kwargs)
            checker.expect(values is not None, f"ci.yml: '{DECIDE_STEP}' failed for {description}: {error}")
            if values is None:
                continue
            wrong = {
                area: values.get(area, "(unset)") for area, want in expected.items() if values.get(area) != want
            }
            checker.expect(
                not wrong,
                f"ci.yml: '{DECIDE_STEP}' for {description} set {wrong}, expected "
                + str({area: expected[area] for area in wrong}),
            )
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def main():
    """Check the workflows and print each broken rule."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", default=str(DEFAULT_ROOT), help="the checkout to check (default: this one)")
    args = parser.parse_args()
    checker = Checker(args.root)
    workflows = load_workflows(checker)
    check_classified(checker, workflows)
    check_ungated(checker, workflows)
    check_ci_jobs(checker, workflows)
    check_rust_check_inputs(checker, workflows)
    check_other_gates(checker, workflows)
    check_macos_binaries(checker, workflows)
    check_musl_binaries(checker, workflows)
    check_multiarch_image(checker, workflows)
    check_rustflags(checker, workflows)
    check_scans_grouped(checker, workflows)
    check_publish_independent(checker, workflows)
    check_publish_order(checker, workflows)
    check_scans_always_run(checker, workflows)
    check_published_rescan(checker, workflows)
    check_tag_publish(checker, workflows)
    check_workflow_lint(checker, workflows)
    check_sarif_uploads(checker, workflows)
    check_job_names(checker, workflows)
    check_decide(checker, workflows)
    for failure in checker.failures:
        print(f"FAIL: {failure}")
    if checker.failures:
        print(f"{len(checker.failures)} of {checker.checks} CI change gating checks failed")
        return 1
    print(f"all {checker.checks} CI change gating checks passed for {len(workflows)} workflows")
    return 0


if __name__ == "__main__":
    sys.exit(main())
