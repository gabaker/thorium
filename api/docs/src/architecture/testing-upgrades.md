# Testing Upgrades and Schema Changes

This page is for developers who change Thorium's schema, add an upgrade revision or a data
migration, or touch the operator's upgrade code. It describes the levels at which that work is
tested, the backends each level needs, and how to run the tests that continuous integration
doesn't.

## Levels

| Level | What it runs | Needs |
|-------|--------------|-------|
| Unit | The revision catalog's checks, the planner, the loss gate, and the schema snapshots (`cargo test -p thorium-api --lib`, with `--features upgrades-runtime` for the planner and gate logic), thoradm's library tests, and the operator's tests against an in-process fake Kubernetes API (`cargo test -p thorium-operator`) | Nothing |
| Integration | The schema, migration, backup, and restore tests (`cargo test -p thorium-api --features test-utilities --test schema_apply`, `cargo test -p thoradm --test migrate_runner`, and the others) | A Scylla, a Redis, and S3, and for some tests an Elasticsearch |
| Direct driver | Upgrade scenarios run in one process: the planner, the gates, and `thoradm migrate`'s library code run against real backends, with no Kubernetes (`cargo test -p thorium-api --features test-utilities --test upgrade_harness --test upgrade_matrix`) | A Scylla, a Redis, and S3 |
| Live | The same scenarios on a development cluster, through the operator and its Jobs, with images built from the branch | A minithor cluster and the images |

The direct driver builds each scenario from a fixture catalog, seeds objects in the previous
layout, applies the approvals the scenario names, and verifies the objects, the history, and the
per-pass counts. It covers the approval gates, single steps, lossy confirmations, optional-field
variants, and reason-coded corruptions. The pattern matrix runs a fixture catalog that mirrors each
kind of schema change through it.

## Backends in continuous integration

Every job has its own Scylla and Redis, which end with the job, so tests inside a job share them and
no test can touch another pipeline's data. Elasticsearch and S3 are shared by every active
pipeline, so each run names what it creates uniquely (a run id with an epoch-minute stamp), each job
makes its own buckets and deletes them when it ends, and a janitor deletes any bucket a cancelled job
left behind by the stamp in its name. No CI test creates an Elasticsearch index.

A job can't restart, pause, or change its Scylla, and has no Docker. So a test that needs that, and a
test that creates Elasticsearch indexes, is marked `#[ignore = "<command>"]` and belongs to the manual
tier below, and a test that runs for hours runs in the nightly pipeline. Tests that need a namespace
use a random one by default, and fail in CI when the harness can't build one.

## The manual tier

The manual tier is a set of ignored tests. Each test's `#[ignore]` reason holds the command to run
it. Run them yourself on a local Scylla (and Elasticsearch, Redis, and S3 where noted) before you
merge a change to the code they cover; nothing checks that you did.

| Test | Environment | Command |
|------|-------------|---------|
| The `schema_views` restart tests | A disposable Docker Scylla container (the tests restart it) | `THORIUM_TEST_SCYLLA_CONTAINER=<container> cargo test -p thorium-api --features test-utilities --test schema_views -- --ignored` |
| `migrate_cli::a_paused_scylla_is_retried_through` | A disposable Docker Scylla container (the test pauses it for 40 seconds) | `THORADM_TEST_DOCKER_SCYLLA=<container> cargo test -p thoradm --test migrate_cli -- --ignored a_paused_scylla_is_retried_through` |
| The one-million-row view build | A Scylla with room for the build; also runs nightly | `cargo test -p thorium-api --features test-utilities --test schema_apply -- --ignored apply_waits_for_a_new_view_to_build` |
| The Elasticsearch probe test | Your own Elasticsearch, where you may create and delete indexes | `cargo test -p thorium-api --features test-utilities --test schema_probe -- --ignored the_elastic_probe_observes_each_configured_index` |
| The `schema_diff_cli` tests | Your own Elasticsearch, as above | `cargo test -p thoradm --test schema_diff_cli -- --ignored` |
| The Elastic rebuild against a real Elasticsearch | A throwaway cluster: point `THORIUM__ELASTIC__*`, `THORIUM__REDIS__*`, and the Scylla and S3 overrides at it | `cargo test -p thoradm --test migrate_elastic -- --ignored` |
| The cluster driver | A minithor cluster and two local images | See below |

The five `redis_flush` tests in `thoradm/tests/redis_flush.rs` flush a whole Redis database. They
refuse to run unless `THORADM_TEST_FLUSH_REDIS=<host:port>` equals the Redis the configuration
points at, so run them only against a Redis nothing else uses; CI sets the variable to the job's own
Redis. The API test harness also flushes the Redis it is configured for, so don't point it at a shared
development Redis; it refuses a namespace that doesn't start with `testing_`.

## The live level

The live scenarios run on a minithor cluster with the previous release's image and an image built
from your branch (`minithor deploy --local-image`). Each starts from a fresh install at the
previous revision with seeded data. The scenarios to run when you change a revision, a migration, or
the operator's upgrade code:

1. **Upgrade.** A fresh install at the previous revision with seeded data upgrades to the new one:
   an additive change runs by itself, an Elasticsearch rebuild and a data migration wait for an
   approval, a migration with corrupted objects fails on the first group over its limit and
   finishes after you fix the objects and run `thoradm migrate run --id <migration> --reason
   <code>` or accept the code in `acceptIssues`, and a migration interrupted by killing its pod
   resumes.
2. **Pre-release round trip.** (A feature branch's catalog only: the shipped catalog has no
   pre-release revision.) The base revision to a pre-release revision, back with the branch
   image (`thoradm migrate revert --to <base>` or a lowered `targetRevision`), to the base with the
   main image, and to the pre-release again, with the data intact.
3. **Drift.** Drop a materialized view, delete an Elasticsearch index, and add a column by hand.
   `thoradm schema diff` and the `SchemaDrift` condition report them, and the next schema apply
   repairs the additive part.
4. **Downgrade.** Upgrade, write new data, approve and run the reverse steps with the newer
   operator, deploy the older operator and images, and check that the older release reads
   everything, including what was written after the upgrade. Scale the paused components back by
   hand when the older operator predates the paused-component counts.
5. **Partial upgrade.** Stop an upgrade after some steps and revert it; only the finished steps
   are reversed.
6. **From the first Helm release.** Convert a deployment of the first Helm release (the chart and
   images over a keyspace shaped like release 1.1.1) with `convert-to-helm.sh`, upgrade it to
   `2026-10-v02`, and then to the current release. The record is seeded from the ConfigMap, the
   first schema apply finds nothing missing, the API starts, and no step already done runs again.
7. **Back to the first Helm release.** From the current release, run the reverse to `2026-10-v02`,
   redeploy the first Helm release's operator, and check that it parses the `ThoriumCluster` and
   the ConfigMap the newer operator left in hand-over mode and reconciles; scale the paused
   components back by hand to the counts the status showed.

The cluster driver runs the example migration as one scenario:

```bash
THORIUM_TEST_CONFIG=<the cluster's thorium.yml, with its Redis reachable> \
THORIUM_TEST_PREVIOUS_IMAGE=<previous release image> THORIUM_TEST_NEXT_IMAGE=<next image> \
THORIUM_TEST_NAMESPACE=thorium THORIUM_TEST_CLUSTER=thorium \
[THORIUM_TEST_DEPLOY_COMMAND="<deploy command with {image}>"] \
cargo test -p thorium-api --features test-utilities --test upgrade_cluster -- --ignored
```

The driver runs `kubectl` and a deploy command, so both must be on the `PATH`. The default deploy
command is `minithor deploy --local-image {image}`; set `THORIUM_TEST_DEPLOY_COMMAND` when the
deploy needs a chart or a profile (`{image}` stands for the image, and each image needs the chart
of its own release). The images must be loaded into the cluster's nodes, which
`minithor deploy --local-image` does. The previous image must write the layout the example's
migration converts (the development image that stores a user's theme in its own field), and the
scenario reads every `user_data:*` key of the cluster's Redis, so a cluster holding many users
makes it slow. A deploy that ends with an upgrade step waiting for an approval exits non-zero in
`minithor`; the driver lets that refusal through and the scenario asserts the block next.

After a live run, check the databases too: `thoradm schema diff` exits 0 (or reports only
acknowledged objects), `thoradm migrate issues --id <migration>` lists nothing unexpected, and
`thoradm schema status` shows the revision you expect.
