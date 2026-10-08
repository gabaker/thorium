# Converting a Pre-Helm Deployment

A deployment made by the minithor or megathor scripts before the Helm charts must be converted
once before a current operator, chart, minithor, or megathor manages it. This page explains how to
recognize such a deployment and convert it with `convert-to-helm.sh` so the Helm charts take it
over, keeping every database, credential, and running component. To keep the deployment outside
Helm instead, see
[Moving a pre-Helm deployment without Helm](./deploy-thorium.md#moving-a-pre-helm-deployment-without-helm).

## Recognizing a pre-Helm deployment

A pre-Helm deployment has:

- a `ThoriumCluster` (usually named `dev`) with every credential inline in `spec.config` and no
  `config_secrets`;
- an operator Deployment named `operator` running as the `thorium` service account, bound to a
  `thorium-operator` ClusterRole, and watching every namespace;
- databases deployed by the scripts outside any chart (for minithor's default instance,
  Elasticsearch in `elastic-system`).

A current operator leaves it untouched and sets the phase to `Error`:

```
This ThoriumCluster was deployed by the minithor or megathor scripts from before the Helm charts
and has not been converted (spec.config holds thorium.secret_key inline; the ThoriumCluster has
no config_secrets). The operator changed nothing. ...
```

The thorium chart refuses to install next to it ("namespace thorium already holds ThoriumCluster
dev, which Helm doesn't manage. ..."), and minithor and megathor refuse to deploy over it.

## Before you start

- **Tools**: `kubectl` (with the cluster's context), `helm` 3.8 or newer, and `jq`.
- **The script and the chart**: `deploy/charts/scripts/convert-to-helm.sh` from the Thorium
  source repository at the release you will deploy. It renders the thorium chart to find
  collisions and applies the chart's `ThoriumCluster` CRD, so give it the same chart you will
  install: by default the chart next to the script in the checkout, or
  `--chart oci://ghcr.io/cisagov/thorium/charts/thorium --chart-version $VERSION`.
- **Backups**: back up Redis, Scylla, and S3 with `thoradm backup new` (see
  [Backups](./operate.md#backups)). Elasticsearch is rebuilt from Scylla by the search streamer,
  so it needs no backup of its own.
- **One deployment per cluster**: a Kubernetes cluster runs one Thorium deployment. If the
  cluster holds several pre-Helm instances (minithor's `--instance` or several megathor
  `namespace_prefix` deployments), choose the one to keep and
  [retire the others](#retiring-other-pre-helm-instances) before deploying the chart.
- **The admin login**: the chart's `thorium-admin` Secret should hold an existing admin's login,
  which the toolbox import and minithor use. Pass it to the script (see below); minithor's default
  is `test`/`INSECURE_DEV_PASSWORD`, and a login minithor saved with `--rand-password` (in the
  `minithor-credentials` Secret) is read automatically. Without one the chart's admin bootstrap is
  turned off, and the `thorium-admin` Secret the chart still renders names no real user.

## Procedure

1. **Preview** the conversion. A dry run writes the chart values and lists every change without
   making any:

   ```bash
   export THORIUM_ADMIN_PASSWORD='<the existing admin password>'
   deploy/charts/scripts/convert-to-helm.sh --admin-user <admin> --dry-run
   ```

   Add `--namespace-prefix <prefix>` for a deployment in `<prefix>-thorium`, and pass the values
   the deployment will also be deployed with (minithor's or megathor's) with `--values` so the
   collision check sees every object they render. The check renders them before the converted
   values, which win as they do when deploying. Review `thorium-converted-values.yaml`.

2. **Convert** by running it again without `--dry-run`. It asks for confirmation (pass `--yes` to
   skip it) and writes the values file again, so make any edits to it afterwards. Thorium's
   components keep running, unmanaged, and the legacy ingress is gone until the chart is
   deployed.

3. **Deploy the chart** with the values it wrote, after any other values files so they win:

   ```bash
   helm upgrade --install thorium oci://ghcr.io/cisagov/thorium/charts/thorium --version $VERSION \
     -n thorium -f site.yaml -f thorium-converted-values.yaml
   ```

   With minithor or megathor, see [minithor](#minithor) and [megathor](#megathor) below.

4. **Upgrade**: the operator records revision `2026-10-v01` and waits in `UpgradeRequired` until
   a target is set. Set `operator.cluster.upgrade.targetRevision` (minithor sets `autoTargetDev`)
   and approve the reindex step if the deployment needs it (see
   [Upgrading Thorium](./upgrades.md) and [Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch)).
   The operator then checks the backends and deploys the new components.

5. **Clean up** once the `ThoriumCluster` reports `Ready`: remove the RBAC kept for the old
   scaler.

   ```bash
   deploy/charts/scripts/convert-to-helm.sh --cleanup
   ```

   `--cleanup` refuses to run until the recorded revision is past `2026-10-v01` and the phase is
   `Ready`. Delete the `thorium-legacy-cluster` Secret once you no longer need to roll back.

Never recover from a failed chart install with `helm uninstall` (without `--no-hooks`) or
`helm install --atomic`: the uninstall hook deletes the `ThoriumCluster`, and once the operator
has finished the upgrade its cleanup removes every component (a cluster still waiting in
`UpgradeRequired` has no finalizer yet, so deleting it leaves the components running unmanaged).
Fix the problem and run `helm upgrade --install` again instead.

### Options

| Option | Effect |
|--------|--------|
| `--namespace-prefix <prefix>` | The deployment's namespace prefix: minithor's `--instance` or megathor's `namespace_prefix` (default none, namespace `thorium`) |
| `--context <name>` | The kubectl context to use (default the current context) |
| `--release <name>` | The Helm release that will own Thorium (default `thorium`) |
| `--chart <path\|oci-ref>` | The thorium chart that will be deployed (default the chart next to the script) |
| `--chart-version <version>` | The chart version for an `oci://` chart |
| `--values <file>` | Extra values the deployment will use, rendered before the converted values (which win, as when deploying) when looking for collisions (repeatable) |
| `--values-out <file>` | Where to write the chart values (default `./thorium-converted-values.yaml`) |
| `--admin-user <name>` | The existing Thorium admin to record in `thorium-admin` |
| `--admin-password <password>` | Its password. A command-line argument is visible to every local user in the process list, so prefer the `THORIUM_ADMIN_PASSWORD` environment variable or `--admin-password-stdin` |
| `--admin-password-stdin` | Read the admin password from the first line of stdin (then also pass `--yes` or `--dry-run`, since stdin can't confirm) |
| `--dry-run` | Write the values file and show what would change, changing nothing in the cluster |
| `--yes` | Don't ask for confirmation |
| `--cleanup` | After the operator finished upgrading the converted deployment, remove the legacy RBAC kept for its old scaler |

### The values it writes

The values file holds credentials and is written readable only by you. It:

- treats every existing database as an external service (`global.managed.*: false`) and points
  `operator.backends` at their endpoints, with Scylla's replication and the S3 region and
  path-style setting carried over;
- carries every credential over to `secrets.credentials` (Thorium's secret key, Redis, Scylla,
  Elasticsearch, and S3), and the admin login when one was given;
- turns off Quickwit (`global.quickwit.enabled: false`), the chart's registry, the toolbox
  import, and the Scylla and Elasticsearch bootstrap; the existing Quickwit, Jaeger, and registry
  aren't managed by the chart;
- carries Thorium's tracing endpoint over to `operator.backends.tracing.grpcEndpoint`;
- sets `operator.backends.elastic.insecureCertificates: true`, since the pre-Helm search streamer
  never verified Elasticsearch's certificate, unless the old config set `elastic.cert_validation`;
- keeps the `ThoriumCluster` name, components, pull policy, tool registry credentials,
  `image_pull_secrets`, image repository, and banner; the index names go to
  `global.elasticIndices`, the namespace blacklist to `operator.cluster.namespaceBlacklist`, and
  the primary (or first) scaler cluster to `operator.cluster.scaler.k8s`;
- carries every other non-credential setting over in `operator.cluster.config`;
- carries minithor's nginx ingress over (class, host, and TLS Secret), or, without one,
  megathor's Traefik `thorium-ingress` IngressRoute: its `Host(...)` match becomes
  `operator.ingress.host`, its TLS Secret (or that of a `default` TLSStore in the namespace)
  `operator.ingress.tls.secretName`, and `traefikDefaultStore` is on when that TLSStore named the
  same Secret and no Helm release owns it;
- carries the pull login of the pre-Helm `--docker-config` (minithor) or
  `thorium_image_pull_secret` (megathor) `registry-token` Secret into
  `operator.createImagePullSecret` and `operator.dockerConfigJson` when the `ThoriumCluster` has
  no `registry_auth`. The operator renders its own `registry-token` from `registry_auth` and
  deletes a Secret of that name when its `ThoriumCluster` is deleted, so the login moves to the
  chart's `thorium-image-pull` Secret rather than being referenced by name.

The image tag isn't carried over, so the chart's own version is deployed. If the old config set
`elastic.cert_validation` to a CA file, that file isn't mounted by the chart: put the CA in a
Secret, set `operator.backends.elastic.caSecret`, and remove `cert_validation` from
`operator.cluster.config.elastic` (see
[Elasticsearch certificates](./helm-configuration.md#elasticsearch-certificates)). Move any other
secret left in `operator.cluster.config` (such as an LDAP bind password) into a
[config Secret](./helm-configuration.md#thoriumcluster-settings).

### What the script changes

1. Stops every pre-Helm Thorium operator in the cluster (they watched every namespace and would
   fight the new one). A shared operator also leaves the cluster's other pre-Helm instances
   unmanaged.
2. Saves the `ThoriumCluster` (with the legacy ingress objects and pull login it carried over) in
   the `thorium-legacy-cluster` Secret,
   applies the chart's `ThoriumCluster` CRD, and records revision `2026-10-v01` in the
   `thorium-upgrade-state` ConfigMap.
3. Moves the old scaler's `thorium-operator` ClusterRole aside (to `thorium-legacy-components`,
   rebinding its subjects) when the chart renders a ClusterRole with the same name, so the old
   scaler keeps its permissions until it is replaced.
4. Hands the objects the chart renders identically (the namespaces, the `thorium` service
   account, and the `banner` ConfigMap) to the Helm release, and deletes the ones the chart
   replaces: the old operator, the legacy ingress objects (`thorium-default-ingress`,
   `thorium-ingress`, `ui-prefix-prepend`, and a `default` TLSStore no Helm release owns), and the
   `thorium-account-token` Secret, which nothing uses. Jaeger keeps running, since the converted
   values turn the chart's own tracing off.
5. Deletes the `ThoriumCluster` without letting anything clean up after it, so the chart creates
   it again; its components keep running and are adopted by the new operator.

It refuses to continue when the chart would collide with any other object outside Helm. Every
step checks whether it already ran, so a failed run can be run again; once the chart has taken the
`ThoriumCluster` over, a run only writes the values file again.

## minithor

Pass the old `--instance` as `--namespace-prefix`, and the minikube profile's context with
`--context` (the context is named after the profile; the script otherwise uses the current kubectl
context, which may belong to another profile). Then deploy with the current minithor:

```bash
deploy/charts/scripts/convert-to-helm.sh --context minikube --admin-user test --dry-run   # with THORIUM_ADMIN_PASSWORD set
deploy/charts/scripts/convert-to-helm.sh --context minikube --admin-user test
minithor deploy --skip-operators --no-toolbox --values thorium-converted-values.yaml
```

minithor sets `autoTargetDev`, so the operator upgrades the converted deployment at once. Its
databases stay where the old minithor put them. Those namespaces have the names the current
minithor uses, except the default instance's Elasticsearch in `elastic-system`, which
`expose --dev` and `get-config --local` skip; `cleanup` leaves what the old minithor created
outside them. See
[Existing (pre-Helm) minithor deployments](./minithor.md#existing-pre-helm-minithor-deployments)
for why `--skip-operators` and `--no-toolbox` are needed and for the leftovers to remove by hand.

## megathor

Run the conversion (with `--namespace-prefix` when `namespace_prefix` is set), then run the
playbook with:

- `thorium_deployment_name: dev` (the old `ThoriumCluster` name);
- `infra_operators_enabled: false` (the old operators keep running);
- the converted values file last in `thorium_values_files`;
- `thorium_upgrade_target_revision: 2026-10-v02`, and `thorium_upgrade_approvals` if the reindex
  step needs it.

The converted values are applied after megathor's own, so their admin bootstrap setting wins.
Pass an existing admin's login to the conversion (`--admin-user` with `THORIUM_ADMIN_PASSWORD` or
`--admin-password-stdin`); without one the bootstrap stays off, megathor's generated
`thorium_admin_password` creates no user, and the playbook's final message says that no admin user
was created.

## Retiring other pre-Helm instances

Only one Thorium deployment can run on a Kubernetes cluster, and the thorium chart and minithor
refuse to install while a `ThoriumCluster` exists in any other namespace. Remove every pre-Helm
instance you don't keep before deploying the chart, preferably before converting (while the old
operator still runs). For each instance with prefix `<p>` (drop the `<p>-` for an instance
without a prefix, such as minithor's default instance):

1. Back it up if its data matters: write its config from the `thorium` Secret in `<p>-thorium`
   (see [Operating a Deployment](./operate.md#where-things-are)) and run `thoradm backup new` with
   it.
2. Delete its `ThoriumCluster`. While the old operator runs, it cleans up the instance's
   components:

   ```bash
   kubectl -n <p>-thorium delete thoriumcluster dev
   ```

   After the conversion stopped the old operators, nothing removes the finalizer; remove it first:

   ```bash
   kubectl -n <p>-thorium patch thoriumcluster dev --type merge -p '{"metadata":{"finalizers":null}}'
   kubectl -n <p>-thorium delete thoriumcluster dev
   ```

3. Delete its namespaces, which deletes its volumes and data. Delete the database resources first
   while the Scylla and ECK operators still run, so their finalizers don't hold the namespaces:

   ```bash
   kubectl -n <p>-scylla delete scyllacluster --all
   kubectl -n <p>-elastic delete elasticsearch,kibana --all
   kubectl delete namespace <p>-thorium <p>-redis <p>-scylla <p>-elastic <p>-seaweedfs <p>-quickwit <p>-jaeger <p>-minithor --ignore-not-found
   ```

   minithor's default instance kept Elasticsearch in `elastic-system`, next to the old ECK
   operator; delete only its `elasticsearch` and `kibana` resources there while other instances
   remain.
4. Delete its ClusterRoleBinding: `thorium-operator-binding-<p>` (`thorium-operator-binding` for
   an instance without a prefix). If the conversion already ran, it moved that binding to
   `thorium-operator-binding-<p>-legacy`, bound to the `thorium-legacy-components` ClusterRole;
   delete that one instead, so `convert-to-helm.sh --cleanup` can remove the copied role once the
   kept deployment no longer needs it:

   ```bash
   kubectl delete clusterrolebinding thorium-operator-binding-<p> thorium-operator-binding-<p>-legacy --ignore-not-found
   ```

The pre-Helm scalers named group namespaces after Thorium groups, shared them between instances,
and didn't label them, so leave group namespaces alone: the deployment you keep uses them.

## Rolling back

**Before the chart is deployed**, run the old minithor or megathor again, which recreates the old
operator, its RBAC, and the ingress the script removed. Restore the `ThoriumCluster` if the old
tool doesn't create it, then delete the conversion's leftovers so a later conversion reads the
live `ThoriumCluster`:

```bash
kubectl -n thorium get secret thorium-legacy-cluster -o jsonpath='{.data.cluster\.json}' \
  | base64 -d | kubectl create -f -
kubectl -n thorium delete secret thorium-legacy-cluster
kubectl -n thorium delete configmap thorium-upgrade-state
```

The old tool recreates the `thorium-operator` ClusterRole and its binding, so also remove the copy
of the old scaler's role the conversion made, and the Helm ownership it gave the objects it handed
over (the `thorium` service account, the `banner` ConfigMap, and any namespace the chart renders),
so a later conversion or chart install starts from the pre-Helm state:

```bash
# the copied role and the bindings moved to it, which the conversion labeled
kubectl delete clusterrolebinding,clusterrole -l thorium.sandia.gov/legacy-components=true
# the Helm ownership annotations and label on every object handed to the release
for obj in serviceaccount/thorium configmap/banner; do
  kubectl -n thorium annotate "$obj" meta.helm.sh/release-name- meta.helm.sh/release-namespace-
  kubectl -n thorium label "$obj" app.kubernetes.io/managed-by-
done
for ns in $(kubectl get namespaces -o json | jq -r '.items[]
    | select(.metadata.annotations["meta.helm.sh/release-name"] == "thorium") | .metadata.name'); do
  kubectl annotate namespace "$ns" meta.helm.sh/release-name- meta.helm.sh/release-namespace-
  kubectl label namespace "$ns" app.kubernetes.io/managed-by-
done
```

Use `<p>-thorium` and `<p>-` names for a deployment with a namespace prefix, and your release name
in place of `thorium` in the `jq` filter if you passed `--release`.

**After the chart is deployed**, the Helm release owns the namespaces, the `thorium` service
account, the operator, and the `ThoriumCluster`, so the old tools can't take them back. Fix
problems by running `helm upgrade --install` again; before a target revision is set nothing in the
databases has changed and the old components keep running. Once upgrade steps have run and the
new components rolled out, the way back is restoring your backups.

## Removing a native-realm Elasticsearch user

The converted values keep using the Elasticsearch user the pre-Helm deployment created, which may
live in Elasticsearch's native realm. If you later give Thorium a different user (for example
when moving to an Elasticsearch whose Thorium user comes from ECK's file realm), delete the old
native-realm `thorium` user and role with the superuser once Thorium no longer uses them, so no
stale credentials or privileges are left (adjust the namespace to where that Elasticsearch runs,
such as `elastic-system`):

```bash
PASS=$(kubectl -n elastic get secret elastic-es-elastic-user -o jsonpath='{.data.elastic}' | base64 -d)
kubectl -n elastic port-forward svc/elastic-es-http 9200 &
curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/_security/user/thorium
curl -sk -u "elastic:$PASS" -X DELETE https://localhost:9200/_security/role/thorium
```

Indexes created from dynamic mappings are flagged by the `elastic-reindex-keyword-mappings` step
during the upgrade, or by a note in `status.message` on a `Ready` deployment; see
[Reindexing Elasticsearch](./upgrades.md#reindexing-elasticsearch).
