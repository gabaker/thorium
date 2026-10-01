
# Overview

Minithor utilizes Minikube to provide a local Kubernetes instance with minimal custom configuration required, and deploys Thorium onto it with the Thorium Helm charts (`deploy/charts`). Such an instance is useful for development and testing of Thorium and its tools. Minithor deployments are not highly available distributed systems like our production instances and provides minimal redundancy. The Thorium deployment produced by following these instructions should be considered Beta. We will work to improve its stability over time. While a Minithor deployment is accessible only from your localhost, all backing-service passwords are randomly generated on the first deploy and kept afterward. The default admin user password can be randomized with `--rand-password`.

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
minithor — manage local Thorium instances on Minikube with the Thorium Helm charts

Usage: minithor [global options] <command> [options]

Commands:
  minikube         Manage the minikube cluster (install, delete)
  start            Start a previously stopped minikube cluster
  deploy           Deploy a Thorium instance (and the cluster-wide operators if missing)
  credentials      Print the instance's admin (and optionally every backend) credential
  get-config       Extract the running Thorium config to ~/thorium.yml
  expose           Port-forward Thorium (and optionally backing services) to localhost
  stop             Stop the minikube cluster (preserves state)
  cleanup          Remove an instance and its data (requires --confirm)

Global options (may appear anywhere on the command line):
  --instance <name>  Operate on a named Thorium instance (see Multiple Instances)
  --profile <name>   The minikube profile to use (default: minikube; also MINITHOR_PROFILE)
  -h, --help         Show this help message
```

Every command only touches the selected minikube profile's cluster (`--profile`), so other clusters in your kubeconfig are never used by accident.

### Install Minikube

Install and start minikube and any necessary plugins.

```bash
minithor minikube install [--cpus <n>] [--memory <n>] [--certs <path>] [--force-node-config]
```

| Flag | Description |
|------|-------------|
| `--cpus <n>` | Number of CPUs to allocate to minikube (default: 8) |
| `--memory <n>` | Memory in GiB to allocate to minikube (default: 16) |
| `--certs <path>` | A `.crt` file, or a directory of `*.crt` files, to install into the minikube node's trust store. Required behind a TLS-intercepting proxy so the node trusts it for image pulls. No default — cert install only runs when this flag is given. |
| `--force-node-config` | Overwrite the node's CA and container-runtime (dockerd or containerd) proxy drop-in even if they already exist. By default existing files are left in place so a manual fix on the node isn't clobbered. |

Detects the best available driver (podman > docker > kvm2), downloads minikube for your OS/architecture, configures resource limits, and starts the cluster with Calico CNI, CSI, and Ingress addons. When the host has an HTTP proxy configured (see below), the node's container runtime is also configured to pull images through it.

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
| `--registry` | Deploy a container registry in the thorium namespace (default instance only) |
| `--registry-user <name>` | Enable registry basic auth for this user (implies `--registry`; password via `credentials --all`) |
| `--toolbox <path\|url>` | Toolbox to import (default: `tools/toolbox.json` on GitHub main), fetched on this host |
| `--no-toolbox` | Don't import a toolbox |
| `--banner <file>` | Login banner text |
| `--docker-config <file>` | `.dockerconfigjson` for pulling the Thorium image |
| `--bin <dir>` | Where to install thorctl (default: `/usr/local/bin`) |
| `--no-thorctl` | Don't install thorctl |
| `--no-kibana` | Skip Kibana |
| `--no-scaler` | Omit the k8s scaler (when another instance runs one) |
| `--values <file>` / `--set <key=value>` | Any other thorium chart setting (repeatable; applied last) |
| `--skip-operators` | Don't install or upgrade the infra-operators chart |
| `--timeout <seconds>` | Timeout for each install and the readiness wait (default: 1800) |

The chart values come from development defaults embedded in the script, then your flags (written to `~/.cache/minithor/values.yaml`), then `--values`/`--set`. Every backing-service credential is generated by the chart on the first deploy and kept in the `thorium-credentials` secret, so re-running `deploy` is safe and upgrades in place.

#### Testing unpublished charts

To test chart changes before they are published, point `--chart` at a chart directory or a packaged chart:

```bash
minithor deploy --chart deploy/charts/thorium                 # from a checkout
helm package -d /tmp/charts deploy/charts/infra-operators deploy/charts/thorium
minithor deploy --chart /tmp/charts/thorium-1.8.1.tgz         # infra-operators-*.tgz is found alongside
```

#### Existing minithor deployments

Instances deployed by earlier versions of minithor (namespaces `elastic-system`, `minithor`, ...) are not upgraded in place; remove them with that version's `cleanup --confirm` (or delete the cluster) and deploy again.

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
| `--stop`         | Stop your port-forwards into this profile (every instance) and remove the Scylla loopback aliases |
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

`--instance <name>` (or `MINITHOR_INSTANCE=<name>`) selects a named Thorium instance for `deploy`, `credentials`, `expose`, `get-config`, and `cleanup`. The default instance uses the plain namespaces (`thorium`, `redis`, `scylla`, `elastic`, `seaweedfs`, `quickwit`, `jaeger`); a named instance prefixes each with `<name>-`, so several instances can run side by side:

```bash
minithor deploy                                  # default instance
minithor --instance b deploy --no-scaler         # a second instance
minithor --instance b expose --dev --port-offset 100   # API on :8180, Elastic on :9300, ...
minithor --instance b get-config --local --port-offset 100   # ~/thorium-b.local.yml
minithor --instance b cleanup --confirm          # removes only instance b
```

All instances share the cluster-wide operators (the `infra-operators` release); each runs its own Thorium operator that only manages its own namespace. Each instance's scaler creates job namespaces named after Thorium groups (such as `static`), so run the scaler in only one instance (`--no-scaler` elsewhere). The in-cluster registry (`--registry`) is only available to the default instance, and a named instance's ingress host is `<name>.localhost`.

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

Remove an instance and its data. The `--confirm` flag is required:

```bash
minithor cleanup --confirm [--operators]
```

This deletes the instance's ThoriumCluster (letting its operator clean up), uninstalls its Helm release, and deletes its namespaces, including PVCs and generated credentials. Other instances are left alone. `--operators` also uninstalls the `infra-operators` chart and its CRDs when no other instance remains. `--timeout <seconds>` sets the timeout for each delete/wait (default: 300). The minikube cluster itself is preserved.

### Delete Minikube

Delete the selected profile's minikube cluster. The `--confirm` flag is required:

```bash
minithor minikube delete --confirm [--purge]
```

Only the selected profile is deleted, so other minikube clusters on the host survive. `--purge` also removes `~/.minikube` and `~/.kube`, uninstalls the minikube and kubectl binaries, and prunes unused container images (skipped while other profiles remain). This is irreversible.
