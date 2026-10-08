# Operator

The Thorium operator deploys and maintains Thorium inside Kubernetes. It watches a
`ThoriumCluster` custom resource, which describes the Thorium components to run and the
non-secret part of Thorium's config, and turns it into the Secrets, Services, and Deployments
the components need. It also prepares Thorium's backends, provisions the nodes Thorium schedules
jobs on, and moves the deployment between upgrade revisions. The Helm chart runs it as the
`operator` Deployment in the thorium namespace, from the same image as every other component.

## What it watches

- **`ThoriumCluster` resources**, in its own namespace or every namespace (`--namespace`). It
  manages only the oldest one in the Kubernetes cluster, since Thorium supports one deployment per
  Kubernetes cluster.
- **The Secrets and ConfigMaps a cluster reads** (its config, bootstrap, and CA Secrets, the
  `kube-config` Secret, and the `banner` ConfigMap), by metadata only, so a change to one
  reconciles the cluster.
- **Component Deployments** (`api`, `scaler`, `baremetal-scaler`, `event-handler`,
  `search-streamer`): a change in their availability, or a deleted one, reconciles the cluster.
- **Nodes**: each node the k8s scaler may schedule on gets a provision pod that installs the
  Thorium agent and is labelled `thorium=enabled` with its `thorium_version`. The node watcher
  checks each node when it changes and every 15 minutes: it provisions and labels a ready node
  that has no `thorium` label yet, and replaces the provision pod when the API reports a new
  version. It leaves a node that is already labelled with the current version alone, so a node
  that returns after going down gets a deleted provision pod back on the next cluster reconcile
  instead. Provision pods themselves aren't watched.
- **API pods**: exactly one live API pod is labelled `mcp=enabled` to serve MCP queries through the
  `thorium-mcp` Service.

## How it reconciles a cluster

1. **Gate.** Refuse an unconverted pre-Helm cluster or a second cluster, record the cluster's
   upgrade revision the first time, and hold the cluster in `UpgradeRequired` until a target is
   set when the operator knows a newer revision. With a target set, run the upgrade steps
   (`Upgrading`).
2. **Render the config.** Merge the `config_secrets` over `spec.config` and validate the result.
3. **Prepare and check the backends.** Create Thorium's S3 buckets, run the privileged bootstrap
   steps when their inputs changed (Thorium's Scylla role, an external Elasticsearch's user), and
   check that Elasticsearch, Scylla, and Redis accept Thorium's own credentials. A backend that
   isn't up yet keeps the cluster in `Provisioning`; a rejected credential is an `Error`.
4. **Deploy the API.** Write the checked config to the `thorium` Secret, create the
   `thorium-api` and `thorium-mcp` Services and the `api` Deployment, and wait for the API to
   answer.
5. **Create users and keys.** The operator's own user, the initial admin user (once), and the
   `thorium` and `thorium-kaboom` users whose keys the other components mount.
6. **Deploy everything else.** System settings, the scalers, the event handler, and the search
   streamer.
7. **Provision nodes.** Node provision pods, then labels and registration in Thorium. A node that
   can't be provisioned is reported without blocking the components above; nodes that are down
   are skipped (keeping any labels they have) until they return.
8. **Report.** `Ready` once every Deployment has rolled out, with any non-fatal problems in the
   message, or `Error` listing nodes that failed to provision. The operator watches its component
   Deployments, so a component that becomes unavailable later (for example on a node that went
   down) moves the cluster back to `Provisioning` until it recovers.

Deleting a `ThoriumCluster` runs the operator's cleanup before its finalizer is removed: node
labels, provision pods, and the resources it created are deleted, while databases and their data
are kept.

## Revisions

Each deployment records the upgrade revision it is at in the `thorium-upgrade-state` ConfigMap.
The operator ships a catalog of revisions and the steps between them; it changes nothing on a
cluster behind its latest revision until an admin sets a target, and steps that change data need
an approval naming a backup. See [Upgrading Thorium](../admins/deploy/upgrades.md).

## Learn more

- [Deployment Concepts](../admins/deploy/concepts.md)
- [ThoriumCluster and Operator Reference](../admins/deploy/thoriumcluster.md)
- [Troubleshooting Deployments](../admins/deploy/troubleshooting.md)
