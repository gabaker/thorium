# minithor

`minithor` deploys Thorium onto a local Minikube cluster with the Thorium Helm charts
(`deploy/charts`), for development and testing. It is a single script:

```bash
curl -fsSL https://raw.githubusercontent.com/cisagov/thorium/main/minithor/minithor -o minithor
chmod +x minithor
./minithor minikube install   # install Minikube and start the cluster
./minithor deploy             # deploy Thorium (admin user: test / INSECURE_DEV_PASSWORD)
./minithor expose             # Thorium UI and API at http://localhost:8080
```

The full guide (requirements, every command and option, multi-node clusters, namespace prefixes,
the registry, proxies, and upgrades) is the page "Development Clusters (minithor)" in the Thorium
docs ([api/docs/src/admins/deploy/minithor.md](../api/docs/src/admins/deploy/minithor.md)).
`./minithor --help` and `./minithor <command> --help` list every option.

## Testing unpublished charts

To test chart changes before they are published, point `--chart` at a chart directory or a
packaged chart:

```bash
minithor deploy --chart deploy/charts/thorium                 # from a checkout
deploy/charts/scripts/package.sh -d /tmp/charts               # or package both charts first
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

## Existing minithor deployments

A deployment made by a minithor from before the Helm charts must be converted with
`deploy/charts/scripts/convert-to-helm.sh` before this minithor deploys over it (`--instance` is
`--namespace-prefix` here). See "Existing (pre-Helm) minithor deployments" on the
"Development Clusters (minithor)" page
([api/docs/src/admins/deploy/minithor.md](../api/docs/src/admins/deploy/minithor.md)) and
"Converting a Pre-Helm Deployment"
([api/docs/src/admins/deploy/convert-to-helm.md](../api/docs/src/admins/deploy/convert-to-helm.md)).
