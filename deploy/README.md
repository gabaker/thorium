# Thorium Helm Charts

This folder holds the Helm charts that deploy Thorium and its backing services onto any
Kubernetes cluster, and the scripts used to build, check, and publish them. `minithor/`
(development clusters) and `megathor/` (production clusters on bare metal or VMs) both install
these charts.

This README is for chart maintainers. Admins deploying Thorium should read the deployment guide in
the Thorium docs (Admins > Deploying Thorium, source in `api/docs/src/admins/deploy/`), starting
with "Deploying Thorium" (`deploy.md`) and "Install with Helm" (`deploy-helm.md`).

## Layout

| Path | Purpose |
|------|---------|
| `charts/infra-operators` | Cluster-wide operators: Scylla, Elastic Cloud on Kubernetes (ECK), Kubegres. Installed once per cluster |
| `charts/thorium` | Thorium itself (once per cluster): the umbrella chart over the `secrets`, `infra`, `quickwit`, and `operator` subcharts in `charts/thorium/charts/` |
| `charts/thorium/values-production.yaml` | A multi-node starting point layered over the chart defaults |
| `charts/scripts/convert-to-helm.sh` | Converts a deployment made by the pre-Helm minithor or megathor scripts so the charts can take it over |
| `charts/scripts/vendor-charts.sh` | Re-vendors the upstream charts (Quickwit, Scylla operator, ECK operator, Kubegres) with their patches |
| `charts/scripts/list-images.sh` | Prints every image the charts deploy, with its path in a mirror registry |
| `charts/scripts/package.sh` | Packages both charts, optionally pointing the thorium chart at a given Thorium image; used by the Helm Charts workflow and for local testing |

Both charts carry all of their subcharts, so they install from a directory or packaged `.tgz`
with no dependency downloads.

## How the thorium chart fits together

| Subchart | Contents |
|----------|----------|
| `secrets` | Thorium's namespaces and one canonical `thorium-credentials` Secret. Credentials not supplied are generated on first install and read back (Helm `lookup`) on every upgrade, then rendered into each consumer's own Secret (Redis password, SeaweedFS S3 identities, Quickwit/Postgres credentials, the admin user, Elasticsearch's file-realm user and role, a partial `thorium.yml`, the optional registry's htpasswd) |
| `infra` | Redis, SeaweedFS (`weed mini`), the `ScyllaCluster`, Elasticsearch/Kibana, Kubegres Postgres for Quickwit's metastore (each only when `global.managed` keeps it), Jaeger, an optional registry, and jobs that create the Quickwit bucket and metastore database in the chart's SeaweedFS and Postgres |
| `quickwit` | The upstream Quickwit chart, vendored and patched to install into `<prefix>-quickwit` and honor `global.imageRegistry` and `global.clusterDomain`; installed only with `global.quickwit.enabled` |
| `operator` | The operator, its RBAC and CRD, the `ThoriumCluster` (no secrets in `spec.config`; credentials arrive through `config_secrets` and `bootstrap`), ingress (nginx or Traefik), the uninstall hook, and an optional toolbox import job |

`global.managed.<service>` and `global.quickwit.enabled` are shared by every subchart, so each one
follows the same toggles. Every chart has a `values.schema.json`, and each subchart (and the
umbrella) fails rendering on a moved value with "<old> is not a chart value; use <new>"
(`thorium.movedValues`), so old values files break loudly instead of being ignored. When you
move or remove a value, add it to that list and to the schema.

## Publishing

`.github/workflows/charts.yml` publishes both charts to `oci://ghcr.io/<owner>/<repo>/charts`
(`oci://ghcr.io/cisagov/thorium/charts` for the core repo) under the same rules as the Thorium
image (`.github/scripts/release_refs.py`): prereleases
(`<chart version>-<branch>.<run number>.g<short sha>`) from `main` on the core repo and from every
branch on forks, releases from `X.Y.Z` release tags (equal to the chart version or a prerelease of
it), and nothing from pull requests. A published thorium chart deploys the image `deploy.yml`
publishes for the same branch or tag in the same repository, so a fork's charts run that fork's
image. The two workflows run independently, so a chart can be published before its image is, or
without one when the image build fails.

## Versions

The charts are versioned with the rest of Thorium. `python3 .github/scripts/versions.py bump <version>`
writes one version into the Cargo workspace (including Cargo.lock and the Python crate), the web
UI's package files, every Thorium chart's `version`, `appVersion`, and subchart pins, the minithor
and megathor default chart versions, and the documentation examples. Without a version it syncs
everything to the current `Cargo.toml` workspace version. `versions.py check`, run by CI, fails
when any of them disagree. Documentation lines that carry a version are listed in its
`PATTERN_PINS` (for example `VERSION=<version>` at the start of a line in
`api/docs/src/admins/deploy/deploy-helm.md`); update the pin when such a line moves, and add one
for any new version example.

## Developing the charts

`helm lint` and `helm template` can't read the generated credentials back from a cluster, so render
with `secrets.renderOnly`, which stands in placeholder credentials and fails during a real install:

```bash
helm lint --strict charts/infra-operators
helm lint --strict -n thorium --set secrets.renderOnly=true charts/thorium
helm template thorium charts/thorium -n dev-thorium --set secrets.renderOnly=true > /dev/null
helm template thorium charts/thorium -n thorium --set secrets.renderOnly=true \
  -f charts/thorium/values-production.yaml > /dev/null
```

CI also checks that rendering without `renderOnly` stops at the credentials guard, and checks the
namespace blacklist rendered for `-n dev-thorium`:

```bash
helm template thorium charts/thorium -n dev-thorium --set secrets.renderOnly=true \
  --show-only charts/operator/templates/thoriumcluster.yaml | grep -A24 namespace_blacklist
```

To test charts on a cluster before publishing them, package them (optionally pointing the thorium
chart at another Thorium image) and install the packages, for example with minithor:

```bash
charts/scripts/package.sh -d /tmp/charts --image-repository ghcr.io/<owner>/<repo>/infrastructure/thorium --image-tag <tag>
minithor deploy --chart /tmp/charts/thorium-<version>.tgz
```

`charts/scripts/list-images.sh [values files...]` prints every image for the given thorium chart
values (and the infra-operators values files listed in `INFRA_OPERATORS_VALUES`), as
`<source> <mirror path>` lines that megathor's `scripts/mirror-images.bash` reads.
`charts/scripts/vendor-charts.sh` re-vendors the upstream charts at the versions set in the script
and reapplies the patches; it has no options, so read its header before running it and review the
diff afterwards.

## Adding an upgrade revision

A change that needs work on existing deployments (beyond what a normal reconcile does) ships as a
new revision:

1. Add a `CatalogEntry` (`YYYY-MM-vNN`, after the last one) with its `StepDef`s to `CATALOG` in
   `api/src/models/upgrades.rs`. `Data` and `Manual` steps need an approval; give them a
   `manual_procedure` when the operator can't do the work itself.
2. Add an executor for every new step to `EXECUTORS` in `operator/src/upgrades/steps.rs`
   (unattended for `Config`/`Infra`, guarded for `Data`/`Manual`); the `every_step_has_an_executor`
   test enforces it.
3. Register any data migration a step names (`thoradm_migration`) in `MIGRATIONS` in
   `thoradm/src/migrate.rs`; `catalog_migrations_are_registered` enforces it.
4. Review how a namespace without recorded state is seeded (`operator/src/upgrades/detect.rs`).
5. Document the revision and its steps in "Upgrading Thorium"
   (`api/docs/src/admins/deploy/upgrades.md`, section "Revisions").

## Pointers for admins

Messages from the charts, the operator, minithor, megathor, and `convert-to-helm.sh` refer to the
sections below. Each topic is documented in the Thorium docs (Admins > Deploying Thorium):

### External services

See "External services" in "Configure the Helm Charts"
(`api/docs/src/admins/deploy/helm-configuration.md`).

### Credentials that must not change or be lost

See "Credentials" in "Configure the Helm Charts"
(`api/docs/src/admins/deploy/helm-configuration.md`).

### Upgrades

See "Upgrading Thorium" (`api/docs/src/admins/deploy/upgrades.md`).

### Converting a Pre-Helm Deployment

See "Converting a Pre-Helm Deployment" (`api/docs/src/admins/deploy/convert-to-helm.md`),
including rolling a conversion back. To keep a pre-Helm deployment outside Helm, see "Moving a
pre-Helm deployment without Helm" in "Deploy the Operator Without Helm"
(`api/docs/src/admins/deploy/deploy-thorium.md`).

## Known limitations

See "Known limitations" in "ThoriumCluster and Operator Reference"
(`api/docs/src/admins/deploy/thoriumcluster.md`).
