#!/usr/bin/env bash
# Clip: read a quarantined rule's status and notifications, then re-admit it to
# scanning — the on-camera commands target the rule by name. Embedded in
# users/yara_rules.md as rules-notifications.cast. Requires admin keys so the
# off-camera staging can write the scanner-style notification.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# stage a "quarantined" rule off camera exactly like the scanner leaves one: a Noisy
# status tag plus an error notification carrying the reason (staging resolves the id
# via the list route rather than racing the fresh rule's by-name view)
thorctl rules create --engine yara --name demo_hex_blob_scan --groups demolab \
  --rule-file rules/demo_hex_blob_scan.yar --tags YaraRuleStatus=Noisy >/dev/null 2>&1
NOISY_ID="$(entity_id rules demo_hex_blob_scan)"
thorctl rules notifications create "$NOISY_ID" \
  --msg "Rule quarantined as noisy: it matched 847 of the last 1000 samples" \
  --level error >/dev/null 2>&1

# wait off camera for the staged rule to resolve by name before the on-camera get
await_name thorctl rules get demo_hex_blob_scan

# the quarantine shows on the rule itself and its notification says why
type_cmd "thorctl rules get demo_hex_blob_scan"
type_cmd "thorctl rules notifications get demo_hex_blob_scan"
# deleting the status tag puts the rule back into scanning rotation
type_cmd "thorctl rules unquarantine demo_hex_blob_scan"
