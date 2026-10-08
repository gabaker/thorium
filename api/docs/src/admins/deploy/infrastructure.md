# Bring Your Own Infrastructure

Thorium needs ScyllaDB, Redis, Elasticsearch, and S3-compatible storage. Tracing is optional
and uses Quickwit, Postgres for Quickwit's metastore, and Jaeger. By default the `thorium` Helm
chart deploys all of these for you. Use the pages in this section when you run one or more of
them yourself:

- with the Helm chart, after turning the service off with its `global.managed.<service>: false`
  toggle (see [External services](./helm-configuration.md#external-services)), or
- when you [deploy the operator without Helm](./deploy-thorium.md), where every backing service
  is yours to provide.

A managed cloud service works as well as one you install yourself. These pages show one tested
way to run each service on Kubernetes and list the settings Thorium depends on.

You can install the `infra-operators` chart on its own to get the operators these pages use
(the Scylla operator, Elastic Cloud on Kubernetes (ECK), and Kubegres), even if you don't use
the `thorium` chart. Turn off any operator you don't need:

```bash
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait \
  --set kubegres.enabled=false
```

`$VERSION` is the Thorium release you deploy (see [Install with Helm](./deploy-helm.md)).

## What Thorium needs from each service

| Service | Thorium needs | Notes |
|---------|---------------|-------|
| Scylla | Contact points (`scylla.nodes`) and a role Thorium logs in as (`scylla.auth`) | The role must be a superuser, since the API creates its own keyspace with `scylla.replication`. Create the role yourself, or have the operator create it from a superuser login (`bootstrap.scylla`). See [Scylla](./install-scylla.md) |
| Redis | Host, port, and the password Redis requires (`redis.password`) | See [Redis](./install-redis.md) |
| Elasticsearch | The URL (`elastic.node`), a user (`elastic.username`/`password`), and a way to validate its certificate | The user needs `view_index_metadata`, `write`, and `read` on the four indexes in `elastic.results` and `elastic.tags` (`thorium_sample_results`, `thorium_repo_results`, `thorium_sample_tags`, `thorium_repo_tags` by default), plus `create_index` on any of them that doesn't exist yet. `all` on `thorium*` covers the defaults. Running the search streamer with `--reindex` also needs `delete_index`. Don't create the indexes yourself: the search streamer creates them with the right mappings. See [Elasticsearch (ECK)](./install-elastic.md) |
| S3 | The endpoint, region, path-style setting, and an access key and secret | The operator creates Thorium's buckets if they are missing. With `thorium.s3.skip_bucket_auto_create: true` it creates nothing and only checks that every bucket exists, so create them first. See [Rook](./install-rook.md) for an in-cluster object store |
| Quickwit (optional) | An S3 bucket for traces (`quickwit` by default) and a Postgres database for the metastore | Nothing in Thorium creates the bucket or the database outside the chart's own SeaweedFS and Postgres. See [Tracing](./install-tracing.md) |

How you pass these settings to Thorium depends on how you deploy it:

- **Helm chart:** set the endpoints under `operator.backends` and the credentials under
  `secrets.credentials`, as described in
  [External services](./helm-configuration.md#external-services).
- **Operator without Helm:** put the credentials in a config Secret and the endpoints in the
  `ThoriumCluster`'s `spec.config`, as described in
  [Config Secret](./deploy-thorium.md#config-secret).

Either way, the operator checks every backend with Thorium's own credentials before it writes
Thorium's config, and reports a rejected login or a missing privilege in the `ThoriumCluster`'s
status.

## Versions

The `thorium` and `infra-operators` charts are tested with these versions. Other versions may
work, but these are the ones a chart release deploys.

| Component | Version |
|-----------|---------|
| ECK operator | 2.16.0 |
| Elasticsearch and Kibana | 8.19.2 |
| Scylla operator | 1.21.1 |
| ScyllaDB | 6.2.3 |
| Scylla Manager agent | 3.4.2 |
| Redis | 7.4.11 |
| SeaweedFS | 4.48 |
| Quickwit | chart 0.8.17, image v0.8.2 |
| Jaeger query | 1.76.0 |
| Kubegres | 1.19 |
| Postgres | 17 |
| Rook (installed by megathor) | Helm charts v1.18.1 |
| Traefik (installed by megathor) | Helm chart 37.1.0 |

To list the exact images a chart release deploys, run `deploy/charts/scripts/list-images.sh`
from the Thorium source repository at that release, passing the values files you deploy with.

## Namespaces

Services you run yourself can live in any namespace, or outside the cluster. The chart's own
services use `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, and `jaeger` (with the
namespace prefix, if any). Thorium never creates a group namespace with a name in its namespace
blacklist, which includes those names and Thorium's built-in defaults (`thorium`, `scylla`,
`scylla-operator`, `cert-manager`, `redis`, `elastic-system`, `jaeger`, `quickwit`). If your
services use other namespaces that a Thorium group name could match, add them to the blacklist
(`operator.cluster.namespaceBlacklist` with Helm, or `thorium.namespace_blacklist` in the
config).
