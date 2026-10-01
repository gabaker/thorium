# Development Instances (minithor)

`minithor` creates a single-node Kubernetes cluster with [Minikube](https://minikube.sigs.k8s.io)
and deploys Thorium onto it with the [Thorium Helm charts](./deploy-helm.md). It is meant for
Thorium development, tool development, and testing, not production. It is a single script, so
you can download it on its own:

```bash
curl -fsSL https://raw.githubusercontent.com/cisagov/thorium/main/minithor/minithor -o minithor
chmod +x minithor
```

The host needs a container runtime (Docker or Podman) or KVM, at least 16 GiB of memory, 8 CPUs,
and 100 GiB of free disk.

## Quick start

```bash
./minithor minikube install   # install Minikube and start the cluster
./minithor deploy             # deploy Thorium (admin user: test / INSECURE_DEV_PASSWORD)
./minithor expose             # Thorium UI and API at http://localhost:8080
```

`deploy` installs the published charts, waits for Thorium to be ready, installs `thorctl` and
logs it in, and imports the default toolbox. Common options:

| Option | Effect |
|--------|--------|
| `--user <name>`, `--password <pass>`, `--rand-password` | The initial admin user |
| `--version <tag>` | The Thorium image tag to run |
| `--chart <path>` | Use a locally downloaded chart (`.tgz` or directory) instead of the published one |
| `--toolbox <path or url>`, `--no-toolbox` | The toolbox to import |
| `--registry` | Deploy a container registry for tool images |
| `--values <file>`, `--set <key=value>` | Any other [chart setting](./deploy-helm.md#how-the-thorium-chart-works) |

Other commands:

| Command | Purpose |
|---------|---------|
| `credentials [--all]` | Print the admin user's (and every generated) credential |
| `get-config [--local]` | Write the instance's Thorium config to `~/thorium.yml` |
| `expose --dev` | Also forward Elasticsearch, Redis, S3, and Scylla for running Thorium components locally |
| `stop`, `start` | Stop or restart the cluster, keeping its data |
| `cleanup --confirm` | Remove the Thorium instance and its data |

## Several instances

`--instance <name>` deploys an additional instance whose namespaces are prefixed with
`<name>-`, next to the default one:

```bash
./minithor --instance b deploy --no-scaler
./minithor --instance b expose --port-offset 100   # http://localhost:8180
```

Run `./minithor --help` and `./minithor <command> --help` for every option.
