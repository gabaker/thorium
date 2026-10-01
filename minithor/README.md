
# Overview

Minithor utilizes Minikube to provide a local Kubernetes cluster with minimal custom configuration required, and deploys Thorium onto it with the Thorium Helm charts (`deploy/charts`). Such a deployment is useful for development and testing of Thorium and its tools. Minithor deployments are not highly available distributed systems like our production deployments and provides minimal redundancy. The Thorium deployment produced by following these instructions should be considered Beta. We will work to improve its stability over time. While a Minithor deployment is accessible only from your localhost, all backing-service passwords are randomly generated on the first deploy and kept afterward. The default admin user password can be randomized with `--rand-password`.

### Requirements

To deploy Minithor, you will need a container runtime such as that provided by the docker engine or podman. KVM/libvirt (`kvm2` driver) is also supported on Linux. Minithor automatically selects the best available driver in order of preference: podman > docker > kvm2. Minithor also requires a relatively beefy machine, with 16+ GiB of memory, 8+ CPUs, and 100GiB of local storage. `curl` and `tar` are needed; Helm 3.8+ is used from your PATH or downloaded (pinned and checksum-verified) into `~/.cache/minithor/bin/`.

Minithor is a single script: run it from a checkout, or download just the file:

```bash
curl -fsSL https://raw.githubusercontent.com/cisagov/thorium/main/minithor/minithor -o minithor
chmod +x minithor
```

## Usage

All lifecycle operations are available through the `minithor` CLI.

```
minithor — manage a local Thorium deployment on Minikube with the Thorium Helm charts

Usage: minithor [global options] <command> [options]

Commands:
  minikube         Manage the minikube cluster (install, delete)
  start            Start a previously stopped minikube cluster
  deploy           Deploy Thorium (and the cluster-wide operators if missing)
  credentials      Print Thorium's admin (and optionally every backend) credential
  get-config       Extract the running Thorium config to ~/thorium.yml
  expose           Port-forward Thorium (and optionally backing services) to localhost
  stop             Stop the minikube cluster (preserves state)
  cleanup          Remove Thorium and its data (requires --confirm)

Global options (may appear anywhere on the command line):
  --namespace-prefix <prefix>
                     Prefix Thorium's namespaces with "<prefix>-" (see Namespace Prefix)
  --profile <name>   The minikube profile to use (default: minikube; also MINITHOR_PROFILE or MINIKUBE_PROFILE)
  -h, --help         Show this help message
```

Every command only touches the selected minikube profile's cluster (`--profile`), so other clusters in your kubeconfig are never used by accident.

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

`--nodes <n>` creates a cluster with `n` nodes (`minikube`, `minikube-m02`, ...) so larger deployments can be tested. `--cpus` and `--memory` are totals for the whole cluster and are split evenly across the nodes (for example `--nodes 3 --cpus 15 --memory 48` gives each node 5 CPUs and 16 GiB); use `--node-cpus`/`--node-memory` to size each node directly instead. Each node needs more than 2 CPUs and 2 GiB of memory because the Thorium scaler reserves that much on every node, and install refuses a split that leaves less. The ThoriumCluster's scaler node list is empty by default, which makes the operator label and register every node so jobs are scheduled across all of them.

To try a multi-node cluster without touching an existing one, use a separate minikube profile:

```bash
minithor --profile multi minikube install --nodes 3 --cpus 15 --memory 48
minithor --profile multi deploy
minithor --profile multi minikube delete --confirm   # deletes only the "multi" cluster
```

### Private Thorium image (optional)

If the Thorium container image is hosted in a private registry, pass its credentials to deploy and they are stored as the image pull secret:

```bash
docker login registry.domain:port
minithor deploy --docker-config ~/.docker/config.json
```

Without it, images are pulled without authentication (works for public registries like `ghcr.io`).

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

`minithor deploy` also gives the Thorium scaler the proxy (it reaches registries such as ghcr.io), set as the scaler's `http_proxy`/`https_proxy`/`no_proxy` environment. It defaults to the host's `HTTPS_PROXY`/`HTTP_PROXY`; pass `--proxy <url>` to use a different one or `--proxy none` to leave the scaler unproxied.

The scaler's `no_proxy` lists only the cluster's own names and ranges: `localhost`, `127.0.0.1`, `.svc`, `.svc.cluster.local`, `cluster.local`, `control-plane.minikube.internal`, the profile's service and pod CIDRs (read from the profile's saved minikube config, otherwise minikube's defaults `10.96.0.0/12` and `10.244.0.0/16`), the profile's node subnet, and the in-cluster registry host. Your host's `NO_PROXY`/`no_proxy` is not copied into it, so anything else the scaler talks to goes through the proxy; append extra entries with `--no-proxy <list>` (comma-separated). `deploy` also sets the chart's `global.clusterCIDRs` to the same service and pod CIDRs, which the chart adds to the operator's `noProxy` when you give the operator a proxy (`operator.operator.proxy`).

On an unproxied host, all of this proxy/cert plumbing is a no-op.

### Deploy

The `deploy` command installs the `infra-operators` chart (the Scylla, ECK, and Kubegres operators, once per cluster; operators that already run elsewhere are skipped) and the `thorium` chart, waits for Thorium to be ready, installs `thorctl`, and imports the default toolbox.

```bash
minithor deploy [options]
```

| Flag | Description |
|------|-------------|
| `--chart <path\|oci-ref>` | The thorium chart: a local chart directory, a packaged `.tgz`, or an `oci://` reference (default: `oci://ghcr.io/cisagov/thorium/charts/thorium` at the pinned version) |
| `--operators-chart <path\|oci-ref>` | The infra-operators chart (default: next to a local `--chart`, or the matching `oci://` reference) |
| `--chart-version <version>` | Chart version for `oci://` charts |
| `--user <name>` | Username for the initial admin user (default: `test`) |
| `--password <pass>` | Password for the initial admin user (default: `INSECURE_DEV_PASSWORD`) |
| `--rand-password` | Let the chart generate the admin password (kept on redeploy; see `credentials`) |
| `--version <tag>` | Thorium image tag (default: the chart's appVersion) |
| `--registry` | Deploy a container registry in the thorium namespace, at `docker-registry.<thorium namespace>.svc.cluster.local:5000` (see Namespace Prefix) |
| `--registry-user <name>` | Enable registry basic auth for this user (implies `--registry`; password via `credentials --all`) |
| `--toolbox <path\|url>` | Toolbox to import (default: `tools/toolbox.json` on GitHub main), fetched on this host |
| `--no-toolbox` | Don't import a toolbox |
| `--banner <file>` | Login banner text |
| `--docker-config <file>` | `.dockerconfigjson` for pulling the Thorium image |
| `--bin <dir>` | Where to install thorctl (default: `/usr/local/bin`). It is logged in to `http://localhost:8080` (see `expose`) in `~/.thorium/config.yml`, or `~/.thorium/config-<profile>.yml` for any other profile (use it with `thorctl --config`); an existing config is backed up to `<config>.bak` first |
| `--no-thorctl` | Don't install thorctl |
| `--no-kibana` | Skip Kibana |
| `--proxy <url\|none>` | Proxy for the scaler (default: the host's `HTTPS_PROXY`/`HTTP_PROXY`; `none` leaves it unset) |
| `--no-proxy <list>` | Extra comma-separated entries appended to the scaler's `no_proxy` (see "Proxy configuration") |
| `--values <file>` / `--set <key=value>` | Any other thorium chart setting (repeatable; applied last), e.g. `--set operator.cluster.components.scaler=null` to omit the k8s scaler |
| `--skip-operators` | Don't install or upgrade the infra-operators chart |
| `--timeout <seconds>` | Timeout for each install and the readiness wait (default: 1800) |

The chart values come from development defaults embedded in the script, then your flags (written to `~/.cache/minithor/values.yaml`), then `--values`/`--set`. Every backing-service credential is generated by the chart on the first deploy and kept in the `thorium-credentials` secret, so re-running `deploy` is safe and upgrades in place. Every backing service runs in the cluster by default; to use an external one, pass a `--values` file that sets its `global.managed.<service>` toggle (`scylla`, `elastic`, `redis`, `s3`, `postgres`) to `false` along with its external settings (see `deploy/README.md`, "External services").

#### Testing unpublished charts

To test chart changes before they are published, point `--chart` at a chart directory or a packaged chart:

```bash
minithor deploy --chart deploy/charts/thorium                 # from a checkout
deploy/charts/scripts/package.sh -d /tmp/charts                       # or package both charts first
minithor deploy --chart /tmp/charts/thorium-1.8.1.tgz         # infra-operators-*.tgz is found alongside
```

`deploy/charts/scripts/package.sh` is what the Helm Charts workflow runs. Its `--image-repository`,
`--image-tag`, and `--pull-policy` options point the packaged thorium chart at another Thorium
image (see `--help`), for example a branch image a fork has published:

```bash
deploy/charts/scripts/package.sh -d /tmp/charts \
  --image-repository ghcr.io/<owner>/<repo>/infrastructure/thorium --image-tag <branch> --pull-policy Always
minithor deploy --chart /tmp/charts/thorium-1.8.1.tgz
```

Every branch pushed to a fork publishes prerelease charts to that fork's registry, and they
deploy the fork's image for that branch (see `deploy/README.md`). Install one with
`MINITHOR_CHART_REPO`, using the version printed by the `Helm Charts` workflow run:

```bash
MINITHOR_CHART_REPO=oci://ghcr.io/<owner>/<repo>/charts \
  minithor deploy --chart-version 1.8.1-<branch>.<run number>.g<short sha>
```

Packages a public repository's workflows publish are linked to that repository and public like
it, so a public fork's charts and image install without credentials. For a private fork, log in
for the charts and pass your Docker config for the image:

```bash
helm registry login ghcr.io -u <user>     # a token with read:packages
docker login ghcr.io -u <user>
MINITHOR_CHART_REPO=oci://ghcr.io/<owner>/<repo>/charts \
  minithor deploy --chart-version <version> --docker-config ~/.docker/config.json
```

#### Existing minithor deployments

Deployments made by earlier versions of minithor (namespaces `elastic-system`, `minithor`, ...) are not upgraded in place; remove them with that version's `cleanup --confirm` (or delete the cluster) and deploy again.

### Access Thorium

Port-forward the Thorium API to localhost:

```bash
minithor expose [--dev] [--no-api] [--port <port>] [--port-offset <n>] [--stop] [--status]
```

| Flag             | Description                                                     |
|------------------|-----------------------------------------------------------------|
| `--dev`          | Also forward database ports (Elastic, Kibana, Redis, SeaweedFS, Scylla) that the chart deployed |
| `--no-api`       | Forward only the backing services (skip API/registry); implies the database ports |
| `--port <port>`  | Local port for the Thorium API (default: 8080 + offset)         |
| `--port-offset <n>` | Add `<n>` to every local port so two minikube profiles (separate clusters) can be exposed at once |
| `--stop`         | Stop your port-forwards into this profile and remove the Scylla loopback aliases |
| `--status`       | Show which port-forwards are running                            |

The registry service is also forwarded (to port 5000 + offset) when a registry has been deployed. `expose` exits non-zero if any forward fails to start. If a local port is already held by something other than one of your own stale port-forwards (a local Redis, another user's forward, ...), that forward is reported as failed and the other process is left alone. Port-forward logs and loopback alias state are kept per user in `~/.cache/minithor/` (or `$XDG_CACHE_HOME/minithor/`).

Then open http://localhost:8080 in your browser and log in with the credentials shown at the end of the deploy output (default: `test` / `INSECURE_DEV_PASSWORD`).

### Get Config

Extract the running Thorium config from the cluster:

```bash
minithor get-config            # writes the raw in-cluster config to ~/thorium.yml
minithor get-config --local    # writes a config for host access via 'expose --dev' to ~/thorium.local.yml
```

Both files contain every backend credential and are written readable only by you. For a profile other than the default (`--profile <name>`) they are `~/thorium-<name>.yml` and `~/thorium-<name>.local.yml`.

### Namespace Prefix

`--namespace-prefix <prefix>` (or `MINITHOR_NAMESPACE_PREFIX=<prefix>`) prefixes Thorium's namespaces with `<prefix>-` (`<prefix>-thorium`, `<prefix>-redis`, `<prefix>-scylla`, `<prefix>-elastic`, `<prefix>-seaweedfs`, `<prefix>-quickwit`, `<prefix>-jaeger`), matching megathor's `namespace_prefix`. The prefix must be a lowercase DNS label of at most 32 characters; the `-` separator is added automatically. Without it the plain namespaces are used. A cluster runs one Thorium deployment, so pass the same prefix to `deploy`, `credentials`, `expose`, `get-config`, and `cleanup`:

```bash
minithor --namespace-prefix dev deploy
minithor --namespace-prefix dev expose --dev
minithor --namespace-prefix dev get-config --local   # ~/thorium.local.yml
minithor --namespace-prefix dev cleanup --confirm
```

The in-cluster registry (`deploy --registry`) is reached at `docker-registry.<prefix>-thorium.svc.cluster.local:5000`. The node's container runtime only trusts the registries minikube was started with, so also pass the prefix to `minikube install` (or `stop` and `start` the cluster with it) before deploying with `--registry`; deploy refuses a registry the cluster doesn't trust. `minithor start` maps the registry's host on every node whichever prefix it was deployed with.

To run two separate Thorium deployments on one host, use two minikube profiles (`--profile`), and `expose --port-offset` to expose both at once.

### Credentials

```bash
minithor credentials         # the admin user
minithor credentials --all   # every generated credential
```

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

Remove Thorium and its data. The `--confirm` flag is required:

```bash
minithor cleanup --confirm [--operators]
```

This deletes the ThoriumCluster (letting its operator clean up), uninstalls the thorium Helm release, and deletes Thorium's namespaces, including PVCs and generated credentials, along with the group namespaces the k8s scaler created (labelled `app.kubernetes.io/managed-by=thorium-scaler`) and the job data in them. Group namespaces created by a scaler that didn't label them must be deleted by hand. `--operators` also uninstalls the `infra-operators` chart and its CRDs (including the ECK CRDs, which Helm keeps), along with the ThoriumCluster CRD. `--timeout <seconds>` sets the timeout for each delete/wait (default: 300). The minikube cluster itself is preserved.

### Delete Minikube

Delete the selected profile's minikube cluster. The `--confirm` flag is required:

```bash
minithor minikube delete --confirm [--purge]
```

Only the selected profile is deleted, so other minikube clusters on the host survive. `--purge` also removes `~/.minikube` and `~/.kube`, uninstalls the minikube and kubectl binaries, and prunes unused container images (skipped while other profiles remain). This is irreversible.
