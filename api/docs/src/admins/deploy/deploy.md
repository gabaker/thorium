# Deploying Thorium

Thorium runs on Kubernetes. A deployment consists of the Thorium components (API, scaler, event
handler, search streamer, and the agents on each worker node), the backing services they store
data in (ScyllaDB, Redis, Elasticsearch, and an S3-compatible object store, plus optional
tracing), and the Thorium operator, which deploys and upgrades the components from a
`ThoriumCluster` resource. [Deployment Concepts](./concepts.md) explains how these fit together.

## Choose a path

| Path | Use it for | Page |
|------|------------|------|
| Helm charts | Any Kubernetes cluster: on premises, cloud, or GitOps (Argo CD, Flux) | [Install with Helm](./deploy-helm.md) |
| megathor | Production clusters on bare metal or VMs: prepares Rook/Ceph storage and Traefik, then installs the Helm charts with Ansible | [Production Clusters (megathor)](./megathor.md) |
| minithor | A laptop or workstation: development, tool development, and testing on Minikube with one or more nodes | [Development Clusters (minithor)](./minithor.md) |
| The operator without Helm | Sites that can't use Helm, or a pre-Helm deployment that stays outside Helm | [Deploy the Operator Without Helm](./deploy-thorium.md) |
| Your own backing services | External ScyllaDB, Elasticsearch, Redis, S3, or Postgres with any of the paths above | [Bring Your Own Infrastructure](./infrastructure.md) and [External services](./helm-configuration.md#external-services) |

megathor and minithor both install the same Helm charts, so everything on the Helm pages also
applies to them.

## Already running Thorium?

- **Upgrading** a deployment made with the Helm charts, megathor, or minithor: see
  [Upgrading Thorium](./upgrades.md).
- **A deployment made by the minithor or megathor scripts before the Helm charts** (a
  `ThoriumCluster` usually named `dev` with every credential in its spec) must be converted
  before a current operator, chart, minithor, or megathor manages it. The operator leaves such a
  deployment untouched and reports that it "has not been converted". Convert it to Helm with
  [Converting a Pre-Helm Deployment](./convert-to-helm.md), or keep it outside Helm with
  [Moving a pre-Helm deployment without Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm).

## Reading order

For a new deployment, read the pages in this order:

1. [Deployment Concepts](./concepts.md): components, backing services, the operator, namespaces,
   credentials, and revisions.
2. [Planning a Deployment](./planning.md): cluster prerequisites, sizing, storage, ingress, and
   offline installs.
3. The install page for your path (see the table above).
4. [Configure the Helm Charts](./helm-configuration.md): credentials, external services,
   certificates, ingress, proxies, and pod security.
5. [Operating a Deployment](./operate.md): status, configuration changes, backups, and
   uninstalling.
6. [Upgrading Thorium](./upgrades.md).
7. [Troubleshooting Deployments](./troubleshooting.md) when something doesn't reach `Ready`.

[ThoriumCluster and Operator Reference](./thoriumcluster.md) and
[Chart Values Reference](./chart-values.md) list every setting.
