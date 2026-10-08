# Example Values Files

Complete values files to copy and edit. Each one renders with the thorium chart as-is (the
chart's CI renders all of them), so a copy starts from a known-good file. Layer them with
repeated `-f` flags; later files win:

```bash
helm install thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
  -n thorium --create-namespace -f values-production.yaml -f site.yaml
```

| Example | Use it for |
|---|---|
| [Production](./example-production.md) | a multi-node starting point (sizes, replicas, storage) |
| [Minimal site](./example-site.md) | the few settings every site sets: storage class, ingress, job nodes |
| [External services](./example-external.md) | pointing Thorium at Scylla, Elasticsearch, Redis, S3 and Postgres the site runs |
| [Proxy](./example-proxy.md) | clusters that reach the internet only through an HTTP proxy |
| [Offline mirror](./example-offline.md) | air-gapped installs pulling every image from a local registry |

The files live in the repository under `deploy/charts/examples/` (and `values-production.yaml`
ships inside the thorium chart). Every option they set is described in
[Configure the Helm Charts](./helm-configuration.md) and listed in the
[Chart Values Reference](./chart-values.md).
