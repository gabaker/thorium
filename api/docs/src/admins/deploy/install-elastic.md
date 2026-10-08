# Elasticsearch (ECK)

This page sets up Elasticsearch and Kibana with Elastic Cloud on Kubernetes (ECK) for Thorium,
the way the `thorium` chart does it. Use it when you run Elasticsearch yourself
(`global.managed.elastic: false` with Helm, or the operator without Helm). Thorium stores only
derived search data in Elasticsearch: the search streamer rebuilds every index from Scylla, so
Elasticsearch needs no backup of its own. See
[Bring Your Own Infrastructure](./infrastructure.md) for what Thorium needs from it.

## 1) Install the ECK operator

Install ECK with the `infra-operators` chart (which runs ECK 2.16.0 in the `infra-operators`
namespace), turning off the operators you don't need:

```bash
helm install infra-operators oci://ghcr.io/cisagov/thorium/charts/infra-operators \
  --version $VERSION -n infra-operators --create-namespace --wait \
  --set scylla-operator.enabled=false --set kubegres.enabled=false
```

Or install ECK from Elastic's own Helm chart or manifests, following Elastic's documentation.

## 2) Set the kernel parameter

Elasticsearch memory-maps its indexes (`node.store.allow_mmap: true`), which needs
`vm.max_map_count` of at least 262144 on every node Elasticsearch runs on:

```bash
sudo sysctl -w vm.max_map_count=262144
echo "vm.max_map_count=262144" | sudo tee /etc/sysctl.d/99-elasticsearch.conf
```

If you can't change the nodes, either add a privileged init container that runs
`sysctl -w vm.max_map_count=262144` to the node set (what the chart's
`infra.elastic.setMaxMapCount` does), or set `node.store.allow_mmap: false` in the node set's
config (slower searches, no kernel setting needed).

## 3) Create the Thorium user and role

The chart gives Thorium its own user and role through ECK's file realm, so nothing needs the
`elastic` superuser at runtime. Create the two Secrets the Elasticsearch resource below refers
to, in the namespace Elasticsearch will run in (`elastic` here):

```bash
kubectl create namespace elastic
ELASTIC_PASSWORD=$(openssl rand -hex 24)
kubectl -n elastic create secret generic thorium-elastic-user \
  --type=kubernetes.io/basic-auth \
  --from-literal=username=thorium \
  --from-literal=password="$ELASTIC_PASSWORD" \
  --from-literal=roles=thorium
kubectl -n elastic create secret generic thorium-elastic-roles --from-file=roles.yml=/dev/stdin <<'EOF'
thorium:
  indices:
    - names: ["thorium*"]
      privileges: ["all"]
EOF
```

`all` on `thorium*` covers the default index names. If you rename an index in `elastic.results`
or `elastic.tags` to something outside `thorium*`, add it to `names`. Keep `$ELASTIC_PASSWORD`:
it is Thorium's `elastic.password`.

## 4) Deploy Elasticsearch and Kibana

```yaml
apiVersion: elasticsearch.k8s.elastic.co/v1
kind: Elasticsearch
metadata:
  name: elastic
  namespace: elastic
spec:
  version: 8.19.2
  volumeClaimDeletePolicy: DeleteOnScaledownOnly
  auth:
    fileRealm:
      - secretName: thorium-elastic-user
    roles:
      - secretName: thorium-elastic-roles
  nodeSets:
    - name: default
      count: 3
      podTemplate:
        spec:
          containers:
            - name: elasticsearch
              resources:
                limits:
                  cpu: "2"
                  memory: 4Gi
      volumeClaimTemplates:
        - metadata:
            name: elasticsearch-data
          spec:
            accessModes:
              - ReadWriteOnce
            # storageClassName: <your storage class>
            resources:
              requests:
                storage: 32Gi
      config:
        node.store.allow_mmap: true
        http.max_content_length: 1024mb
---
apiVersion: kibana.k8s.elastic.co/v1
kind: Kibana
metadata:
  name: elastic
  namespace: elastic
spec:
  version: 8.19.2
  count: 1
  elasticsearchRef:
    name: elastic
```

Save it as `elastic.yaml`, apply it, and wait for the cluster to turn green:

```bash
kubectl apply -f elastic.yaml
kubectl -n elastic get elasticsearch,kibana
# NAME                                                 HEALTH   NODES   VERSION   PHASE
# elasticsearch.elasticsearch.k8s.elastic.co/elastic   green    3       8.19.2    Ready
```

ECK creates these Services and Secrets:

| Name | What it is |
|------|------------|
| Service `elastic-es-http` (port 9200) | The Elasticsearch HTTPS endpoint, `https://elastic-es-http.elastic.svc.cluster.local:9200` |
| Service `elastic-kb-http` (port 5601) | Kibana |
| Secret `elastic-es-elastic-user` (key `elastic`) | The `elastic` superuser's password |
| Secret `elastic-es-http-certs-public` (key `ca.crt`) | The CA of ECK's self-signed HTTP certificate |

## Creating the user without the file realm

For an Elasticsearch you don't manage with ECK (or to keep the user in the native realm), create
the role and user with the security API as a superuser. The role needs `view_index_metadata`,
`write`, and `read` on Thorium's indexes, plus `create_index` while they don't exist yet:

```bash
curl -sk -u "elastic:$SUPERUSER_PASSWORD" -X PUT "$ELASTIC_URL/_security/role/thorium" \
  -H 'Content-Type: application/json' \
  -d '{"indices": [{"names": ["thorium*"], "privileges": ["all"]}]}'
curl -sk -u "elastic:$SUPERUSER_PASSWORD" -X PUT "$ELASTIC_URL/_security/user/thorium" \
  -H 'Content-Type: application/json' \
  -d "{\"password\": \"$ELASTIC_PASSWORD\", \"roles\": [\"thorium\"]}"
```

Don't create the indexes: the search streamer creates each missing index with explicit
mappings and fills it from Scylla. An index created any other way (for example by writing to it
before the search streamer starts) gets dynamic mappings, which Thorium reports as an index that
doesn't map `group` as a keyword.

Instead of creating the user yourself, you can let the operator create the role and user from a
superuser login: put the superuser's credentials in a Secret in the thorium namespace and set
`operator.cluster.bootstrap.elastic.adminSecret.name` (Helm) or `spec.bootstrap.elastic` (see
[ThoriumCluster and Operator Reference](./thoriumcluster.md)).

## Certificates

ECK serves Elasticsearch over HTTPS with a self-signed certificate. Thorium either skips
verification or verifies against the CA:

- **Skip verification** (what the chart does for its own Elasticsearch, whose traffic stays in
  the cluster): set `insecure_certificates: true` in the config's `elastic` section
  (`operator.backends.elastic.insecureCertificates: true` with Helm).
- **Verify against the CA:** copy the CA into a Secret in the thorium namespace and reference
  it. With Helm set `operator.backends.elastic.caSecret.name`; without Helm set
  `spec.elastic_ca_secret` and mount the Secret in the operator yourself (see
  [Elasticsearch certificates](./helm-configuration.md#elasticsearch-certificates)).

  ```bash
  kubectl -n elastic get secret elastic-es-http-certs-public -o jsonpath='{.data.ca\.crt}' \
    | base64 -d > elastic-ca.pem
  kubectl -n thorium create secret generic elastic-ca --from-file=ca.crt=elastic-ca.pem
  ```

  Verification checks the hostname too, so `elastic.node` must be a name in the certificate,
  such as `elastic-es-http.elastic.svc` or `elastic-es-http.elastic.svc.cluster.local`.

## Thorium settings

| Setting | Value |
|---------|-------|
| `elastic.node` | `https://elastic-es-http.elastic.svc.cluster.local:9200` (Helm: `operator.backends.elastic.node`) |
| `elastic.username` / `elastic.password` | `thorium` and `$ELASTIC_PASSWORD` (Helm: `secrets.credentials.elasticUsername` / `elasticPassword`) |
| `elastic.results` / `elastic.tags` | The index names; defaults `thorium_sample_results`, `thorium_repo_results`, `thorium_sample_tags`, `thorium_repo_tags` (Helm: `global.elasticIndices`) |

## Accessing Elasticsearch and Kibana

```bash
PASS=$(kubectl -n elastic get secret elastic-es-elastic-user -o jsonpath='{.data.elastic}' | base64 -d)
kubectl -n elastic port-forward svc/elastic-es-http 9200 &
curl -sk -u "elastic:$PASS" https://localhost:9200/_cat/indices
kubectl -n elastic port-forward svc/elastic-kb-http 5601   # Kibana at https://localhost:5601
```
