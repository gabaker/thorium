# Example: External Services

Thorium against backing services the site runs itself, so the chart deploys none of them. See [External services](./helm-configuration.md#external-services) for the requirements of each service and for letting the operator create Thorium's Scylla role and Elasticsearch user instead of supplying their credentials.

```yaml
{{#include ../../../../../deploy/charts/examples/external-services.yaml}}
```
