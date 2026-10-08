#!/usr/bin/env bash
# Vendor the upstream charts the thorium and infra-operators charts are built from, so both
# install straight from a directory or packaged .tgz with no dependency downloads. Re-run after
# changing a version below, then review the diff.
#
#   thorium/charts/quickwit           Quickwit, patched to install into <prefix>-quickwit and to
#                                     honor global.imageRegistry and global.clusterDomain
#   infra-operators/charts/scylla-operator
#                                     Scylla operator, patched to install into the release
#                                     namespace, to serve its webhook with a certificate the
#                                     chart generates or takes from webhook.tls (instead of one
#                                     issued by cert-manager), and to honor global.imageRegistry and
#                                     global.clusterDomain
#   infra-operators/charts/eck-operator
#                                     Elastic Cloud on Kubernetes operator, patched to honor
#                                     global.imageRegistry
#   infra-operators/crds, templates/kubegres.yaml
#                                     Kubegres, converted from its install manifest, with its
#                                     metrics Service selecting only the Kubegres controller
#
#   deploy/charts/scripts/vendor-charts.sh
set -euo pipefail

QUICKWIT_CHART_VERSION="0.8.17"
# the operator's built-in auxiliary images, which infra-operators/values.yaml repeats under
# scyllaOperatorConfig, change with it: copy them (without the digest) from assets/config/config.yaml
# in the operator's source at this version
SCYLLA_OPERATOR_CHART_VERSION="v1.21.1"
ECK_OPERATOR_CHART_VERSION="2.16.0"
KUBEGRES_VERSION="v1.19"

charts="$(cd "$(dirname "$0")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Pull an upstream chart into the work directory
#
#   pull <chart> <repo url> <version>
pull() {
    helm pull "$1" --repo "$2" --version "$3" --untar --untardir "$work"
}

# Replace a vendored chart directory with a freshly pulled one
#
#   install_chart <pulled dir> <destination dir>
install_chart() {
    rm -rf "$2"
    mkdir -p "$(dirname "$2")"
    mv "$1" "$2"
    echo "Vendored $(basename "$2") into $2"
}

pull quickwit https://helm.quickwit.io "$QUICKWIT_CHART_VERSION"
pull scylla-operator https://scylla-operator-charts.storage.googleapis.com/stable "$SCYLLA_OPERATOR_CHART_VERSION"
pull eck-operator https://helm.elastic.co "$ECK_OPERATOR_CHART_VERSION"
curl -fsSL -o "$work/kubegres.yaml" \
    "https://raw.githubusercontent.com/reactive-tech/kubegres/${KUBEGRES_VERSION}/kubegres.yaml"

python3 - "$work" "$KUBEGRES_VERSION" <<'EOF'
import pathlib, re, sys

work = pathlib.Path(sys.argv[1])
kubegres_version = sys.argv[2]
marker = "{{/* Patched by deploy/charts/scripts/vendor-charts.sh */}}\n"


def replace_count(text, old, new, count, what):
    found = text.count(old)
    assert found == count, f"{what}: expected {count} occurrence(s) of the pattern, found {found}"
    return text.replace(old, new)


def replace_once(text, old, new, what):
    return replace_count(text, old, new, 1, what)


# --- Quickwit: every namespaced resource into <prefix>-quickwit, images through the mirror helper
templates = work / "quickwit" / "templates"
for path in sorted(templates.iterdir()):
    # VolumeAttributesClass is cluster-scoped
    if path.suffix not in (".yaml", ".tpl") or path.name in ("_helpers.tpl", "volumeattributesclass.yaml"):
        continue
    root = "$root" if path.name == "_metastore-deployment.tpl" else "$"
    text = path.read_text()
    patched = re.sub(r"(?m)^metadata:\n",
                     'metadata:\n  namespace: {{ include "quickwit.namespace" %s }}\n' % root, text)
    if patched != text:
        path.write_text(patched)
helpers = templates / "_helpers.tpl"
text = helpers.read_text()
# in-cluster addresses built from the release namespace must use the Quickwit namespace
text = text.replace("{{ $.Release.Namespace }}", '{{ include "quickwit.namespace" $ }}')
text = text.replace("{{ .Release.Namespace }}", '{{ include "quickwit.namespace" . }}')
# the metastore address follows the thorium chart's cluster domain
text = replace_once(text, "{{ .Values.clusterDomain }}",
    "{{ ((.Values.global).clusterDomain) | default .Values.clusterDomain }}", "quickwit cluster domain")
old_image = re.search(r'\{\{- define "quickwit.image" -\}\}.*?\n\{\{- end \}\}\n', text, re.S)
assert old_image, "quickwit.image helper not found"
new_image = '''{{- define "quickwit.image" -}}
{{- $ref := "" -}}
{{- if .Values.image.digest -}}
{{- $ref = printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- $ref = printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion | toString) -}}
{{- end -}}
{{- include "thorium.image" (dict "root" . "image" $ref) -}}
{{- end -}}
'''
text = marker + text[:old_image.start()] + new_image + text[old_image.end():] + '''
{{/*
The namespace Quickwit runs in: <prefix>-quickwit, next to Thorium's other services.
*/}}
{{- define "quickwit.namespace" -}}
{{- include "thorium.ns" (dict "root" . "name" "quickwit") -}}
{{- end -}}
'''
helpers.write_text(text)

# --- Scylla operator: release namespace instead of a hard-coded one, chart-generated webhook cert
templates = work / "scylla-operator" / "templates"
for path in sorted(templates.glob("*.yaml")):
    text = path.read_text()
    patched = text.replace("namespace: scylla-operator", "namespace: {{ .Release.Namespace }}")
    if patched != text:
        path.write_text(patched)
(templates / "certificate.yaml").unlink()
(templates / "issuer.yaml").unlink()
webhook = templates / "validatingwebhook.yaml"
text = webhook.read_text()
text = replace_once(text,
    "  annotations:\n    cert-manager.io/inject-ca-from: scylla-operator/{{ include \"scylla-operator.certificateName\" . }}\n",
    "", "scylla webhook annotation")
text = replace_once(text, "      path: /validate\n",
    "      path: /validate\n    caBundle: {{ $caBundle }}\n", "scylla webhook clientConfig")
cert = '''{{- /*
The webhook serving certificate, so the webhook works without cert-manager. The secret and the
webhook configuration share this template so both use the same CA. The certificate comes from
webhook.tls when set (a stable certificate for renderers that can't read the cluster, such as
Argo CD or Flux, which would otherwise generate a new CA on every render), else from the existing
secret (read back with lookup, so upgrades keep it), else it is generated with SANs for the webhook
Service under global.clusterDomain. With createSelfSignedCertificate false the secret is managed
outside the chart and only the CA bundle is read: from webhook.tls.caCrt, else from that secret.
*/ -}}
{{- $secretName := include "scylla-operator.certificateSecretName" . -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace $secretName -}}
{{- $tls := .Values.webhook.tls | default dict -}}
{{- $caBundle := "" -}}
{{- if and (or $tls.crt $tls.key) (not (and $tls.caCrt $tls.crt $tls.key)) }}
{{- fail "scylla-operator.webhook.tls needs caCrt, crt, and key set together (PEM)" -}}
{{- end }}
{{- if .Values.webhook.createSelfSignedCertificate }}
{{- $crt := "" -}}
{{- $key := "" -}}
{{- if $tls.crt }}
{{- $caBundle = $tls.caCrt | b64enc -}}
{{- $crt = $tls.crt | b64enc -}}
{{- $key = $tls.key | b64enc -}}
{{- else if and $existing (index ($existing.data | default dict) "ca.crt") }}
{{- $caBundle = index $existing.data "ca.crt" -}}
{{- $crt = index $existing.data "tls.crt" -}}
{{- $key = index $existing.data "tls.key" -}}
{{- else }}
{{- $service := include "scylla-operator.webhookServiceName" . -}}
{{- $domain := (.Values.global).clusterDomain | default "cluster.local" -}}
{{- $ca := genCA "scylla-operator-webhook-ca" 3650 -}}
{{- $dns := list (printf "%s.%s.svc" $service .Release.Namespace) (printf "%s.%s.svc.%s" $service .Release.Namespace $domain) -}}
{{- $signed := genSignedCert (printf "%s.%s.svc" $service .Release.Namespace) nil $dns 3650 $ca -}}
{{- $caBundle = $ca.Cert | b64enc -}}
{{- $crt = $signed.Cert | b64enc -}}
{{- $key = $signed.Key | b64enc -}}
{{- end }}
apiVersion: v1
kind: Secret
metadata:
  name: {{ $secretName }}
  namespace: {{ .Release.Namespace }}
type: kubernetes.io/tls
data:
  ca.crt: {{ $caBundle }}
  tls.crt: {{ $crt }}
  tls.key: {{ $key }}
---
{{- else if $tls.caCrt }}
{{- $caBundle = $tls.caCrt | b64enc -}}
{{- else if $existing }}
{{- $caBundle = index ($existing.data | default dict) "ca.crt" | default "" -}}
{{- end }}
'''
webhook.write_text(marker + cert + text)
# the operator image (also handed to the operator for its Scylla pod sidecars) through the mirror
# helper of the infra-operators chart
scylla_image = "{{ .Values.image.repository }}/scylla-operator:{{ .Values.image.tag | default .Chart.AppVersion }}"
mirrored_scylla_image = ('{{ include "infra-operators.image" (dict "root" $ "image" (printf "%s/scylla-operator:%s" '
                         '.Values.image.repository (.Values.image.tag | default .Chart.AppVersion | toString))) }}')
for name, count in (("operator.deployment.yaml", 2), ("webhookserver.deployment.yaml", 1)):
    path = templates / name
    path.write_text(marker + replace_count(path.read_text(), scylla_image, mirrored_scylla_image, count,
                                           f"scylla operator image in {name}"))

# --- ECK operator: the operator image through the mirror helper of the infra-operators chart
statefulset = work / "eck-operator" / "templates" / "statefulset.yaml"
eck_image = ('"{{ .Values.image.repository }}{{- if .Values.config.ubiOnly -}}-ubi{{- end -}}'
             '{{- if .Values.image.fips -}}-fips{{- end -}}:{{ default .Chart.AppVersion .Values.image.tag }}"')
mirrored_eck_image = ('{{ include "infra-operators.image" (dict "root" $ "image" (printf "%s%s%s:%s" '
                      '.Values.image.repository (ternary "-ubi" "" (not (empty .Values.config.ubiOnly))) '
                      '(ternary "-fips" "" (not (empty .Values.image.fips))) '
                      '(default .Chart.AppVersion .Values.image.tag | toString))) | quote }}')
statefulset.write_text(marker + replace_once(statefulset.read_text(), eck_image, mirrored_eck_image, "eck operator image"))

# --- Kubegres: CRD into crds/, everything else into the release namespace
manifest = (work / "kubegres.yaml").read_text()
docs = [d for d in re.split(r"(?m)^---\n", manifest) if d.strip()]
crds, resources = [], []
for doc in docs:
    kind = re.search(r"(?m)^kind: (\S+)", doc).group(1)
    if kind == "Namespace":
        continue
    if kind == "CustomResourceDefinition":
        crds.append(doc)
        continue
    doc = doc.replace("namespace: kubegres-system", "namespace: {{ .Release.Namespace }}")
    # control-plane: controller-manager is a common label, so the metrics Service also selects
    # the Kubegres name, added to the controller's pods (the Deployment's selector is immutable)
    if kind == "Service":
        doc = replace_once(doc, "  selector:\n    control-plane: controller-manager\n",
                           "  selector:\n    app.kubernetes.io/name: kubegres\n    control-plane: controller-manager\n",
                           "kubegres metrics service selector")
    if kind == "Deployment":
        doc = replace_once(doc, "      labels:\n        control-plane: controller-manager\n",
                           "      labels:\n        app.kubernetes.io/name: kubegres\n        control-plane: controller-manager\n",
                           "kubegres pod labels")
    doc = re.sub(r"(?m)^(\s+image: )(\S+)$",
                 lambda m: m.group(1) + '{{ include "infra-operators.image" (dict "root" $ "image" .Values.kubegres.image) }}',
                 doc)
    resources.append(doc)
out = work / "kubegres"
(out / "crds").mkdir(parents=True)
(out / "crds" / "kubegres.yaml").write_text(
    f"# Kubegres {kubegres_version} CRD, vendored by deploy/charts/scripts/vendor-charts.sh\n---\n" + "---\n".join(crds))
(out / "kubegres.yaml").write_text(
    f"{{{{- /* Kubegres {kubegres_version}, vendored from its install manifest by deploy/charts/scripts/vendor-charts.sh */ -}}}}\n"
    "{{- if .Values.kubegres.enabled }}\n---\n" + "---\n".join(resources) + "{{- end }}\n")
EOF

install_chart "$work/quickwit" "$charts/thorium/charts/quickwit"
install_chart "$work/scylla-operator" "$charts/infra-operators/charts/scylla-operator"
install_chart "$work/eck-operator" "$charts/infra-operators/charts/eck-operator"
mkdir -p "$charts/infra-operators/crds" "$charts/infra-operators/templates"
mv "$work/kubegres/crds/kubegres.yaml" "$charts/infra-operators/crds/kubegres.yaml"
mv "$work/kubegres/kubegres.yaml" "$charts/infra-operators/templates/kubegres.yaml"
echo "Vendored kubegres ${KUBEGRES_VERSION} into $charts/infra-operators"
