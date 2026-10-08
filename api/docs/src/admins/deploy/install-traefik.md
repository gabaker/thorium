# Traefik

Traefik is a reverse proxy and load balancer that routes HTTP and HTTPS traffic to Thorium (and
to other web services in the cluster, such as a tool image registry). megathor installs it with
the settings on this page. Install it yourself when your cluster has no ingress controller and
you want the chart's Traefik ingress (`operator.ingress.type: traefik`); with an nginx ingress
controller, use `operator.ingress.type: nginx` instead and skip this page.

## 1) Add the Helm repository

```bash
helm repo add traefik https://helm.traefik.io/traefik
helm repo update
```

## 2) Write the values

Save these values as `traefik-values.yaml`. megathor sets the replica count and the external
IPs and otherwise uses the chart's defaults, including `global.sendAnonymousUsage: false`:

```yaml
deployment:
  replicas: 1
global:
  sendAnonymousUsage: false
service:
  # the addresses clients outside the cluster reach Thorium at
  externalIPs:
    - 1.2.3.4
    - 1.2.3.5
```

Large file uploads and downloads can run longer than Traefik's default timeouts. To remove the
read and write timeouts on both entrypoints, also add:

```yaml
ports:
  web:
    transport:
      respondingTimeouts:
        readTimeout: 0
        writeTimeout: 0
        idleTimeout: 600
  websecure:
    transport:
      respondingTimeouts:
        readTimeout: 0
        writeTimeout: 0
        idleTimeout: 600
```

Run `helm show values traefik/traefik --version 37.1.0` for every setting.

## 3) Install Traefik

```bash
helm install traefik traefik/traefik --version 37.1.0 -n traefik --create-namespace \
  -f traefik-values.yaml --wait
kubectl -n traefik get pods
# NAME                       READY   STATUS    RESTARTS   AGE
# traefik-HASH               1/1     Running   0          1m
```

Change the values later with
`helm upgrade traefik traefik/traefik --version 37.1.0 -n traefik -f traefik-values.yaml`.

## Thorium settings

With `operator.ingress.type: traefik`, the `thorium` chart creates, in the thorium namespace:

- an `IngressRoute` named `thorium-ingress` on the `websecure` entrypoint that sends
  `operator.ingress.host` (every host when empty) to the `thorium-api` Service on port 80, and
  `operator.ingress.registryHost` to the chart's registry when set;
- a `TLSOption` named `tls12` (TLS 1.2 and 1.3);
- with `operator.ingress.tls.secretName` and `operator.ingress.tls.traefikDefaultStore: true`,
  the cluster's `default` `TLSStore`, which makes that certificate Traefik's default. Traefik
  allows only one default `TLSStore` per cluster, so set `traefikDefaultStore: false` when
  something else already defines it.

Create the TLS Secret in the thorium namespace before installing:

```bash
kubectl -n thorium create secret tls thorium-tls --cert=thorium.crt --key=thorium.key
```

See [Configure the Helm Charts](./helm-configuration.md) for the other ingress settings.
