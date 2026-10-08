# Example: Production

A starting point for multi-node clusters, layered over the chart defaults (see [Sizing](./planning.md#sizing)). It ships inside the thorium chart as `values-production.yaml`; layer a [site file](./example-site.md) over it.

```yaml
{{#include ../../../../../deploy/charts/thorium/values-production.yaml}}
```
