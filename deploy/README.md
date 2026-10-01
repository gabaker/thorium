# Thorium Helm Charts

This folder holds the Helm charts that deploy Thorium and its backing services onto any
Kubernetes cluster. `minithor/` (single-node development) and `megathor/` (multi-node
clusters) both install these charts, and they can be installed directly with Helm. The
user-facing guide is the mdbook page `api/docs/src/admins/deploy/deploy-helm.md`.

## Layout

| Path | Purpose |
|------|---------|
| `charts/infra-operators` | Cluster-wide operators: Scylla, Elastic Cloud on Kubernetes (ECK), Kubegres. Installed once per cluster |
| `charts/thorium` | One Thorium instance: the umbrella chart over the `secrets`, `infra`, `quickwit`, and `operator` subcharts in `charts/thorium/charts/` |
| `charts/thorium/values-production.yaml` | A multi-node starting point layered over the chart defaults |
| `charts/vendor-charts.sh` | Re-vendors the upstream charts (Quickwit, Scylla operator, ECK operator, Kubegres) with their patches |
| `charts/list-images.sh` | Prints every image the charts deploy, with its path in a mirror registry |

Both charts carry all of their subcharts, so they install from a directory or packaged `.tgz`
with no dependency downloads. They are published to `oci://ghcr.io/cisagov/thorium/charts` by
`.github/workflows/charts.yml` (prereleases from `main`, releases from `X.Y.Z` release tags, which must match the chart version).

## How the thorium chart fits together

| Subchart | Contents |
|----------|----------|
| `secrets` | The instance's namespaces and one canonical `thorium-credentials` secret. Credentials not supplied are generated on first install and read back (Helm `lookup`) on every upgrade, then rendered into each consumer's own secret format (Redis password, SeaweedFS S3 identities, Quickwit/Postgres credentials, the admin user, a partial `thorium.yml`, the optional registry's htpasswd) |
| `infra` | Redis, SeaweedFS (`weed mini`), the `ScyllaCluster`, Elasticsearch/Kibana, Kubegres Postgres for Quickwit's metastore, Jaeger, an optional registry, and jobs that create the Quickwit bucket and metastore database |
| `quickwit` | Upstream Quickwit chart, vendored and patched to install into `<prefix>quickwit` and honor `global.imageRegistry` |
| `operator` | The namespace-scoped Thorium operator, its RBAC and CRD, the `ThoriumCluster` (no secrets in `spec.config`; credentials arrive through `config_secrets` and `bootstrap`), ingress (nginx or Traefik), and an optional toolbox import job |

The operator merges the `config_secrets` over `spec.config`, creates Thorium's Scylla role and
Elastic user from admin credentials, deploys the Thorium components, registers and provisions
the scaler's nodes, creates the initial admin user, and reports progress in `status.phase`.

## Instances and namespaces

An instance is installed into `thorium` or `<prefix>thorium` (e.g. `b-thorium`); every chart
derives the prefix from the release namespace and uses it for all of the instance's
namespaces (`<prefix>redis`, `<prefix>scylla`, `<prefix>elastic`, `<prefix>seaweedfs`,
`<prefix>quickwit`, `<prefix>jaeger`). Install with `--create-namespace`; the `secrets`
subchart creates the rest.

Several instances can share a cluster: the operators are shared and each instance's Thorium
operator only watches its own namespace. Run the Thorium k8s scaler in only one instance
(`operator.cluster.components.scaler: null` elsewhere), since it names namespaces after
Thorium groups.

## Install

```bash
helm install infra-operators deploy/charts/infra-operators -n infra-operators --create-namespace --wait
helm install thorium deploy/charts/thorium -n thorium --create-namespace -f site.yaml
kubectl -n thorium wait thoriumcluster/thorium --for=jsonpath='{.status.phase}'=Ready --timeout=30m
```

Replace the chart paths with `oci://ghcr.io/cisagov/thorium/charts/<chart> --version <version>`
to use published charts. The `secrets` subchart uses Helm `lookup` to keep generated
credentials stable, so install against a live cluster rather than rendering with
`helm template`.

## Offline

Pull or package both charts, mirror the images `charts/list-images.sh` prints (each at its
upstream path without the registry host), and install with `global.imageRegistry=<registry>`
on the thorium chart, plus `global.imageRegistry`, `scylla-operator.image.repository`, and
`eck-operator.image.repository` on infra-operators. `global.imageRegistry` also covers the
images the operators start (Elasticsearch, Kibana, Scylla, Thorium components).

## Credentials

```bash
kubectl -n thorium get secret thorium-admin -o jsonpath='{.data.password}' | base64 -d
kubectl -n thorium get secret thorium-credentials -o json | jq '.data | map_values(@base64d)'
```

Set any credential under `secrets.credentials` to supply your own. Existing values are never
regenerated; to rotate one, set it explicitly.

## Known limitations

- Elasticsearch TLS is not verified in-cluster (`operator.backends.elastic.insecureCertificates`)
  because ECK serves a self-signed certificate and the operator cannot yet mount ECK's CA.
- The ThoriumCluster CRD is cluster-scoped: instances on one cluster should run operator
  versions with the same CRD schema.
- The Scylla operator's webhook certificate is generated by the chart (valid 10 years) instead
  of by cert-manager.
