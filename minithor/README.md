
# Overview

Minithor utilizes Minikube to provide a local Kubernetes instance with minimal custom configuration required. Such an instance is useful for development and testing of Thorium as well as small stand alone analyst fly-away kits where external network access may not be available. Minithor deployments are not highly available distributed systems like our production instances and provides minimal redundancy. The Thorium deployment produced by following these instructions should be considered Beta. We will work to improve its stability over time. While a Minithor deployment is accessible only from your localhost, all backing-service passwords are randomly generated per deploy. The default admin user password can be randomized with `--rand-password`.

### Requirements

To deploy Minithor, you will need a container runtime such as that provided by the docker engine or podman. KVM/libvirt (`kvm2` driver) is also supported on Linux. Minithor automatically selects the best available driver in order of preference: podman > docker > kvm2. Minithor also requires a relatively beefy machine, with 16+ GiB of memory, 8+ CPUs, and 100GiB of local storage.

## Usage

All lifecycle operations are available through the `minithor` CLI. The script reads input files from your current working directory by default and can be installed globally (e.g. `/usr/local/bin/minithor`).

```
minithor — manage a local Thorium instance on Minikube

Usage: minithor <command> [options]

Commands:
  minikube         Manage the minikube cluster (install, delete)
  start            Start a previously stopped minikube cluster
  deploy           Deploy all Thorium services and backing infrastructure
  get-config       Extract the running Thorium config to ~/thorium.yml
  expose           Port-forward Thorium (and optionally backing services) to localhost
  stop             Stop the minikube cluster (preserves state)
  cleanup          Remove all Thorium resources for a fresh deploy (requires --confirm)

Global options:
  --instance <name>  Operate on a named Thorium instance (namespaces prefixed with "<name>-")
  --profile <name>   Operate on a separate minikube cluster (profile) instead of "minikube"
  -h, --help         Show this help message

Run 'minithor <command> --help' for command-specific options.
```

### Working directory layout

When running `minithor` commands, the following files are looked for in your current working directory:

| File | Used by | Required? |
|------|---------|-----------|
| `thorium-cluster.yml` | `deploy` | No — cluster config (a built-in default is used if absent) |
| `.dockerconfigjson` | `deploy` | No — private registry credentials for pulling images |
| `banner.txt` | `deploy` | No — login banner (a default is generated if absent) |

All paths can be overridden with CLI flags — run `minithor <command> --help` for details.

### Install Minikube

Install and start minikube and any necessary plugins.

```bash
minithor minikube install [--nodes <n>] [--cpus <n> | --node-cpus <n>] [--memory <n> | --node-memory <n>] [--certs <path>] [--force-node-config]
```

| Flag | Description |
|------|-------------|
| `--nodes <n>` | Number of Kubernetes nodes to create (default: 1) |
| `--cpus <n>` | Total CPUs for the whole cluster, split evenly across the nodes and rounded down (default: 8). Unlike minikube's own `--cpus`, this is a cluster total |
| `--memory <n>` | Total memory in GiB for the whole cluster, split evenly across the nodes (default: 16) |
| `--node-cpus <n>` | CPUs for each node instead of splitting `--cpus` (conflicts with `--cpus`) |
| `--node-memory <n>` | Memory in GiB for each node instead of splitting `--memory` (conflicts with `--memory`) |
| `--certs <path>` | A `.crt` file, or a directory of `*.crt` files, to install into the minikube node's trust store. Required behind a TLS-intercepting proxy so the node trusts it for image pulls. No default — cert install only runs when this flag is given. |
| `--force-node-config` | Overwrite the node's CA and container-runtime (dockerd or containerd) proxy drop-in even if they already exist. By default existing files are left in place so a manual fix on the node isn't clobbered. |

Detects the best available driver (podman > docker > kvm2), downloads minikube for your OS/architecture, configures resource limits, and starts the cluster with Calico CNI, CSI, and Ingress addons. When the host has an HTTP proxy configured (see below), the node's container runtime is also configured to pull images through it.

#### Multi-node clusters

`--nodes <n>` creates a cluster with `n` nodes (`minikube`, `minikube-m02`, ...) so larger deployments can be tested. `--cpus` and `--memory` are totals for the whole cluster and are split evenly across the nodes (for example `--nodes 3 --cpus 15 --memory 48` gives each node 5 CPUs and 16 GiB); use `--node-cpus`/`--node-memory` to size each node directly instead. Each node needs more than 2 CPUs and 2 GiB of memory because the Thorium scaler reserves that much on every node, and install refuses a split that leaves less. `minithor deploy` adds every node to the ThoriumCluster's scaler node list, which makes the operator label and register them so jobs are scheduled across all of them. Custom `thorium-cluster.yml` files can use `nodes: ${K8S_NODES}` to get the same list.

To try a multi-node cluster without touching an existing one, use a separate minikube profile:

```bash
minithor --profile multi minikube install --nodes 3 --cpus 15 --memory 48
minithor --profile multi deploy
minithor --profile multi minikube delete --confirm   # deletes only the "multi" cluster
```

### Create registry auth file (optional)

If the Thorium container image is hosted in a private registry, create a `.dockerconfigjson` file in your working directory containing the registry credentials. The deploy command will detect this file and create a Kubernetes image pull secret automatically.

```bash
docker login registry.domain:port
cp ~/.docker/config.json .dockerconfigjson
```

If omitted, the operator will pull images without authentication (works for public registries like `ghcr.io`).

### Proxy configuration (optional)

If your organization maintains a proxy for all traffic going to the internet, export proxy settings before running the `minikube install` command:

```bash
export HTTP_PROXY=<HTTP_PROXY_URL:PORT>
export HTTPS_PROXY=<HTTPS_PROXY_URL:PORT>
```

Or use the provided proxy file: `source proxy`

`HTTP_PROXY`/`HTTPS_PROXY` are taken from your environment as-is. You do **not** need to set `NO_PROXY` for the cluster to work — minithor automatically computes the full bypass list (the node subnet, service and pod CIDRs, control-plane host, loopback, and the cluster-internal registry/DNS suffixes) and unions it with any `NO_PROXY`/`no_proxy` you already have set. This computed value is used consistently across `install`, `start`, and `expose`.

When a proxy is configured, `minithor minikube install` (and subsequent `minithor start`) configures the minikube node's container runtime to pull external images through it, since minikube's own proxy propagation into the node is unreliable. If your proxy performs TLS interception, pass its CA to `minikube install` with `--certs <path>` so the node trusts it for image pulls:

```bash
minithor minikube install --certs /path/to/proxy-ca.crt
```

On an unproxied host, all of this proxy/cert plumbing is a no-op.

### Deploy

The `deploy` command handles the full deployment in a single step: all backing services (Redis, Elasticsearch, ScyllaDB, SeaweedFS, Postgres, Quickwit, Jaeger), the Thorium operator, the ThoriumCluster resource, and a default test user.

```bash
minithor deploy [options]
```

| Flag | Description |
|------|-------------|
| `--config <path>` | Path to `thorium-cluster.yml` (default: `$PWD/thorium-cluster.yml`; a built-in default is used if absent) |
| `--docker-config <path>` | Path to `.dockerconfigjson` (default: `$PWD/.dockerconfigjson`) |
| `--banner <path>` | Path to `banner.txt` (default: `$PWD/banner.txt`, generates default if absent) |
| `--bin <path>` | Directory for downloaded binaries like thorctl (default: `/usr/local/bin`) |
| `--toolbox <path>` | Download location for `toolbox.json` (default: `$PWD/toolbox.json`) |
| `--user <name>` | Username for the initial admin user (default: `test`) |
| `--password <pass>` | Password for the initial admin user (default: `INSECURE_DEV_PASSWORD`) |
| `--rand-password` | Generate a random password for the admin user (saved and reused on redeploy) |
| `--registry` | Deploy a container registry (registry:2) in the thorium namespace with persistent storage |
| `--registry-user <name>` | Enable registry basic auth for this user (implies `--registry`, password is auto-generated and printed to stdout) |
| `--timeout <seconds>` | Timeout for each service rollout/wait (default: 600) |
| `--version <tag>` | Thorium image tag for the operator and ThoriumCluster (default: `latest`) |
| `--instance <name>` | Deploy a named instance alongside others, for deployment testing only (see [Multiple Instances](#multiple-instances)) |

This will:

1. Wait for the minikube cluster to be healthy
2. Install Helm and add required chart repos
3. Deploy Redis, Elasticsearch (ECK), cert-manager, ScyllaDB, SeaweedFS, Jaeger, Kubegres, and Quickwit
4. Configure databases (Scylla roles/keyspace, Elasticsearch index/user, the Quickwit bucket in SeaweedFS; the Thorium operator creates Thorium's own buckets)
5. Deploy the Thorium operator and create the ThoriumCluster CRD
6. Wait for all Thorium components (API, scaler, event-handler, search-streamer) to be running
7. Create an admin user (default: `test` / `INSECURE_DEV_PASSWORD`, customizable via `--user` / `--password` / `--rand-password`)
8. Install `thorctl` to the bin directory and import the default toolbox
9. Create a `static` group and an `allow-all` network policy
10. Optionally deploy a container registry with persistent storage (if `--registry` or `--registry-user` is specified)

All backing-service passwords (Redis, Scylla, Elasticsearch, SeaweedFS S3, Postgres, and the Thorium API secret key) are randomly generated on the first deploy using a cryptographically secure RNG and saved in the `minithor-credentials` secret (namespace `minithor`). Re-running `deploy` reuses them, so it is safe to run again after a timeout or to pick up script changes; each run also re-applies the saved password to Scylla's `thorium` role and checks that Postgres still accepts the saved password. A cluster deployed before credentials were saved has its existing passwords read back from the running deployment on the next deploy. `minithor cleanup --confirm` deletes the saved credentials along with the data, so the following deploy starts fresh. The passwords are not displayed during deployment but can be retrieved afterward with `minithor get-config`.

To customize the ThoriumCluster configuration, provide your own config file:

```bash
minithor deploy --config path/to/thorium-cluster.yml
```

Write backend credentials in the file as placeholders (`${THORIUM_SECRET}`, `${S3_AK}`, `${S3_SK}`, `${REDIS_PASS}`, `${SCYLLA_PASS}`, `${ES_PASS}`), and the namespace and service hosts as `${THORIUM_NAMESPACE}`, `${REDIS_HOST}`, `${SCYLLA_HOST}`, `${ELASTIC_HOST}`, `${S3_HOST}`, and `${QUICKWIT_INDEXER_HOST}`; deploy substitutes the real values before applying it, so one file works for any instance. See `thorium-cluster.yml.example`.

Operator and chart versions (ECK, cert-manager, the Scylla operator, the Quickwit chart) and the Redis and Jaeger images are pinned to megathor's defaults, so what is tested here matches what megathor deploys. If a shared operator is already installed at a different chart version (for example by an older minithor), deploy leaves it unchanged and prints a warning; `minithor cleanup --confirm` followed by a fresh deploy moves to the pinned versions.

### Access Thorium

Port-forward the Thorium API to localhost:

```bash
minithor expose [--dev] [--no-api] [--port <port>] [--port-offset <n>] [--stop] [--status]
```

| Flag             | Description                                                     |
|------------------|-----------------------------------------------------------------|
| `--dev`          | Also forward database ports (Elastic, Kibana, Redis, SeaweedFS, Scylla) |
| `--no-api`       | Forward only the backing services (skip API/registry); implies the database ports |
| `--port <port>`  | Local port for the Thorium API (default: 8080 + offset)         |
| `--port-offset <n>` | Add `<n>` to every local port so several instances can be exposed at once |
| `--stop`         | Stop all of your port-forwards (every instance) and remove the Scylla loopback aliases |
| `--status`       | Show which port-forwards are running                            |

The registry service (port 5000) is also forwarded when a registry has been deployed. `expose` exits non-zero if any forward fails to start. If a local port is already held by something other than one of your own stale port-forwards (a local Redis, another user's forward, ...), that forward is reported as failed and the other process is left alone. Port-forward logs and loopback alias state are kept per user in `~/.cache/minithor/` (or `$XDG_CACHE_HOME/minithor/`).

Then open http://localhost:8080 in your browser and log in with the credentials shown at the end of the deploy output (default: `test` / `INSECURE_DEV_PASSWORD`).

### Get Config

Extract the running Thorium config from the cluster:

```bash
minithor get-config            # writes the raw in-cluster config to ~/thorium.yml
minithor get-config --local    # writes a config for host access via 'expose --dev' to ~/thorium.local.yml
```

Both files contain every backend credential and are written readable only by you.

### Multiple Instances

`--instance <name>` (or `MINITHOR_INSTANCE=<name>`) selects a named Thorium instance for `deploy`, `expose`, `get-config`, and `cleanup`. A named instance puts each component in its own `<name>-` prefixed namespace (`<name>-thorium`, `<name>-redis`, `<name>-scylla`, `<name>-elastic`, `<name>-quickwit`, `<name>-seaweedfs`, `<name>-jaeger`, `<name>-minithor`), so several instances can run side by side.

> **Deployment testing only.** Running more than one Thorium instance in a single Kubernetes cluster is a developer feature for testing deployment tooling. It is not a supported production configuration: the scaler does not support sharing a cluster with another Thorium instance (each instance's scaler schedules into the same group-named job namespaces).


```bash
minithor deploy                                  # default instance
minithor --instance b deploy                     # a second instance
minithor --instance b expose --dev --port-offset 100   # API on :8180, Elastic on :9300, ...
minithor --instance b get-config --local --port-offset 100   # ~/thorium-b.local.yml
minithor --instance b cleanup --confirm          # removes only instance b
```

All instances share the cluster-wide operators (ECK, the Scylla operator, cert-manager, Kubegres) and a single Thorium operator, which manages every ThoriumCluster; the first instance deployed runs it and later instances reuse it. `cleanup` removes the shared operators and CRDs only with the last instance, and refuses to remove the instance running the Thorium operator while others remain unless `--force` is given. Each instance's scaler creates job namespaces named after Thorium groups (such as `static`), so instances using the same group names schedule into the same namespaces; keep instances apart when testing scheduling. The in-cluster registry (`--registry`) is only available to the default instance, and a named instance's ingress host is `<name>.localhost`.

### Stop

Stop the minikube cluster while preserving its state:

```bash
minithor stop
```

Resume later with `minithor start`.

### Start

Restart a previously stopped cluster:

```bash
minithor start
```

### Cleanup

Remove all deployed Thorium resources and backing services for a fresh deploy. The `--confirm` flag is required:

```bash
minithor cleanup --confirm
```

This removes all namespaces, Helm releases, CRDs, cluster-level RBAC, and the saved credentials created by the deploy step. Individual step failures are logged but do not abort the cleanup. `--timeout <seconds>` sets the timeout for each delete/wait (default: 300). The minikube cluster itself is preserved.

### Delete Minikube

Completely remove minikube, its data, and associated binaries. The `--confirm` flag is required:

```bash
minithor minikube delete --confirm
```

This runs `minikube delete --all --purge`, removes `~/.minikube` and `~/.kube`, uninstalls the minikube and kubectl binaries, and prunes unused container images. This is irreversible.
