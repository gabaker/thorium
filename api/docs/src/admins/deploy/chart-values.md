# Chart Values Reference

This page lists every value of the Thorium Helm charts with its default and description, taken
from the charts' own `values.yaml` files for this version of the documentation.
[Configure the Helm Charts](./helm-configuration.md) explains how the values fit together. To read
the values of the chart version you deploy, pull it:

```bash
helm pull oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION --untar
helm show values oci://ghcr.io/cisagov/thorium/charts/infra-operators --version $VERSION
```

`helm show values` on the thorium chart prints only the umbrella's own values, not its subcharts'.
In the thorium chart, the values of each subchart below go under its key: `secrets.*`,
`infra.*`, `quickwit.*`, and `operator.*`.

## thorium (umbrella)

```yaml
{{#include ../../../../../deploy/charts/thorium/values.yaml}}
```

## thorium: operator (`operator.*`)

```yaml
{{#include ../../../../../deploy/charts/thorium/charts/operator/values.yaml}}
```

## thorium: secrets (`secrets.*`)

```yaml
{{#include ../../../../../deploy/charts/thorium/charts/secrets/values.yaml}}
```

## thorium: infra (`infra.*`)

```yaml
{{#include ../../../../../deploy/charts/thorium/charts/infra/values.yaml}}
```

The `quickwit.*` values configure the vendored upstream Quickwit chart; the umbrella's
`quickwit` section above sets the values Thorium needs, and the Quickwit chart's own
`values.yaml` (in `charts/quickwit/` of the pulled chart) lists the rest.

## values-production.yaml and other examples

`values-production.yaml` (a multi-node starting point that ships inside the thorium chart) and
the other complete example files are in [Example Values Files](./example-values.md).

## infra-operators

```yaml
{{#include ../../../../../deploy/charts/infra-operators/values.yaml}}
```

The `scylla-operator.*` and `eck-operator.*` values configure the vendored upstream charts; see
their `values.yaml` files in `charts/` of the pulled infra-operators chart.
