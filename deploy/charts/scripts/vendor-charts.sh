#!/usr/bin/env bash
# Vendor the upstream charts the thorium and infra-operators charts are built from, so both
# install straight from a directory or packaged .tgz with no dependency downloads. Re-run after
# changing a version below, then review the diff.
#
#   thorium/charts/quickwit           Quickwit, patched to install into <prefix>-quickwit and to
#                                     honor global.imageRegistry
#   infra-operators/charts/scylla-operator
#                                     Scylla operator, patched to install into the release
#                                     namespace and to serve its webhook with a certificate the
#                                     chart generates (instead of one issued by cert-manager)
#   infra-operators/charts/eck-operator
#                                     Elastic Cloud on Kubernetes operator, unmodified
#   infra-operators/crds, templates/kubegres.yaml
#                                     Kubegres, converted from its install manifest
#
#   deploy/charts/scripts/vendor-charts.sh
set -euo pipefail

QUICKWIT_CHART_VERSION="0.8.17"
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


def replace_once(text, old, new, what):
    assert old in text, f"{what}: pattern not found"
    return text.replace(old, new)


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
The webhook serving certificate, generated once by the chart and kept on upgrades (read back from
the secret), so the webhook works without cert-manager. The secret and the webhook configuration
share this template so both use the same CA.
*/ -}}
{{- $secretName := include "scylla-operator.certificateSecretName" . -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace $secretName -}}
{{- $caBundle := "" -}}
{{- if .Values.webhook.createSelfSignedCertificate }}
{{- $crt := "" -}}
{{- $key := "" -}}
{{- if and $existing (index ($existing.data | default dict) "ca.crt") }}
{{- $caBundle = index $existing.data "ca.crt" -}}
{{- $crt = index $existing.data "tls.crt" -}}
{{- $key = index $existing.data "tls.key" -}}
{{- else }}
{{- $service := include "scylla-operator.webhookServiceName" . -}}
{{- $ca := genCA "scylla-operator-webhook-ca" 3650 -}}
{{- $dns := list (printf "%s.%s.svc" $service .Release.Namespace) (printf "%s.%s.svc.cluster.local" $service .Release.Namespace) -}}
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
{{- else if $existing }}
{{- $caBundle = index ($existing.data | default dict) "ca.crt" | default "" -}}
{{- end }}
'''
webhook.write_text(marker + cert + text)

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
