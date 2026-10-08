# Deployment Concepts

This page explains the parts of a Thorium deployment and the vocabulary the other deployment
pages use: the components, the backing services, the Thorium operator and its `ThoriumCluster`
resource, the Helm charts, namespaces, configuration and credentials, and upgrade revisions.

## Components

Every Thorium component runs from one container image,
`ghcr.io/cisagov/thorium/infrastructure/thorium:<version>`, which also carries the operator, the
admin tool `thoradm`, and the `thorctl`, agent, and reactor binaries the API serves for download.

| Component | Kubernetes object | Role |
|-----------|-------------------|------|
| API | Deployment `api`, Services `thorium-api` and `thorium-mcp` | The REST API and web UI |
| Scaler | Deployment `scaler` (and `baremetal-scaler` for bare-metal workers) | Schedules jobs as pods in per-group namespaces |
| Event handler | Deployment `event-handler` | Triggers reactions from events |
| Search streamer | Deployment `search-streamer` | Creates Thorium's Elasticsearch indexes and streams results and tags into them from Scylla |
| Agent | Installed on each worker node by node provision pods (`app=node-provisioner`) | Runs jobs on the node |
| Operator | Deployment `operator` | Deploys, checks, and upgrades everything above |

See [Architecture](../../architecture/architecture.md) for what each component does inside
Thorium.

## Backing services

| Service | Holds | Notes |
|---------|-------|-------|
| ScyllaDB | Thorium's primary data | The API creates its keyspace |
| Redis | Users, groups, settings, caches, and job queues | |
| Elasticsearch | The search index of results and tags | Derived data: the search streamer rebuilds it from Scylla |
| S3-compatible store | Files, repos, results, comment attachments, graphics, ephemeral files, and the reaction cache | The operator creates Thorium's buckets |
| Quickwit, Postgres, Jaeger | Traces (Quickwit stores them in S3 and keeps its metastore in Postgres; Jaeger is the trace viewer) | Optional |

The Helm charts can deploy every backing service, or point Thorium at services you already run
(see [External services](./helm-configuration.md#external-services) and
[Bring Your Own Infrastructure](./infrastructure.md)).

## The operator and the ThoriumCluster resource

A `ThoriumCluster` (API group `sandia.gov/v1`) describes one Thorium deployment: the image and
version, the components with their resources, the non-secret Thorium configuration, the Secrets
holding credentials, and how to upgrade. The Thorium operator watches it and, on every
reconcile:

1. decides whether the deployment must be upgraded first (see [Revisions](#revisions));
2. writes the `banner` and `tracing-conf` ConfigMaps and the registry pull secrets;
3. renders `thorium.yml` from the spec and its config Secrets, creates Thorium's S3 buckets, runs
   any privileged bootstrap (Thorium's Scylla role, an external Elasticsearch user), and checks
   that Thorium's own credentials work against Elasticsearch, Scylla, and Redis;
4. only then writes the rendered config to the `thorium` Secret, so a bad credential is
   reported in the status instead of being rolled out;
5. deploys the API, creates its own Thorium users and the initial admin user, and writes the
   `keys` Secret the other components use;
6. deploys the scaler, the event handler, and the search streamer;
7. deploys node provision pods (which install the agent), then labels and registers the worker
   nodes. A node that is down is skipped until it returns, and a node that fails to provision
   doesn't hold back the components; the cluster reports `Error` listing it and retries it every
   minute;
8. reports `Ready` once every component Deployment has rolled out and is available.

The operator reports progress in the resource's status:

| Phase | Meaning |
|-------|---------|
| `Provisioning` | Deploying, or waiting for something that finishes on its own (a backend that isn't accepting connections yet, a rollout in progress, a component that became unavailable such as after its node went down); rechecked every 15 seconds |
| `Ready` | Every backend check passed and every component finished rolling out and is available |
| `Error` | Something needs attention; `status.message` names it |
| `UpgradeRequired` | The operator knows a newer revision than the deployment's and waits for a target revision; nothing is changed |
| `Upgrading` | The operator is running the steps that bring the deployment to its target revision |

The operator applies the `ThoriumCluster` CustomResourceDefinition itself when it starts, so a
new operator image also brings the matching CRD schema.
[ThoriumCluster and Operator Reference](./thoriumcluster.md) lists every field, and
[Operator](../../architecture/operator.md) describes how the operator works.

## One deployment per Kubernetes cluster

A Kubernetes cluster runs one Thorium deployment. Node labels, node provisioning, and the
cluster-scoped resources are shared, so the operator manages only the oldest `ThoriumCluster` it
can see and reports `Error` on any other; the thorium chart and minithor refuse to install a
second deployment. To run two deployments, use two Kubernetes clusters (for example two minithor
profiles). Run a single operator per Kubernetes cluster: operators each limited to their own
namespace (`watchOwnNamespaceOnly`) can't see each other's `ThoriumCluster`s, so they can't
enforce this between themselves, and that setup is unsupported. The chart's install guard covers
Helm installs across namespaces.

## The Helm charts

| Chart | Installed | Contents |
|-------|-----------|----------|
| `infra-operators` | Once per cluster, into `infra-operators` | The Scylla operator, the Elastic Cloud on Kubernetes (ECK) operator, and Kubegres (Postgres) |
| `thorium` | Once per cluster, into the thorium namespace | An umbrella over four subcharts: `secrets` (namespaces and credentials), `infra` (the backing services), `quickwit` (trace storage), and `operator` (the operator, its RBAC, the `ThoriumCluster`, ingress, and a toolbox import) |

The `global.managed.<service>` values (`scylla`, `elastic`, `redis`, `s3`, `postgres`) choose
which backing services the thorium chart deploys, and `global.quickwit.enabled` turns tracing
(Quickwit, its Postgres metastore and bucket, and Jaeger) on or off. Both charts are published as
OCI charts under `oci://ghcr.io/cisagov/thorium/charts`.

## Namespaces and the namespace prefix

Thorium is installed into the namespace `thorium`, or `<prefix>-thorium` to give every Thorium
namespace the prefix `<prefix>-`. The prefix is a lowercase DNS label of at most 32 characters;
the charts derive it from the release namespace, megathor sets it with `namespace_prefix`, and
minithor with `--namespace-prefix`. The other deployment pages call this namespace the
**thorium namespace**.

| Thorium namespace | Namespaces used |
|-------------------|-----------------|
| `thorium` | `thorium`, `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, `jaeger` |
| `dev-thorium` | `dev-thorium`, `dev-redis`, `dev-scylla`, `dev-elastic`, `dev-seaweedfs`, `dev-quickwit`, `dev-jaeger` |

A prefix only renames the namespaces; it doesn't make a second deployment on the same
Kubernetes cluster possible (see [One deployment per Kubernetes cluster](#one-deployment-per-kubernetes-cluster)).
Namespaces of backing services the chart doesn't deploy are skipped.

The k8s scaler creates one namespace per Thorium group, labelled
`app.kubernetes.io/managed-by: thorium-scaler`. Thorium never uses any of the namespaces above,
`infra-operators`, the Kubernetes system namespaces, or its built-in defaults for a group: the
chart renders them into `thorium.namespace_blacklist` (see
[Namespaces](./helm-configuration.md#namespaces)).

## Configuration and credentials

Thorium's components read one config file, `thorium.yml` (the format of
`api/thorium-template.yml` in the source repository). In a Kubernetes deployment it is assembled
by the operator:

- `spec.config` in the `ThoriumCluster` holds the non-secret settings. The Helm chart generates
  it from its values.
- `spec.config_secrets` names Secrets in the thorium namespace that hold partial `thorium.yml`
  documents with the credentials. The operator merges them over `spec.config` in order as JSON
  merge patches (RFC 7386): maps merge key by key, lists and other values replace, and `null`
  deletes a key. YAML tags (such as `!Grpc`), non-string map keys, and merge keys (`<<`) aren't
  supported, and values aren't coerced (a quoted `"5"` stays a string).
- The rendered result is written to the `thorium` Secret (key `thorium.yml`) and mounted by every
  component.

The thorium chart generates every credential it isn't given on the first install and keeps them
in the `thorium-credentials` Secret; later upgrades read them back, so they never change. The
most important one is Thorium's secret key (`thorium.secret_key`), which peppers every user's
password hash: **changing it locks out every user**, including the operator's own. See
[Credentials](./helm-configuration.md#credentials).

## Revisions

Thorium versions its deployment layout separately from its software version. A **revision**
(`YYYY-MM-vNN`, such as `2026-10-v02`) names a point in that history, and each revision lists the
steps that bring a deployment from the revision before it. The operator records the revision a
deployment is at in the `thorium-upgrade-state` ConfigMap in the thorium namespace.

| Revision | Meaning |
|----------|---------|
| `2026-10-v01` | A deployment made by the minithor or megathor scripts before the Helm charts; the baseline every converted deployment starts from |
| `2026-10-v02` | A deployment made by the Helm charts with the chart-managed operator |

When an operator knows a newer revision than the deployment's, it waits in `UpgradeRequired`
until an admin sets a target revision, then runs the steps in order. The Thorium version a
revision was released with is advisory only. See [Upgrading Thorium](./upgrades.md).

## Who may edit a ThoriumCluster

Anyone who can create or edit a `ThoriumCluster` controls the images, commands, arguments, and
environment of every component the operator deploys, and the endpoints the operator sends
credentials to. The k8s scaler reads Secrets cluster-wide, and node provision pods mount host
paths as root. Permission to edit a `ThoriumCluster` is therefore roughly equivalent to
cluster-wide Secret read plus root on the nodes: grant it like `cluster-admin`.
