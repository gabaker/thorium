#!/usr/bin/env bash
# Clip: exempt a quarantined watchlist rule from quarantine, then post, list, and retire
# an advisory notification on it. The rule is targeted by name throughout; only the
# notification delete still types an id, since notifications have no names. Embedded in
# admins/rule_scanning_ops.md as rules-admin.cast (cross-linked from
# admins/notifications_admins.md). Requires admin keys.
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./lib.sh

# stage a "quarantined" watchlist rule off camera exactly like the scanner leaves one
# (staging resolves the id via the list route rather than racing the fresh rule's
# by-name view)
thorctl rules create --engine yara --name demo_broad_watchlist --groups demolab \
  --rule-file rules/demo_broad_watchlist.yar --tags YaraRuleStatus=Noisy >/dev/null 2>&1
WATCH_ID="$(entity_id rules demo_broad_watchlist)"
thorctl rules notifications create "$WATCH_ID" \
  --msg "Rule quarantined as noisy: it matched 912 of the last 1000 samples" \
  --level error >/dev/null 2>&1

# wait off camera for the staged rule to resolve by name before the on-camera exempt
await_name thorctl rules get demo_broad_watchlist

# exempting the rule clears its Noisy status in the same action and keeps it scanning
type_cmd "thorctl rules exempt demo_broad_watchlist"
type_cmd "thorctl rules get demo_broad_watchlist"
# retire the scanner's stale quarantine notification off camera so the --ids
# listing below stays inside the recording width
STALE_ID="$(thorctl rules notifications get "$WATCH_ID" --ids 2>/dev/null \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -F 'quarantined as noisy' \
  | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' \
  | head -1)"
thorctl rules notifications delete "$WATCH_ID" "$STALE_ID" >/dev/null 2>&1
# post an advisory for the rule's users, list it with ids, then retire it; the
# create command types across two lines so it fits the recording width
type_cmd "thorctl rules notifications create demo_broad_watchlist \\
  --msg 'Broad matching is expected for this rule' --level warn"
type_cmd "thorctl rules notifications get demo_broad_watchlist --ids"
# resolve the advisory's id off camera (strip any ANSI styling before matching);
# notifications have no names, so the delete below genuinely needs this id
NOTE_ID="$(thorctl rules notifications get "$WATCH_ID" --ids 2>/dev/null \
  | sed 's/\x1b\[[0-9;]*m//g' \
  | grep -F 'Broad matching is expected' \
  | grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' \
  | head -1)"
type_cmd "thorctl rules notifications delete demo_broad_watchlist $NOTE_ID"
