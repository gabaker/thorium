# Thorium's GitHub workflows

Each workflow file's header comment holds the details; this page is the map.

| Workflow | Name in the Actions tab | When it runs | What it does |
| -------- | ----------------------- | ------------ | ------------ |
| `ci.yml` | Thorium Build | every push and pull request, and on demand | builds, then publishes beside the security scans, then checks, in stages ordered with `needs:` |
| `rust-check.yml` | Rust Check | called by `ci.yml` when Rust code changes (the thoradm and thorctl tests only when their code changes), weekly, and on demand | cargo check, Clippy, rustfmt, the unit tests and thoradm and thorctl tests that need no services, and an integration test compile check |
| `security-scan.yml` | Security Scan | called by `ci.yml` once the core image is built, weekly, and on demand | Trivy scans for secrets, Helm and Ansible config, and dependencies, and of the core image `ci.yml` built (or, on its own weekly and manual runs, of the published images) |
| `workflow-lint.yml` | Workflow Lint | called by `ci.yml`, weekly, and on demand | zizmor, the action pin check, and actionlint, in one job |
| `dockerfile-lint.yml` | Dockerfile Lint | called by `ci.yml`, weekly, and on demand | hadolint, Trivy config scans, and the pinned tool version check |
| `tools.yml` | Analysis Tool Builds | nightly, pushes to `main`, pull requests that change `tools/` or `.github/`, and on demand | checks the toolbox and builds the analysis tool images |
| `rerun-unassigned.yml` | Re-run Unassigned Jobs | whenever a Thorium Build run completes | re-runs build and publish jobs no hosted runner picked up |

## The stages of Thorium Build

1. **Check · changed paths** decides which lints and tests the run needs from the paths it changes.
2. **Builds**, in parallel: the mdBook docs, the UI bundle, **Build · Helm charts** (the chart
   package), and the Linux, Windows, and macOS binaries. **Build · Linux binaries (musl)** and
   **Build · Linux arm64 binaries (musl)** build the static x86_64 and arm64 binaries the image
   serves, both through cross. **Build · macOS thorctl** and **Build · macOS thoradm** each build
   one binary for both macOS targets, one target after the other, so a run asks for two of the
   scarce macOS runners and thorctl builds thorium-api with its client features only.
3. **Build · core container image** from the build artifacts. Once it is built, the **Security
   scans** (`security-scan.yml`) start: the scan of this image and the repository scans for
   secrets, Helm and Ansible config, and dependencies. They run beside the publish, and nothing
   waits for them.
4. **Publish · core container image**, once the image is built and the charts packaged, on the
   default branch, release tags, and fork branches (never on pull requests). A branch push
   publishes the image only when a path it is built from changed (the UI, the docs, the
   Dockerfile, Rust or any unlisted path, or this workflow and its scripts), or when there is no
   previous commit to compare. **Publish · Helm charts** follows the image the charts deploy.
   A release tag, `1.2.3` or `v1.2.3`, publishes the image as `<registry>/thorium:<tag>` only, the
   tag as pushed, and the charts with the tag less a leading `v` as their version, deploying that
   image. A tag that is not SemVer builds and is scanned but publishes nothing; the build job
   decides this before any push and logs a warning saying why. Only a default-branch run that
   publishes the image points `latest` at the highest release on the default branch, so a tag
   never moves `latest`, `main`, or a branch's tag; a release becomes `latest` with the next such
   run.
5. After the image publish, in parallel: the workflow and Dockerfile lints, and the lints and
   tests whose areas changed (the Rust checks, version consistency, the Helm chart lint, the
   deployment script checks, and the CI change gating check, CI script tests, and CI name limit
   check).

GitHub Pages deploys from the default branch once the docs are built and the image is published.

## What always runs and what is gated

Every run, on every branch, tag, and pull request, builds everything (the binaries, the core
image, the docs, the UI bundle, and the Helm charts) and runs every security scan: Trivy of every
kind, zizmor, actionlint, hadolint, the action pin and pinned tool version checks, and their code
scanning uploads. No build or scan job has a path condition, and no workflow with one has a `paths:`
filter. Publishing stays limited to the refs `release_refs.py` and the Pages job allow, and to
the changes in step 4 above: a push to the default branch runs every lint and test but publishes
the image only when one of those paths changed.

The lints and tests run only when their area changed, as **Check · changed paths** decides:

| Area | Changed paths | What runs |
| ---- | ------------- | --------- |
| `rust` | any Rust code (any path the table doesn't list counts), Cargo files, the thoradm guides, the operator subchart, the committed toolboxes | Rust checks (`rust-check.yml`), CI name limit check (in the version consistency job) |
| `thoradm` | `thoradm/`, the Rust code it builds on (Cargo files, `api/`, `thorium-derive/`, `cart-rs/`), the thoradm guides | thoradm tests |
| `thorctl` | `thorctl/`, the same shared Rust code, the committed toolboxes | thorctl tests |
| `charts` | `deploy/charts/` | Helm chart lint |
| `deploy-tools` | `minithor/`, `megathor/`, `deploy/charts/` | minithor and megathor checks |
| `versions` | the files `.github/scripts/versions.py` reads | version consistency |
| `ci` | `.github/` | CI change gating check and CI name limit check (in the version consistency job) |

A push to the default branch, a manual run, a new branch (nothing to compare with), and any change
under `.github/` run every lint and test. A release tag runs none, since its commit was checked on
its branch. The reusable workflows' own weekly schedules run all their repo jobs, and
`security-scan.yml`'s rescans the published images in place of a build's image. `tools.yml` builds the
tool images from `main` only and checks the toolbox on pull requests that change `tools/` or
`.github/`.

`.github/scripts/check_ci_gating.py` checks these rules: it classifies every job of every workflow
(a new job fails it until it is added), checks builds and scans have no path conditions and lints
and tests do, checks every scan sits in `security-scan.yml` and no publish waits for a scan or
check, and runs the change detection step against scratch git repositories. It also checks the
workflow lint job and the code scanning uploads below, that the macOS and static Linux builds'
binaries reach the core image, and that every step compiling Rust builds each target with the
target features gxhash needs on its architecture: `+aes,+sse2` on every x86_64 target (macOS
included) and `+aes,+neon` on aarch64 Linux, with AES and no SSE2 on aarch64 macOS. A target or
flags set by an expression fail the check, aarch64 never takes a workflow-wide `RUSTFLAGS`, the
Linux musl and macOS builds set `LZMA_API_STATIC=1`, and `ci.yml` must build every target the
check lists (Linux glibc, musl and arm64 musl, Windows, both macOS targets).

## Code scanning

The security and supply-chain lints, whose findings point at a file and line, upload SARIF to
code scanning, each under its own category: zizmor, the action pin check, and actionlint
(`workflow-lint.yml`), hadolint, the Trivy Dockerfile scan, and the pinned tool version check
(`dockerfile-lint.yml`), the Trivy secret, Helm and Ansible, and dependency scans
(`security-scan.yml`), and the Trivy image scans. The compile and formatting lints (cargo check,
Clippy, rustfmt, the Helm chart lint, the deployment script checks, the UI style check, the
version consistency check, and the schema change warnings) report in their jobs only, so code
scanning holds only security and supply-chain findings. Every lint reports every finding in its
job log, annotations, job summary, and report artifact either way.

The upload runs in a job of its own, the only one holding `security-events: write`, which
downloads the SARIF artifacts and uploads them and runs no scanner, script, or repository code.
`check_ci_gating.py` lists which lint_report.py tools upload (`SARIF_LINTS`) and which do not
(`NO_SARIF_LINTS`), so a new lint fails it until it is placed in one, and checks each upload job
holds no scanner.

## Checks never block builds

No build, publish, package, or Pages job, nor any job one of them needs, needs a check, test,
scan, or lint job, directly or through its `if:`. The image and chart publish wait only for the
image build and the chart package, and the chart publish also for the image publish, so a chart is
pushed only after the image it deploys. The security scans start once the image is built, beside
the publish, and the checks after the publish, only so the builds get the runners first; they run
whatever the builds concluded (`!cancelled()`), so a failed build never hides them, and a failed
or slow scan or check never skips or delays a build or a publish. Each reports its own result.

## Job names

Every job's display name starts with its category and ` · `, then says what it works on, with the
tool in parentheses where it helps: `Build · Linux binaries (glibc)`, `Publish · core container
image`, `Scan · secrets (Trivy)`, `Lint · workspace (Clippy)`, `Check · versions and CI gating`,
`Test · thoradm (no services)`. The categories are `Build`, `Publish`, `Scan`, `Lint`, `Check`, and
`Test`; a job that uploads or reports another job's results takes that job's category (`Scan ·
upload repository scan results`). A job in `ci.yml` that calls a reusable workflow is named for the
group (`Test · Rust`, `Scan · security`), which GitHub shows before the name of each job it runs.
`check_ci_gating.py` fails on a job whose name has no category prefix.
Job ids stay short and are what `needs:` and the callers use; the display name is what the Actions
tab, branch protection rules, and `rerun-unassigned.yml` see. A job that does the same thing as a
job of Thorium's internal GitLab pipeline has the same name there.

Renaming a job changes the check name that branch protection rules require, so update any rule
that names it.

## Re-running unassigned jobs

`rerun-unassigned.yml` weighs only the build and publish jobs, which it matches by display name
with the `BUILD_JOBS` pattern: `Check · changed paths` and any name starting with `Build · ` or
`Publish · `. A new build or publish job gets one of those prefixes, which the job name rule above
gives it, and a check, scan, test, or lint job never has one, so its failures never stop a re-run.

## Running the checks locally

- The Rust checks: the `cargo` commands in `rust-check.yml`, with
  `RUSTFLAGS="-C target-feature=+aes,+sse2"` on x86_64 (`+aes,+neon` on aarch64).
- The workflow lints: `actionlint` and `zizmor --offline .github/workflows`.
- Version consistency: `python3 .github/scripts/versions.py check`.
- The change gating rules: `python3 -I .github/scripts/check_ci_gating.py` (needs PyYAML).
- The CI script tests: `python3 -I -m unittest discover -s .github/scripts -p 'test_*.py'`.
- The CI name limits: `python3 -I .github/scripts/check_ci_name_limits.py` (`--list` prints every
  measured name). The integration tests name Scylla keyspaces (at most 48 characters) and S3
  buckets (at most 63) from CI's run id and namespace, and a local run uses shorter ones, so a name
  over its limit would fail only in CI; the check measures each from the test sources at its CI
  length.
