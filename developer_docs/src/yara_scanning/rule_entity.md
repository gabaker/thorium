# YaraRule Entity

---

## Today

No `YaraRule` entity kind exists — the rules submodule exports only sigma
(`api/src/models/entities/rules.rs:3-5`). The template is
`SigmaRule { rule, applies_to, score, actions }`
(`api/src/models/entities/rules/sigma.rs:95-105`): raw source carried verbatim in
`kind_data`, an applies-to list, a score, and optional auto-flag actions.

The UI is further along than the backend: a complete YARA editor stack is already built
and waiting for a real entity to bind to — `ui/src/utilities/rules/yara/` (parse,
validate, suggestions, schema), the `CodeEditor` yara language
(`ui/src/components/shared/inputs/code/CodeEditor/yara-language.ts`), the `YaraRenderer`
renderer, and an end-to-end spec (`ui/e2e/yara-editor.spec.ts`).

## The YaraRule Metadata Kind

**Decision: add `EntityMetadata::YaraRule(YaraRule)` in a new
`api/src/models/entities/rules/yara.rs`, exported beside sigma.**

```rust
/// A YARA rule that can be applied to samples
pub struct YaraRule {
    /// The raw YARA source document (may define several rules)
    pub rule: String,
    /// The kinds of data this rule scans (v1: `Samples` only)
    pub applies_to: Vec<YaraRuleAppliesTo>,
    /// The score to apply when this rule hits
    pub score: i64,
    /// The actions to take when this rule hits
    pub actions: Vec<YaraActionToTake>,
}
```

`EntityKinds::YaraRule` falls out of the `strum` discriminant derive on `EntityMetadata`
(`api/src/models/entities.rs:632-663`) and requires **no Scylla schema change**: `kind`
is a TEXT column and `kind_data` is JSON stored as TEXT
(`api/src/models/schema/scylla/entities.rs`).

**One entity = one YARA source document.** Splitting multi-rule files would break
`private` rules and `rule_a and rule_b` dependencies, which only resolve within one
source. Attribution stays exact regardless: each entity compiles into a namespace named
by its UUID, and every hit records the matched rule identifier within it
(see [Hits and Attribution](./hits.md)).

`applies_to` is `{Samples}` in v1 — yara scans raw bytes, and repo bundles have
different download and caching paths. The enum exists from day one so `Repos` is a
purely additive variant later. `YaraActionToTake::Flag(YaraAutoFlag)` mirrors sigma's
auto-flag action.

## Validation at Create AND Update

Rule source is compiled with yara-x (see [Engine Choice](./engine.md)) on **both** the
create and update paths. Sigma validates only at create: its update arm assigns the new
source with `update!(rule.rule, ...)` and never re-compiles
(`api/src/models/backends/entities.rs:1017-1019`; the assignment at `:1019` is only
incidentally guarded by the intrinsic-tag YAML re-parse at `:695`). The yara update arm
re-validates explicitly. Validation limits:

- source capped by `conf.thorium.yara.max_rule_size` (default 128 KiB); empty source 400s;
- `include` statements rejected — the API has no rule filesystem;
- module imports restricted to an allowlist (`pe`, `elf`, `macho`, `dotnet`, `math`,
  `hash`, `string`, `time`, `lnk`); undefined externals rejected;
- compiler warnings surfaced in the 400 body alongside the errors.

Rule identifiers may collide across entities (namespaces isolate them), and `global`
rules affect only their own namespace — yara-x semantics the user docs must state.

Enums parse with `strum::EnumString`, never a hand-written `FromStr`:
`SigmaRuleAppliesTo`'s manual impl parses only 2 of its 4 variants
(`api/src/models/entities/rules/sigma.rs:37-52`), so forms cannot create sigma rules
targeting functions — a trap `YaraRuleAppliesTo` must not inherit when `Repos` lands.

A rule update also clears any `YaraRuleStatus` quarantine tags (Broken/Noisy/Slow): the
scanner path writes those tags (see [Scaling and Performance](./performance.md)), and
editing the rule is the exit from quarantine — the mechanism lands with the M2 scanner.

## Intrinsic Tags

Added in `populate_intrinsic_tags`, following the sigma arm
(`api/src/models/backends/entities.rs:693-704`):

| Tag | Source |
| --- | --- |
| `YaraRuleName` | one per parsed rule identifier in the source |
| `YaraRuleAuthor` / `YaraRuleDescription` | the rules' `meta` sections |
| `YaraRuleTags` | one per YARA tag on any rule |
| `YaraRuleHash` | the content hash (below) |

`YaraRuleStatus=Broken|Noisy|Slow` tags come from the scanner path, never intrinsically
(see [Ruleset Compilation and Caching](./ruleset.md), [Scaling and Performance](./performance.md)).

## Versioning Is a Content Hash

`rule_hash` = sha256 of the rule source, computed at create and update and recorded on
every hit, so a hit stays attributable to the exact text that fired even after edits —
entity updates overwrite `kind_data` in place per group row
(`api/src/models/backends/db/entities.rs:392-503`). A version-history table is out of
scope.

## API, Client, and UI Surface

Multipart fields mirror sigma's exactly: `metadata[yara_rule]`,
`metadata[yara_applies_to][]`, `metadata[yara_actions][]`, and `metadata[score]` beside
the sigma fields in `EntityMetadataForm` (`api/src/models/entities.rs:108-115`), with
`add_`/`remove_` update fields beside sigma's in `EntityMetadataUpdateForm`
(`api/src/models/entities.rs:255-266`). The kind dispatches to `YaraRule::from_form`
next to sigma's arm (`api/src/models/backends/entities.rs:1329`); the client builds the
same fields the way `SigmaRule::add_to_form` does
(`api/src/models/entities/rules/sigma.rs:213-228`).

Dedupe on re-import is `IdentifyingEntityInfo::YaraRule { rule }` keyed by source text —
a new arm beside `SigmaRule { rule }` (`api/src/models/entities.rs:895`, `:929`) feeding
`Entity::hash_identifying` (`api/src/models/entities.rs:341-351`).

The UI copies the three sigma configs —
`ui/src/components/entities/create/configs/SigmaRuleCreateConfig.tsx`,
`ui/src/components/entities/browsing/configs/SigmaRuleBrowsingConfig.tsx`, and
`ui/src/components/entities/details/configs/SigmaRuleDetailsConfig.tsx` — plus the model
`ui/src/models/entities/rules/sigma.ts`, and wires in the yara editor stack above.
Everything on this page is milestone M0.
