# Development Clusters (minithor)

`minithor` is a single script that creates a local Kubernetes cluster with
[Minikube](https://minikube.sigs.k8s.io) (one node by default, or several) and deploys Thorium onto
it with the [Thorium Helm charts](./deploy-helm.md). Use it for Thorium development, tool
development, demos, and testing. It is not highly available, every backing service runs as a
single instance, and it is reachable only from the host, so don't use it for production; see
[Production Clusters (megathor)](./megathor.md) or [Install with Helm](./deploy-helm.md) instead.

```bash
curl -fsSL https://raw.githubusercontent.com/cisagov/thorium/main/minithor/minithor -o minithor
chmod +x minithor
./minithor minikube install   # install Minikube and start the cluster
./minithor deploy             # deploy Thorium (admin user: test / INSECURE_DEV_PASSWORD)
./minithor expose             # Thorium UI and API at http://localhost:8080
```

## Requirements

- A container runtime or hypervisor. minithor picks the first available driver in this order:
  podman, docker, kvm2 (KVM/libvirt on Linux).
- 16 GiB of memory, 8 CPUs, and 100 GiB of free disk for the default cluster.
- `curl` and `tar`, plus `ss` or `lsof` for `expose`.
- `sudo` rights: minithor installs Minikube (and by default thorctl) into `/usr/local/bin` and
  adds loopback addresses for `expose --dev`.
- Helm 3.8 or newer from your `PATH`; without one, minithor downloads its pinned Helm (v3.22.0,
  checksum-verified) into `~/.cache/minithor/bin/`.

minithor downloads Minikube itself during `minikube install`.

## Installing a cluster

Run the three commands at the top of this page: `minikube install` once per host (and per
profile), then `deploy`, then `expose`, whose port-forwards keep running in the background.
`deploy` prints the admin login when it finishes (`test` / `INSECURE_DEV_PASSWORD` unless you
chose another; `minithor credentials` prints it again). After `minithor stop` or a host reboot,
run `minithor start` and `minithor expose` again.

## Global options

These may appear anywhere on the command line and apply to every command:

| Option | Environment variable | Meaning |
|--------|----------------------|---------|
| `--namespace-prefix <prefix>` | `MINITHOR_NAMESPACE_PREFIX` | Name Thorium's namespaces `<prefix>-thorium`, `<prefix>-redis`, and so on (see [Namespace prefix](#namespace-prefix)) |
| `--profile <name>` | `MINITHOR_PROFILE` or `MINIKUBE_PROFILE` | The Minikube profile (cluster) to use (default `minikube`) |
| `-h`, `--help` | | Help; `minithor <command> --help` shows a command's options |

Two more environment variables choose the charts `deploy` installs:

| Variable | Default | Meaning |
|----------|---------|---------|
| `MINITHOR_CHART_REPO` | `oci://ghcr.io/cisagov/thorium/charts` | The OCI repository holding the `thorium` and `infra-operators` charts |
| `MINITHOR_CHART_VERSION` | the chart version this minithor is pinned to | The chart version (`deploy --chart-version` overrides it) |

Every command only touches the selected profile's cluster (its kubectl context, named after the
profile), so other clusters in your kubeconfig are never used by accident. The pre-Helm
minithor's `--instance` option and `MINITHOR_INSTANCE` variable are rejected with an error (see
[Existing (pre-Helm) minithor deployments](#existing-pre-helm-minithor-deployments)).

## Commands

| Command | Purpose |
|---------|---------|
| `minikube install` | Install Minikube and start a cluster |
| `minikube delete` | Delete the selected profile's cluster |
| `start` | Start a stopped cluster |
| `stop` | Stop the cluster, keeping its state |
| `deploy` | Deploy Thorium (and the cluster-wide operators if missing) |
| `credentials` | Print the admin login (and optionally every backend credential) |
| `get-config` | Write the running Thorium config to `~/thorium.yml` |
| `expose` | Port-forward Thorium (and optionally the backing services) to localhost |
| `cleanup` | Remove Thorium and its data |

### minikube install

```bash
minithor minikube install [--nodes <n>] [--cpus <n> | --node-cpus <n>] [--memory <n> | --node-memory <n>] [--certs <path>] [--force-node-config]
```

| Flag | Meaning |
|------|---------|
| `--nodes <n>` | Number of nodes (default 1); Thorium schedules jobs across every node |
| `--cpus <n>` | Total CPUs for the whole cluster, split evenly across the nodes and rounded down (default 8). Unlike Minikube's own `--cpus`, this is a cluster total |
| `--memory <n>` | Total memory in GiB for the whole cluster, split evenly across the nodes (default 16) |
| `--node-cpus <n>` | CPUs per node instead of splitting `--cpus` (conflicts with `--cpus`) |
| `--node-memory <n>` | Memory in GiB per node instead of splitting `--memory` (conflicts with `--memory`) |
| `--certs <path>` | A `.crt` file, or a directory of `*.crt` files, to install into the node's trust store; required behind a TLS-intercepting proxy |
| `--force-node-config` | Overwrite the node's CA and container-runtime proxy drop-in even if they exist (by default existing files are kept so a manual fix isn't clobbered) |

It downloads Minikube for your OS and architecture and starts the cluster with the Calico CNI and
the `csi-hostpath-driver`, `ingress`, and `ingress-dns` addons. When the host has an HTTP proxy
configured, the node's container runtime is configured to pull through it (see
[Proxies and TLS interception](#proxies-and-tls-interception)). It also appends a
`kubectl="minikube kubectl --"` alias and a marked `NO_PROXY` block (replaced on every install) to
`~/.bashrc` and `~/.zshrc`. The sizes only apply when the profile is created; running install
again on an existing profile restarts it with the sizes it was created with.

**Multi-node clusters.** `--nodes <n>` creates nodes `minikube`, `minikube-m02`, ... For example
`--nodes 3 --cpus 15 --memory 48` gives each node 5 CPUs and 16 GiB. Each node needs more than 2
CPUs and 2 GiB of memory, because the Thorium scaler reserves that much on every node, and install
refuses a split that leaves less. The scaler's node list is empty by default, so the operator
labels and registers every node. To try a multi-node cluster next to an existing one, use another
profile:

```bash
minithor --profile multi minikube install --nodes 3 --cpus 15 --memory 48
minithor --profile multi deploy
minithor --profile multi minikube delete --confirm   # deletes only the "multi" cluster
```

### minikube delete

```bash
minithor [--profile <name>] minikube delete --confirm [--purge]
```

| Flag | Meaning |
|------|---------|
| `--confirm` | Required |
| `--purge` | Also remove `~/.minikube` and `~/.kube`, uninstall the Minikube and kubectl binaries, and prune unused container images (skipped while other profiles remain) |

Only the selected profile's cluster is deleted. This is irreversible.

### stop and start

`minithor stop` stops the Minikube VM or container and keeps its state. `minithor start` starts
the cluster again with the driver it was created with, corrects the kubeconfig's API server
address if needed, re-applies the node proxy settings and kernel parameters, maps the in-cluster
registry's host on every node, and cleans up pods stuck in terminal failure states. It refuses a
profile that doesn't exist yet; create that with `minikube install`. Port-forwards don't survive
a stop, so run `expose` again afterwards.

### deploy

`deploy` installs the `infra-operators` chart (once per cluster, skipping operators that already
run), installs or upgrades the `thorium` chart, waits for Thorium to be ready, installs `thorctl`
and logs it in, and imports the default toolbox.

```bash
minithor [--namespace-prefix <prefix>] deploy [options]
```

| Flag | Meaning |
|------|---------|
| `--chart <path\|oci-ref>` | The thorium chart: a chart directory, a packaged `.tgz`, or an `oci://` reference |
| `--operators-chart <path\|oci-ref>` | The infra-operators chart (default: next to a local `--chart`, or the matching `oci://` reference) |
| `--chart-version <version>` | Chart version for `oci://` charts |
| `--user <name>` | Admin username (default: the stored one on a redeploy, else `test`) |
| `--password <pass>` | Admin password (default: the stored one on a redeploy, else `INSECURE_DEV_PASSWORD`). The operator creates the admin once and doesn't change an existing admin's password, so a new password on a redeploy only changes the `thorium-admin` Secret |
| `--rand-password` | Let the chart generate the admin password on the first deploy; a redeploy without `--password` keeps the stored one. Can't be combined with `--password` |
| `--version <tag>` | Thorium image tag (default: the chart's appVersion) |
| `--registry` | Deploy a container registry at `docker-registry.<thorium namespace>.svc.cluster.local:5000` (see [Registry](#registry)) |
| `--registry-user <name>` | Enable registry basic auth for this user (implies `--registry`; the password is in `credentials --all`) |
| `--toolbox <path\|url>` | Toolbox to import (default: `tools/toolbox.json` on GitHub main), fetched on this host |
| `--no-toolbox` | Don't import a toolbox |
| `--banner <file>` | Login banner text |
| `--docker-config <file>` | A `.dockerconfigjson` for pulling the Thorium image (see [Private images](#private-images)) |
| `--bin <dir>` | Where to install thorctl (default `/usr/local/bin`) |
| `--no-thorctl` | Don't install thorctl |
| `--no-kibana` | Skip Kibana |
| `--proxy <url\|none>` | The proxy the scaler uses to reach registries (default: the host's `HTTPS_PROXY`/`HTTP_PROXY`; `none` leaves it unset) |
| `--no-proxy <list>` | Extra comma-separated entries appended to the scaler's `no_proxy` |
| `--values <file>` | Extra values file for the thorium chart (repeatable) |
| `--set <key.path=value>` | Extra value for the thorium chart (repeatable), for example `--set operator.cluster.components.scaler=null` to omit the k8s scaler |
| `--skip-operators` | Don't install or upgrade the infra-operators chart |
| `--timeout <seconds>` | Timeout for each install and for the readiness wait (default 1800) |

**Chart values.** The values come, in increasing precedence, from development defaults embedded in
the script (every backing service in the cluster, an nginx ingress, toolbox `mode: overwrite`, a
1 GiB Elasticsearch heap), then the values your flags produce (written to
`~/.cache/minithor/values.yaml`, or `values-<profile>.yaml` for another profile), then your
`--values` files and `--set` flags. The flag values also set `global.clusterCIDRs` to the
profile's service and pod CIDRs and `operator.cluster.upgrade.autoTargetDev: true`. To use an
external backing service, pass a `--values` file that sets its `global.managed.<service>` toggle to
`false` with its external settings (see [External services](./helm-configuration.md#external-services)).

**Redeploying and upgrades.** Every backing-service credential is generated by the chart on the
first deploy and kept in `thorium-credentials`, so running `deploy` again upgrades in place. No
other option is remembered: each `deploy` renders the values from its own flags (Helm
`--reset-values`), so repeat the flags you deployed with (`--registry`, `--no-kibana`, `--values`,
...) on every redeploy. To upgrade, download the newer minithor (it is pinned to its chart
version) or pass `--chart-version`, then run `deploy` again. Because minithor sets
`autoTargetDev`, the operator runs every new [revision](./upgrades.md)'s upgrade steps as soon as
a newer chart is deployed. `deploy` waits until every `ThoriumCluster` in the thorium namespace is
`Ready` for its current spec, printing what it still waits on every minute, and fails at once,
printing the operator's message, when one is in the `Error` phase, in the `UpgradeRequired` phase
(a `targetRevision` or `autoTargetDev: false` from your `--values` or `--set` holds it below the
latest revision), or has an upgrade step that is `Blocked` (for example a step waiting for an
approval; see [Upgrading Thorium](./upgrades.md)).

**One deployment per cluster.** A namespace prefix only renames namespaces, so `deploy` refuses
when a `thorium` release or a `ThoriumCluster` exists under another prefix (pass that prefix, or
remove that deployment first), and when the `ThoriumCluster` in the selected namespace was made by
a pre-Helm minithor or megathor (see [Existing (pre-Helm) minithor deployments](#existing-pre-helm-minithor-deployments)).

**thorctl and the admin login.** thorctl is installed into `--bin` and logged in to
`http://localhost:8080` (the port `expose` uses by default) in `~/.thorium/config.yml`, or
`~/.thorium/config-<profile>.yml` for another profile (use it with `thorctl --config`). An existing
config is backed up to `<config>.bak` first. `thorctl login` only accepts a password on its
command line or from a terminal, so the admin password is briefly visible to other local users in
the process list while it logs in; use `--no-thorctl` on a shared host. The admin login printed at
the end is read from the `ThoriumCluster`'s admin bootstrap. When that bootstrap is off (a
deployment converted without an admin login), no admin user is created, thorctl is installed
without logging in, and `credentials` says so instead of printing a login.

```bash
minithor deploy
minithor --namespace-prefix dev deploy --rand-password
minithor deploy --chart ./deploy/charts/thorium    # from a checkout
```

### credentials

```bash
minithor [--namespace-prefix <prefix>] credentials [--all]
```

Prints the admin login; `--all` also prints every generated backend credential (from
`thorium-credentials`).

### get-config

```bash
minithor get-config [--local [--port-offset <n>]]
```

Reads the `thorium` Secret in the thorium namespace and writes the rendered config to
`~/thorium.yml` (`~/thorium-<profile>.yml` for another profile). Both files hold every backend
credential and are readable only by you.

`--local` writes `~/thorium.local.yml` (`~/thorium-<profile>.local.yml`) instead, rewritten for
running a local build of the API or tools against the cluster's databases through
`expose --dev`; `--port-offset <n>` matches the offset given to `expose`:

| Setting | Rewritten to |
|---------|--------------|
| Elastic, Redis, and SeaweedFS hosts | `localhost` |
| `elastic.insecure_certificates` | `true` (localhost doesn't match the cluster's certificate) |
| `scylla.nodes` | each node's advertised cluster IP (matching the loopback-aliased forward `expose` creates) |
| `thorium.port` | `8888` (port 80 needs root) |
| `thorium.tracing.external` | `null` (the in-cluster collector isn't reachable) |

### expose

```bash
minithor expose [--dev] [--no-api] [--port <port>] [--port-offset <n>] [--stop] [--status]
```

| Flag | Meaning |
|------|---------|
| `--dev` | Also forward the database ports (Elastic, Kibana, Redis, SeaweedFS, Scylla) the chart deployed |
| `--no-api` | Forward only the backing services (skip the API and registry) so a locally run API can use them; implies the database ports |
| `--port <port>` | Local port for the Thorium API (default 8080 plus the offset) |
| `--port-offset <n>` | Add `<n>` to every local port (API, Elastic, Kibana, Redis, SeaweedFS, registry) so two profiles can be exposed at once |
| `--stop` | Stop your port-forwards into this profile and remove the loopback aliases `expose` added for it; an alias another profile's forwards also use is kept |
| `--status` | Show which port-forwards are running |

The registry is also forwarded (port 5000 plus the offset) when one was deployed, and Kibana only
when it was deployed. `expose` exits non-zero if any forward fails to start; a local port held by
something other than your own stale forward is reported and left alone. Logs and the loopback
aliases each profile added are kept in `~/.cache/minithor/` (or `$XDG_CACHE_HOME/minithor/`), in
`loopback-aliases` for the default profile and `loopback-aliases-<profile>` for the others, so
`expose --stop` and `minikube delete` remove only the selected profile's aliases. Then open
`http://localhost:8080` and log in with the credentials from `deploy`.

### cleanup

```bash
minithor [--namespace-prefix <prefix>] cleanup --confirm [--operators] [--timeout <seconds>]
```

| Flag | Meaning |
|------|---------|
| `--confirm` | Required |
| `--operators` | Also uninstall the infra-operators chart and its CRDs (including the ECK CRDs Helm keeps), and the `ThoriumCluster` CRD |
| `--timeout <seconds>` | Timeout for each delete or wait (default 300) |

cleanup deletes the `ThoriumCluster` (letting its operator clean up), uninstalls the `thorium`
release, and deletes Thorium's namespaces, including PVCs and the generated credentials, along with
the group namespaces the k8s scaler created (labelled
`app.kubernetes.io/managed-by=thorium-scaler`) and the job data in them. Group namespaces are only
deleted when the `thorium` release exists in the selected namespace, so a wrong or missing
`--namespace-prefix` never deletes another deployment's jobs. Group namespaces created by a scaler
that didn't label them must be deleted by hand. The Minikube cluster itself is kept.

## Namespace prefix

`--namespace-prefix <prefix>` names Thorium's namespaces `<prefix>-thorium`, `<prefix>-redis`,
`<prefix>-scylla`, `<prefix>-elastic`, `<prefix>-seaweedfs`, `<prefix>-quickwit`, and
`<prefix>-jaeger`, like megathor's `namespace_prefix`. The prefix is a lowercase DNS label of at
most 32 characters; the `-` is added automatically. Pass the same prefix to every command for a
deployment:

```bash
minithor --namespace-prefix dev deploy
minithor --namespace-prefix dev expose --dev
minithor --namespace-prefix dev get-config --local
minithor --namespace-prefix dev cleanup --confirm
```

A cluster runs one Thorium deployment (see
[Namespaces and the namespace prefix](./concepts.md#namespaces-and-the-namespace-prefix)). To run
two deployments on one host, use two profiles (`--profile`) and `expose --port-offset <n>` to
expose the second on shifted ports.

## Registry

`deploy --registry` deploys a registry for tool images at
`docker-registry.<thorium namespace>.svc.cluster.local:5000`. The node's container runtime only
trusts the registries Minikube was started with, so pass the same `--namespace-prefix` to
`minikube install` (or `stop` and `start` the cluster with it) before deploying with `--registry`;
`deploy` refuses a registry the cluster doesn't trust.

`expose` forwards the registry to `localhost:5000` (plus any `--port-offset`). Push images there
from the host and reference them in Thorium images by the in-cluster address:

```bash
docker tag mytool:latest localhost:5000/tools/mytool:1.0
docker push localhost:5000/tools/mytool:1.0
# image in Thorium: docker-registry.thorium.svc.cluster.local:5000/tools/mytool:1.0
```

With `--registry-user <name>`, first `docker login localhost:5000` as that user with the
`registryPassword` that `minithor credentials --all` prints.

## Private images

If the Thorium image is in a private registry, pass its credentials; they become the chart's
`thorium-image-pull` Secret:

```bash
docker login registry.example.com:5000
minithor deploy --docker-config ~/.docker/config.json
```

Without it, images are pulled anonymously, which works for public registries such as `ghcr.io`.

## Proxies and TLS interception

Export the proxy before installing the cluster:

```bash
export HTTP_PROXY=http://proxy.example.com:3128
export HTTPS_PROXY=http://proxy.example.com:3128
minithor minikube install --certs /path/to/proxy-ca.crt   # --certs only for a TLS-intercepting proxy
```

You don't need to set `NO_PROXY`: minithor computes the bypass list (the node subnet, service and
pod CIDRs, control-plane host, loopback, and the cluster-internal registry and DNS suffixes),
unions it with any `NO_PROXY`/`no_proxy` you set, and uses it for `install`, `start`, and `expose`.
`minikube install` and `start` configure the node's container runtime to pull through the proxy.

`deploy` gives the Thorium scaler the proxy (it reaches registries such as `ghcr.io`) through its
`http_proxy`/`https_proxy`/`no_proxy` environment, defaulting to the host's
`HTTPS_PROXY`/`HTTP_PROXY`; `--proxy <url>` picks another and `--proxy none` leaves the scaler
unproxied. The scaler's `no_proxy` lists only the cluster's own names and ranges: `localhost`,
`127.0.0.1`, `.svc`, `.svc.cluster.local`, `cluster.local`, `control-plane.minikube.internal`, the
profile's service and pod CIDRs (from its saved Minikube config, otherwise `10.96.0.0/12` and
`10.244.0.0/16`), the node subnet, and the in-cluster registry. Your host's `NO_PROXY` isn't copied
into it; append entries with `--no-proxy <list>`. To give the operator itself a proxy, set
`operator.controller.proxy` in a `--values` file (see [Proxies](./helm-configuration.md#proxies)).
On an unproxied host none of this applies.

## Existing (pre-Helm) minithor deployments

A deployment made by a minithor from before the Helm charts (namespaces such as `elastic-system`
and `minithor`, a `ThoriumCluster` named `dev`) must be converted before this minithor deploys over
it. The `--instance` option (and `MINITHOR_INSTANCE` variable) of those versions is
`--namespace-prefix` (`MINITHOR_NAMESPACE_PREFIX`) here; passing either exits with an error pointing
at this procedure. If the cluster holds several instances, keep one and retire the others first
(see [Retiring other pre-Helm instances](./convert-to-helm.md#retiring-other-pre-helm-instances)).
Convert with `convert-to-helm.sh` from the Thorium repository, as described in
[Converting a Pre-Helm Deployment](./convert-to-helm.md), passing the old `--instance` as
`--namespace-prefix`, the profile's kubectl context with `--context` (the profile name, `minikube`
by default), and the admin login it created (a login saved with `--rand-password` is read from the
`minithor-credentials` Secret automatically). Prefer the `THORIUM_ADMIN_PASSWORD` environment
variable or `--admin-password-stdin` over `--admin-password`, which other local users can see in
the process list:

```bash
export THORIUM_ADMIN_PASSWORD=INSECURE_DEV_PASSWORD
deploy/charts/scripts/convert-to-helm.sh --context minikube --admin-user test --dry-run
deploy/charts/scripts/convert-to-helm.sh --context minikube --admin-user test
minithor deploy --skip-operators --no-toolbox --values thorium-converted-values.yaml
```

`--skip-operators` keeps the earlier version's Scylla, ECK, and Kubegres operators, which the
converted deployment's databases still use, and `--no-toolbox` keeps the toolbox import off as the
converted values do. Pass the same flags (with any others you need) on every later `deploy` of the
converted deployment. To roll back before this `deploy`, see
[Rolling back](./convert-to-helm.md#rolling-back).

minithor sets `autoTargetDev`, so the operator upgrades the converted deployment right away; if an
upgrade step needs an approval, `deploy` stops and prints it (see [Upgrading Thorium](./upgrades.md)).
The databases stay where the earlier version put them. Those namespaces have the names this
minithor uses, except the default instance's Elasticsearch in `elastic-system`, which
`expose --dev` skips and `get-config --local` leaves pointing at its in-cluster host.
Alternatively, remove the old deployment with that version's `cleanup --confirm` (or delete the
cluster) and deploy again.

`minithor cleanup --confirm` on a converted deployment removes the Thorium release and the chart's
namespaces but leaves what the earlier version created outside them. Once cleanup has finished,
remove those leftovers by hand. Set `PROFILE` to the Minikube profile, and prefix the namespaces
and binding with `<prefix>-` where shown for a deployment that used `--instance <prefix>`. `helm` is
the one on your `PATH`, or the one minithor downloaded into `~/.cache/minithor/bin/`:

```bash
PROFILE=minikube
k() { minikube -p "$PROFILE" kubectl -- --context "$PROFILE" "$@"; }
h() { helm --kube-context "$PROFILE" "$@"; }
# the legacy Elasticsearch of the default instance and the old ECK operator next to it (an
# instance with a prefix kept Elasticsearch in <prefix>-elastic, which cleanup already deletes)
k delete elasticsearch,kibana --all -n elastic-system
h uninstall elastic-operator -n elastic-system
k delete ns elastic-system
# the credentials the earlier version saved
k delete ns minithor                     # or <prefix>-minithor
# the copy of the old scaler's RBAC the conversion kept (convert-to-helm.sh --cleanup removes it
# too) and the old operator's binding, if either is still there
k delete clusterrolebinding,clusterrole -l thorium.sandia.gov/legacy-components=true
k delete clusterrolebinding thorium-operator-binding --ignore-not-found   # or thorium-operator-binding-<prefix>
# the other legacy operators, unless another deployment on the cluster still uses them
h uninstall scylla-operator -n scylla-operator; k delete ns scylla-operator
h uninstall cert-manager -n cert-manager; k delete ns cert-manager
k delete ns kubegres-system
k delete clusterrole kubegres-kubegres-editor-role kubegres-kubegres-viewer-role \
  kubegres-manager-role kubegres-metrics-auth-role kubegres-metrics-reader --ignore-not-found
k delete clusterrolebinding kubegres-manager-rolebinding kubegres-metrics-auth-rolebinding \
  --ignore-not-found
# the ThoriumCluster CRD the conversion applied, and the legacy operators' CRDs; skip this
# while an infra-operators release is installed (cleanup --operators removes its own CRDs)
k get crd -o name | grep -E 'cert-manager|elastic|scylla|kubegres|sandia' | while read -r crd; do
  k delete "$crd"
done
```

After that, `minithor deploy` (without `--skip-operators`) installs the infra-operators chart and
a fresh deployment.

## Troubleshooting

- **`deploy` times out or keeps printing "still waiting".** The message is the `ThoriumCluster`'s
  `status.message`. Check the operator and the backing services with
  `minikube kubectl -- --context <profile> -n thorium logs deploy/operator` (`<prefix>-thorium`
  with a namespace prefix) and `minikube kubectl -- --context <profile> get pods -A`. See
  [Troubleshooting Deployments](./troubleshooting.md).
- **`deploy` fails with an `Error` phase or a `Blocked` upgrade step.** The printed message names
  the cause; for approvals and reindexing see [Upgrading Thorium](./upgrades.md).
- **Pods stuck in `ImagePullBackOff` behind a proxy.** Export `HTTP_PROXY`/`HTTPS_PROXY` and run
  `minithor stop` and `minithor start`, which re-applies the node proxy settings and restarts the
  stuck pods. Behind a TLS-intercepting proxy the cluster must be installed with `--certs`;
  `minikube install --force-node-config` replaces a node CA or proxy drop-in that is wrong.
- **Scylla or Elasticsearch pods crash right after a host reboot.** `minithor start` (and every
  `deploy`) raises the node kernel limits they need (`fs.aio-max-nr`, `vm.max_map_count`, ...);
  run it rather than plain `minikube start`.
- **`expose` reports a forward as failed.** Each forward logs to `~/.cache/minithor/expose-logs/`.
  A port held by another program is left alone; pick other ports with `--port` or
  `--port-offset`.
- **kubectl can't reach the cluster after a reboot.** Every minithor command fixes a stale
  kubeconfig; run `minithor start` (or any command against a running cluster).

## Testing unpublished charts and fork builds

To deploy charts from a checkout, a locally packaged chart, or a fork's prerelease charts, see
"Testing unpublished charts" in `minithor/README.md` in the Thorium repository.
