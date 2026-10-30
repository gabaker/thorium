# Thorium's GitHub workflows

Each workflow file's header comment holds the details; this page is the map.

| Workflow | Name in the Actions tab | When it runs | What it does |
| -------- | ----------------------- | ------------ | ------------ |
| `ci.yml` | Thorium CI | every push and pull request, and on demand | builds, publishes, then checks and scans, in stages ordered with `needs:` |
| `rust-check.yml` | Rust Check | called by `ci.yml` when Rust code changes, weekly, and on demand | cargo check, Clippy, rustfmt, the unit tests and thoradm and thorctl tests that need no services, and an integration test compile check |
| `security-scan.yml` | Security Scan | called by `ci.yml`, weekly, and on demand | Trivy scans for secrets, Helm and Ansible config, and dependencies |
| `workflow-lint.yml` | Workflow Lint | called by `ci.yml` when `.github/` changes, weekly, and on demand | zizmor and the action pin check |
| `dockerfile-lint.yml` | Dockerfile Lint | called by `ci.yml` when a Dockerfile changes, weekly, and on demand | hadolint, Trivy config scans, and the pinned tool version check |
| `image-rescan.yml` | Image Rescan | weekly and on demand | rescans the published Thorium images with Trivy |
| `tools.yml` | Analysis Tool Builds | nightly, pushes to `main`, pull requests that change `tools/`, and on demand | checks the toolbox and builds the analysis tool images |
| `rerun-unassigned.yml` | Re-run Unassigned Jobs | whenever a Thorium CI run completes | re-runs build and publish jobs no hosted runner picked up |

## The stages of Thorium CI

1. **Detect changed paths** decides which stages the run needs from the paths it changes.
2. **Builds**: the mdBook docs, the UI bundle, and the Linux and Windows binaries. macOS thorctl
   builds once the Linux-hosted Rust builds succeed, so the scarce macOS runners are asked for only
   when the image can still be built.
3. **Build core container image** from the build artifacts, then **Publish core container image**
   on the default branch, release tags, and fork branches (never on pull requests).
4. After the publish, in parallel: the image scan, the Rust checks, the security scans, version
   consistency, the Helm chart lint, package, and publish, the deployment script checks, and the
   workflow and Dockerfile lints when their files changed.

GitHub Pages deploys from the default branch once the docs are built and the image is published.

## Checks never block builds

No build, publish, package, or Pages job needs a check, test, scan, or lint job, directly or
through its `if:`. The checks run after the builds only so the builds get the runners first, and
they run whatever the builds concluded (`!cancelled()`), so a failed build never hides them and a
failed check never skips a build or a publish. Each check reports its own result.

## Job names

Every job has a display name that says what it does, verb first, with the tool in parentheses where
it helps: `Build Linux binaries (glibc)`, `Check the workspace (cargo check)`, `Lint the workspace
(Clippy)`, `Scan for secrets (Trivy)`. A job in `ci.yml` that calls a reusable workflow is named for
the group (`Rust checks`, `Security scans`), which GitHub shows before the name of each job it runs.
Job ids stay short and are what `needs:` and the callers use; the display name is what the Actions
tab, branch protection rules, and `rerun-unassigned.yml` see. A job that does the same thing as a
job of Thorium's internal GitLab pipeline has the same name there.

Renaming a job changes the check name that branch protection rules require, so update any rule
that names it.

## Re-running unassigned jobs

`rerun-unassigned.yml` weighs only the build and publish jobs, which it matches by display name
with the `BUILD_JOBS` pattern: `Detect changed paths`, any name starting with `Build `,
`Publish `, or `Package `, and `Deploy docs to GitHub Pages`. A new build or publish job needs a
name that pattern matches, and a check, scan, test, or lint job must not start with one of those
words, or its failures would stop a re-run.

## Running the checks locally

- The Rust checks: the `cargo` commands in `rust-check.yml`, with
  `RUSTFLAGS="-C target-feature=+aes,+sse2"` on x86_64.
- The workflow lints: `actionlint` and `zizmor --offline .github/workflows`.
- Version consistency: `python3 .github/scripts/versions.py check`.
