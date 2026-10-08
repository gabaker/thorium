# Deploy the Operator Without Helm

This page explains how to run the Thorium operator and a `ThoriumCluster` without Helm: for sites
that can't use Helm, and for a deployment made by the minithor or megathor scripts before the Helm
charts that stays outside Helm. It lists everything the thorium chart would otherwise provide and
gives the manifests to provide it yourself. Read [Deployment Concepts](./concepts.md) first, and
set up the backing services as described in [Bring Your Own Infrastructure](./infrastructure.md)
(or install the thorium chart's backing services and operator separately).

The examples use the thorium namespace `thorium`, the `ThoriumCluster` name `thorium`, and
`VERSION` for the Thorium version (an image tag of
`ghcr.io/cisagov/thorium/infrastructure/thorium`).

## Choose an approach

- **Render the chart and apply it with your own tooling** (recommended). `helm template` produces
  every manifest on this page, kept in step with each release:

  ```bash
  helm template thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
    -n thorium -f site.yaml > thorium.yaml
  ```

  Without cluster access the chart can't read or keep generated credentials, so every credential
  must be set in your values (see [Credentials](./helm-configuration.md#credentials)), and the
  chart's install-time guards (one deployment per cluster, an existing `ThoriumCluster` in the
  namespace) don't run. Apply the CRD-backed `ThoriumCluster` after the operator has started (see
  [The CRD](#the-crd)).
- **Write the manifests yourself**, following the rest of this page.

## What the chart provides

| Piece | Section |
|-------|---------|
| The thorium namespace (and the backing services' namespaces) | create them with `kubectl create namespace` |
| Service accounts `thorium-operator` (the operator) and `thorium` (the components) and their RBAC | [RBAC](#rbac) |
| Image pull secrets for the Thorium image | [Image pull secrets](#image-pull-secrets) |
| The `banner` ConfigMap | [Banner](#banner) |
| The operator Deployment, with the Elasticsearch CA mount | [Operator Deployment](#operator-deployment) |
| The credentials Secret and the admin Secret | [Config Secret](#config-secret) |
| The `ThoriumCluster` | [ThoriumCluster](#thoriumcluster) |
| The ingress | [Ingress](#ingress) |
| The toolbox import | [Toolbox](#toolbox) |
| The uninstall hook | [Uninstalling](#uninstalling) |
| The backing services and their Thorium users | [Bring Your Own Infrastructure](./infrastructure.md) |

## RBAC

The operator runs as its own service account, with cluster rules for its CRD and the nodes, and
namespaced rules for the resources it manages in the thorium namespace. The components run as the
`thorium` service account, which only the k8s scaler uses to call the Kubernetes API (it manages
namespaces, job pods, Secrets, ConfigMaps, and network policies in every group namespace).

```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: thorium-operator
  namespace: thorium
---
apiVersion: v1
kind: ServiceAccount
metadata:
  name: thorium
  namespace: thorium
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: thorium-operator-cluster
rules:
  # the operator applies the ThoriumCluster CRD on startup (server-side apply, which RBAC
  # authorizes as create when the CRD is missing) and waits for it to be established
  - apiGroups: ["apiextensions.k8s.io"]
    resources: ["customresourcedefinitions"]
    verbs: ["get", "list", "watch", "create"]
  # it only ever changes its own CRD
  - apiGroups: ["apiextensions.k8s.io"]
    resources: ["customresourcedefinitions"]
    resourceNames: ["thoriumclusters.sandia.gov"]
    verbs: ["update", "patch"]
  # watching, labelling, and provisioning the nodes the scaler schedules on
  - apiGroups: [""]
    resources: ["nodes"]
    verbs: ["get", "list", "watch", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: thorium-operator-cluster
subjects:
  - kind: ServiceAccount
    name: thorium-operator
    namespace: thorium
roleRef:
  kind: ClusterRole
  name: thorium-operator-cluster
  apiGroup: rbac.authorization.k8s.io
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: thorium-operator
  namespace: thorium
rules:
  - apiGroups: ["sandia.gov"]
    resources: ["thoriumclusters"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["sandia.gov"]
    resources: ["thoriumclusters/status"]
    verbs: ["get", "update", "patch"]
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  # node provision pods (deletecollection is used when a ThoriumCluster is deleted)
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["get", "list", "watch", "create", "patch", "delete", "deletecollection"]
  - apiGroups: [""]
    resources: ["services"]
    verbs: ["get", "create", "update", "patch", "delete"]
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
  - apiGroups: ["apps"]
    resources: ["deployments"]
    verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: thorium-operator
  namespace: thorium
subjects:
  - kind: ServiceAccount
    name: thorium-operator
    namespace: thorium
roleRef:
  kind: Role
  name: thorium-operator
  apiGroup: rbac.authorization.k8s.io
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: thorium-components
rules:
  - apiGroups: [""]
    resources: ["namespaces"]
    verbs: ["list", "create"]
  - apiGroups: [""]
    resources: ["nodes"]
    verbs: ["list"]
  - apiGroups: [""]
    resources: ["pods"]
    verbs: ["list", "create", "delete"]
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get", "list", "create", "update"]
  - apiGroups: [""]
    resources: ["configmaps"]
    verbs: ["list", "create"]
  - apiGroups: ["networking.k8s.io"]
    resources: ["networkpolicies"]
    verbs: ["list", "create", "delete"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: thorium-components
subjects:
  - kind: ServiceAccount
    name: thorium
    namespace: thorium
roleRef:
  kind: ClusterRole
  name: thorium-components
  apiGroup: rbac.authorization.k8s.io
```

These match the rules the chart renders for its release (where the cluster-scoped objects are
named `<thorium namespace>-operator` and `<thorium namespace>-components`). Check them against
each new release:

```bash
helm template thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION -n thorium \
  --set secrets.renderOnly=true --show-only charts/operator/templates/rbac.yaml
```

An operator started without `--namespace` watches every namespace and needs the namespaced rules
as a ClusterRole bound cluster-wide instead of the Role.

## Image pull secrets

If the Thorium image comes from a registry that needs credentials, create a
`kubernetes.io/dockerconfigjson` Secret, reference it from both service accounts (or the operator
Deployment), and list it in the `ThoriumCluster`'s `image_pull_secrets` so every component and
node provision pod uses it:

```bash
kubectl -n thorium create secret docker-registry thorium-image-pull --from-file=.dockerconfigjson=$HOME/.docker/config.json
kubectl -n thorium patch serviceaccount thorium-operator -p '{"imagePullSecrets":[{"name":"thorium-image-pull"}]}'
kubectl -n thorium patch serviceaccount thorium -p '{"imagePullSecrets":[{"name":"thorium-image-pull"}]}'
```

Don't name it `registry-token`: the operator writes that Secret itself from the `ThoriumCluster`'s
`registry_auth` (tool registries).

## Banner

The API mounts the login banner from the optional `banner` ConfigMap (key `banner.txt`):

```bash
kubectl -n thorium create configmap banner --from-literal=banner.txt="Welcome to Thorium"
```

## Operator Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: operator
  namespace: thorium
  labels:
    app: operator
spec:
  replicas: 1
  # the operator has no leader election, so the old pod must stop before its replacement starts
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: operator
  template:
    metadata:
      labels:
        app: operator
    spec:
      serviceAccountName: thorium-operator
      securityContext:
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: operator
          # the image's entrypoint is `./thorium-operator operate`
          image: ghcr.io/cisagov/thorium/infrastructure/thorium:<VERSION>
          imagePullPolicy: Always
          args: ["--namespace", "thorium"]
          securityContext:
            allowPrivilegeEscalation: false
            capabilities:
              drop: [ALL]
          resources:
            limits: { cpu: 256m, memory: 1Gi }
```

- `--namespace` (or the environment variable `THORIUM_OPERATOR_NAMESPACE`) limits the operator to
  `ThoriumCluster`s in the thorium namespace. `--url` is only for running the operator outside the
  cluster during development.
- Run exactly one operator for a `ThoriumCluster`, and never next to an operator from before the
  Helm charts, which watched every namespace.
- Behind a proxy, set `http_proxy`/`https_proxy` (and their upper-case forms), and set
  `no_proxy`/`NO_PROXY` to at least
  `localhost,127.0.0.1,.svc,.svc.cluster.local,cluster.local` plus the cluster's service and pod
  CIDRs, so the operator reaches the API, the backing services, and the Kubernetes API directly.
- With `spec.elastic_ca_secret`, mount the same Secret in the operator at the path the
  components use:

  ```yaml
  # in the operator container
  volumeMounts:
    - name: elastic-ca
      mountPath: /etc/thorium/elastic-ca
      readOnly: true
  # in the pod spec
  volumes:
    - name: elastic-ca
      secret:
        secretName: elastic-ca
        items:
          - key: ca.crt
            path: ca.crt
  ```

- With `KUBECONFIG` set, the operator builds a client for every context in that kube config;
  otherwise it uses its service account and calls the cluster `kubernetes-admin@cluster.local`,
  the name the `ThoriumCluster`'s `thorium.scaler.k8s` config must use for it.

## The CRD

The operator applies the `ThoriumCluster` CRD with server-side apply when it starts, so start the
operator before applying a `ThoriumCluster`. To apply or inspect the CRD without starting it, print
it from the image:

```bash
docker run --rm --entrypoint /app/thorium-operator ghcr.io/cisagov/thorium/infrastructure/thorium:$VERSION crd > thoriumcluster-crd.yaml
kubectl apply --server-side --field-manager thorium_cluster_apply -f thoriumcluster-crd.yaml
```

The API also serves the binary at `<THORIUM_URL>/api/binaries/linux/x86-64/thorium-operator`.

## Config Secret

Keep every credential in a Secret in the thorium namespace and name it in the `ThoriumCluster`'s
`config_secrets`; the operator merges it over `spec.config` (see
[Configuration and credentials](./concepts.md#configuration-and-credentials)). The chart renders
this layout:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: thorium-config
  namespace: thorium
type: Opaque
stringData:
  thorium.yml: |
    thorium:
      secret_key: "<a long random string; never change it once users exist>"
      s3:
        access_key: "<S3 access key>"
        secret_token: "<S3 secret key>"
    redis:
      password: "<Redis password>"
    scylla:
      auth:
        username: "thorium"
        password: "<Scylla password>"
    elastic:
      username: "thorium"
      password: "<Elasticsearch password>"
---
apiVersion: v1
kind: Secret
metadata:
  name: thorium-admin
  namespace: thorium
type: Opaque
stringData:
  username: "admin"
  password: "<initial admin password>"
```

The document is plain YAML: write tracing endpoints in map form (`external: {Grpc: {endpoint:
..., level: Info}}`), not with YAML tags such as `!Grpc`, and give every value the type Thorium
expects. Config and bootstrap Secrets can't use a name the operator manages: `thorium`, `keys`,
`keys-kaboom`, `docker-skopeo`, `registry-token`, `thorium-image-pull`, or any name ending in
`-pass`.

## ThoriumCluster

```yaml
apiVersion: sandia.gov/v1
kind: ThoriumCluster
metadata:
  name: thorium
  namespace: thorium
spec:
  registry: ghcr.io/cisagov/thorium/infrastructure/thorium
  version: "<VERSION>"
  image_pull_policy: Always
  # image_pull_secrets: [thorium-image-pull]
  components:
    api:
      replicas: 1
      resources: { cpu: 0, memory: 0 }
    scaler:
      service_account: true
      resources: { cpu: 0, memory: 0 }
    search_streamer:
      resources: { cpu: 0, memory: 0 }
    event_handler:
      resources: { cpu: 0, memory: 0 }
  config:
    elastic:
      node: https://elastic-es-http.elastic.svc.cluster.local:9200
      # ECK's self-signed certificate; use elastic_ca_secret below to verify one instead
      insecure_certificates: true
      results:
        samples: thorium_sample_results
        repos: thorium_repo_results
      tags:
        samples: thorium_sample_tags
        repos: thorium_repo_tags
    redis:
      host: redis.redis.svc.cluster.local
      port: 6379
    scylla:
      nodes: [scylla-client.scylla.svc.cluster.local]
      replication: 1
    thorium:
      cors:
        insecure: false
      s3:
        endpoint: http://seaweedfs.seaweedfs.svc.cluster.local:8333
        region: us-east-1
        use_path_style: true
      namespace_blacklist: [thorium, redis, scylla, elastic, seaweedfs, quickwit, jaeger,
        infra-operators, kube-system, kube-public, kube-node-lease, default,
        scylla-operator, cert-manager, elastic-system]
      scaler:
        k8s:
          primary_cluster: kubernetes-admin@cluster.local
          clusters:
            kubernetes-admin@cluster.local:
              alias: thorium
              # the nodes Thorium may run jobs on; empty means every node
              nodes: []
  config_secrets:
    - name: thorium-config
      key: thorium.yml
  bootstrap:
    admin:
      secret:
        name: thorium-admin
        username_key: username
        password_key: password
    # create Thorium's Scylla role with the default cassandra superuser of a fresh Scylla
    # (or name a Secret with superuser credentials in admin_secret)
    scylla:
      drop_default_role: true
  # elastic_ca_secret:
  #   name: elastic-ca
  #   key: ca.crt
  upgrade:
    auto_target_dev: false
```

Adjust the endpoints to your backing services. Check the resource with a server-side dry run once
the CRD exists, then apply it and watch it:

```bash
kubectl apply --dry-run=server -f thoriumcluster.yaml
kubectl apply -f thoriumcluster.yaml
kubectl -n thorium get thoriumcluster -o wide
```

To start from what the chart would generate, render it:

```bash
helm template thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION -n thorium \
  --set secrets.renderOnly=true -f site.yaml --show-only charts/operator/templates/thoriumcluster.yaml
```

[ThoriumCluster and Operator Reference](./thoriumcluster.md) lists every field.

## Ingress

Route `/` to the `thorium-api` Service on port 80 (the API serves the UI too). With nginx, allow
large uploads:

```yaml
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: thorium
  namespace: thorium
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "0"
spec:
  ingressClassName: nginx
  tls:
    - secretName: thorium-tls
      hosts: [thorium.example.com]
  rules:
    - host: thorium.example.com
      http:
        paths:
          - path: /
            pathType: Prefix
            backend:
              service:
                name: thorium-api
                port:
                  number: 80
```

With Traefik:

```yaml
apiVersion: traefik.io/v1alpha1
kind: TLSOption
metadata:
  name: tls12
  namespace: thorium
spec:
  minVersion: VersionTLS12
  maxVersion: VersionTLS13
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata:
  name: thorium-ingress
  namespace: thorium
spec:
  entryPoints:
    - websecure
  routes:
    - match: Host(`thorium.example.com`)
      kind: Rule
      services:
        - name: thorium-api
          port: 80
  tls:
    options:
      name: tls12
    secretName: thorium-tls
```

## Toolbox

Import a [toolbox](../../developers/toolbox.md) with Thorctl once the deployment is `Ready` (see
`thorctl toolbox import --help`).

## Verifying

```bash
kubectl -n thorium wait thoriumcluster/thorium --for=jsonpath='{.status.phase}'=Ready --timeout=30m
kubectl -n thorium port-forward svc/thorium-api 8080:80
```

Log in at `http://localhost:8080` with the admin from the `thorium-admin` Secret. A fresh install
records the operator's latest revision. See [Troubleshooting Deployments](./troubleshooting.md) if
the phase is `Error`.

## Upgrading

The operator image and `spec.version` (the components' image tag) are independent without Helm:

1. Back up (see [Backups](./operate.md#backups)), and compare the RBAC for the new release with
   yours (see [RBAC](#rbac)).
2. Change the operator Deployment's image to the new version. The `Recreate` strategy stops the old
   operator first; the new one applies its CRD when it starts.
3. Set `spec.version` to the same version. If the deployment reports `UpgradeRequired`, set
   `spec.upgrade.target_revision` (and any approvals) in the same edit; see
   [Upgrading Thorium](./upgrades.md).

## Uninstalling

Delete the `ThoriumCluster` while the operator still runs, so it removes its node labels,
provision pods, component Deployments, Services, and the Secrets and ConfigMaps it created; then
delete the operator, its RBAC, and the service accounts. The data, the namespaces, the
`thorium-operator-pass` Secret, and the `thorium-upgrade-state` ConfigMap are kept (see
[Uninstalling](./operate.md#uninstalling) for removing them):

```bash
kubectl -n thorium delete thoriumcluster thorium --wait
kubectl -n thorium delete deployment operator
```

## Moving a pre-Helm deployment without Helm

A deployment made by the minithor or megathor scripts before the Helm charts can keep running
outside Helm with a current operator. The current operator refuses a `ThoriumCluster` that holds
its credentials inline without either the `thorium.sandia.gov/allow-inline-config: "true"`
annotation or `config_secrets`, and records the starting revision the first time it sees the
namespace (see [Where a deployment starts](./upgrades.md#where-a-deployment-starts)). This
procedure annotates the cluster so the operator records revision `2026-10-v01` and runs every
upgrade step, and moves the credentials into a config Secret afterwards. The examples use the
pre-Helm name `dev` in the namespace `thorium`; use `<prefix>-thorium` for a deployment with a
namespace prefix.

1. **Retire other instances and back up.** A cluster runs one Thorium deployment, so retire every
   other pre-Helm instance first (see
   [Retiring other pre-Helm instances](./convert-to-helm.md#retiring-other-pre-helm-instances)).
   Back up Redis, Scylla, and S3 with `thoradm backup new`, and save the `ThoriumCluster` and the
   old operator before the new CRD prunes fields the old one kept:

   ```bash
   kubectl -n thorium get thoriumcluster dev -o json > dev-thoriumcluster.json
   kubectl get deployment -A -l app=operator -o yaml > old-operators.yaml
   ```

2. **Stop every old operator** in the cluster. They watched every namespace and would fight the new
   one:

   ```bash
   kubectl get deployment -A -l app=operator
   # for each one listed, in its namespace
   kubectl -n <namespace> scale deployment operator --replicas=0
   kubectl -n <namespace> wait --for=delete pod -l app=operator --timeout=120s
   ```

3. **Annotate the `ThoriumCluster`** so the operator accepts its inline credentials. Leave
   `config_secrets` empty until the operator has recorded the revision: a `ThoriumCluster` with
   `config_secrets` and no recorded revision is taken for a Helm deployment and starts at
   `2026-10-v02`, skipping that revision's checks.

   ```bash
   kubectl -n thorium annotate thoriumcluster dev thorium.sandia.gov/allow-inline-config=true
   ```

   Instead of relying on the annotation for the starting revision, you can record it yourself
   before starting the new operator, the same way `convert-to-helm.sh` does:

   ```bash
   kubectl -n thorium create -f - <<EOF
   apiVersion: v1
   kind: ConfigMap
   metadata:
     name: thorium-upgrade-state
     namespace: thorium
     labels:
       thorium.sandia.gov/state-revision: 2026-10-v01
   data:
     state.json: '{"schema_version":1,"revision":"2026-10-v01","applied":[{"revision":"2026-10-v01","step":"record-revision","outcome":"Done","at":"$(date -u +%Y-%m-%dT%H:%M:%SZ)","by":"admin"}]}'
   EOF
   ```

4. **Elasticsearch certificates.** The pre-Helm operator stored `insecure_certificates: false`
   in `spec.config`, but the pre-Helm components never verified Elasticsearch's certificate; the
   current ones do. For ECK's self-signed certificate, skip verification, or verify against a CA
   with `spec.elastic_ca_secret` and the operator mount (see
   [Operator Deployment](#operator-deployment)):

   ```bash
   kubectl -n thorium patch thoriumcluster dev --type merge \
     -p '{"spec":{"config":{"elastic":{"insecure_certificates":true}}}}'
   ```

5. **Image pull credentials.** The pre-Helm scripts stored the Thorium image's pull credentials in
   a Secret named `registry-token`, which the current operator owns and rewrites from
   `registry_auth`. If the Thorium image needs credentials, copy them to another Secret (see
   [Image pull secrets](#image-pull-secrets)) and add it to `spec.image_pull_secrets`.

6. **Apply the RBAC** from [RBAC](#rbac). Its names don't collide with the pre-Helm
   `thorium-operator` ClusterRole, which the old scaler keeps using until the new operator
   replaces it.

7. **Replace the operator.** Delete the old operator Deployment (already scaled to 0) and create the
   one from [Operator Deployment](#operator-deployment) with the new image; it applies the new CRD
   when it starts:

   ```bash
   kubectl -n thorium delete deployment operator
   kubectl apply -f operator.yaml
   ```

8. **Confirm the revision.** The `ThoriumCluster` goes to `UpgradeRequired` at revision
   `2026-10-v01`:

   ```bash
   kubectl -n thorium get thoriumcluster -o wide
   kubectl -n thorium get configmap thorium-upgrade-state \
     -o jsonpath='{.metadata.labels.thorium\.sandia\.gov/state-revision}'; echo
   ```

9. **Upgrade.** In one edit, set the components' version and the target revision; the operator
   runs the `2026-10-v02` steps and then rolls out the new components:

   ```bash
   kubectl -n thorium patch thoriumcluster dev --type merge \
     -p '{"spec":{"version":"<VERSION>","upgrade":{"target_revision":"2026-10-v02"}}}'
   ```

   If the `elastic-reindex-keyword-mappings` step blocks, approve it in `spec.upgrade.approvals`
   and follow [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch). Wait for
   `Ready`.

10. **Move the credentials into a config Secret** (recommended once the revision is
    recorded). Create a Secret with the credentials from `spec.config` in the
    [Config Secret](#config-secret) layout (add any other secret, such as an LDAP bind password),
    name it in `config_secrets`, and remove the inline copies and the annotation:

    ```bash
    kubectl -n thorium patch thoriumcluster dev --type merge \
      -p '{"spec":{"config_secrets":[{"name":"thorium-config","key":"thorium.yml"}]}}'
    kubectl -n thorium patch thoriumcluster dev --type json -p '[
      {"op":"remove","path":"/spec/config/thorium/secret_key"},
      {"op":"remove","path":"/spec/config/thorium/s3/access_key"},
      {"op":"remove","path":"/spec/config/thorium/s3/secret_token"},
      {"op":"remove","path":"/spec/config/redis/password"},
      {"op":"remove","path":"/spec/config/scylla/auth"},
      {"op":"remove","path":"/spec/config/elastic/username"},
      {"op":"remove","path":"/spec/config/elastic/password"}]'
    kubectl -n thorium annotate thoriumcluster dev thorium.sandia.gov/allow-inline-config-
    ```

    Remove only the paths your `spec.config` has; a JSON patch fails on a missing path.

11. **Replace the ingress.** The pre-Helm ingress (an nginx `thorium-default-ingress`, or a Traefik
    `thorium-ingress` with a `ui-prefix-prepend` middleware) routes through a `/ui` prefix. Replace
    it with a route of `/` to `thorium-api:80` (see [Ingress](#ingress)).

12. **Remove the pre-Helm leftovers** once the new components run: the
    `thorium-operator-binding` ClusterRoleBinding (`thorium-operator-binding-<prefix>` for a
    deployment with a prefix), the `thorium-operator` ClusterRole once nothing else binds it, and
    the unused `thorium-account-token` Secret:

    ```bash
    kubectl delete clusterrolebinding thorium-operator-binding
    kubectl get clusterrolebinding -o json | jq -r '.items[] | select(.roleRef.name == "thorium-operator") | .metadata.name'
    kubectl delete clusterrole thorium-operator          # when the line above prints nothing
    kubectl -n thorium delete secret thorium-account-token
    ```

**Rolling back** before step 9: scale the new operator to 0 and delete its Deployment, delete the
`thorium-upgrade-state` ConfigMap, recreate the old operator from `old-operators.yaml` (it applies
its own CRD when it starts), and restore the `ThoriumCluster` from `dev-thoriumcluster.json` if
fields were pruned. After step 9, restore from your backups.
