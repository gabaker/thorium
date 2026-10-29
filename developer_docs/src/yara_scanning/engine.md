# Engine Choice

---

**Decision: the scan path runs on `yara-x`, VirusTotal's official pure-Rust rewrite,
at a pinned version.** No yara crate of any kind existed in the workspace before M0
(`Cargo.lock` had no yara entry), so this was a green-field choice; the in-process rule
engine precedent is `sigma-rust` (`Cargo.toml:62`).

## Why yara-x

- **In-process `Compiler` with per-source namespaces** — each rule entity compiles under
  its own UUID, so hit attribution holds by construction
  (see [Ruleset Compilation and Caching](./ruleset.md)).
- **Serializable compiled rules** — `Rules::serialize`/`deserialize` give replicas and
  restarts a compile-skipping cache.
- **`Scanner::set_timeout`** bounds the cost of any single scan.
- **`Scanner::scan(&[u8])` scans in-memory buffers** — the scan path's default: bytes go
  from the download stream to a RAM buffer to the scanner without ever touching disk
  (see [Scan Execution](./execution.md)). `scan_file` (memory-mapped) also exists and
  backs the optional spill mode for memory-constrained deployments.
- **Per-match detail** — matched rule identifier, metadata, and pattern offsets feed hit
  metadata directly (see [Hits and Attribution](./hits.md)).
- **The `rules-profiling` cargo feature** attributes scan time to individual rules — the
  mechanism behind `YaraRuleStatus=Slow` quarantine
  (see [Scaling and Performance](./performance.md)).
- **Memory safety** in a long-lived, multi-tenant daemon that compiles
  attacker-adjacent input.

## What yara-x Cannot Do

- **A scan cannot be restricted to one namespace** — the whole compiled `Rules` always
  runs. This shapes the yara hunt engine: a hunt compiles its frozen rule subset into its
  own ruleset — content-addressed by the same `(engine_version, ruleset_hash)` scheme —
  instead of re-running the global set over the corpus (see
  [Hunts and Rule Sets](./hunts.md)).
- **The serialized format is not stable across crate versions** — cache keys are always
  `(engine_version, ruleset_hash)`, never `ruleset_hash` alone
  (see [Ruleset Compilation and Caching](./ruleset.md)).
- **No incremental or streaming feed API** — a scan takes a whole object (`scan(&[u8])`
  or `scan_file`); chunks cannot be fed as they arrive. This is YARA semantics, not a
  yara-x gap, and it bounds what "streaming" can mean in the pipeline
  (see [Scan Execution](./execution.md)).

## Alternatives

- **The libyara FFI crate**: maximum YARA 4.x compatibility — it *is* YARA — but it
  drags a C toolchain into every CI and cross target (the workspace cross-builds via
  `Cross.toml` today), and FFI faults take the whole daemon down. Rule-triggered
  crashes in a process compiling many tenants' input is an unacceptable blast radius.
- **Shelling out to the yara CLI** — the legacy tool pattern: a subprocess per scan
  against an image-baked `rules.bin`, attribution regex-parsed from stdout
  (`tools/images/github.com/virustotal/yara/run_yara.py:31`, parser at `:16-27`). It
  stays available as an ordinary pipeline tool; rejected for the scan path.

## Compatibility Risk for Legacy Rules

yara-x targets 4.x compatibility but is stricter: escape sequences in strings and
regexes, some module and field gaps, edge syntax. Mitigations:

- create/update validation rejects incompatible rules with the compiler's real error at
  submission time, not scan time (see [YaraRule Entity](./rule_entity.md));
- the `thorctl rules import` bulk importer surfaces each rule's server-side compile
  errors and warnings as a per-file compat report during import;
- the legacy tool image keeps running during migration.

## Version Pinning and Upgrades

The yara-x version is pinned, and `engine_version` is recorded on every hit, so match
behavior stays auditable. An engine upgrade is treated like a rule edit for compile
validation: rules that stop compiling quarantine at the next rebuild with a notification.
Match-behavior changes over history are repaired by running hunts over the affected
corpus — a documented manual operation, deliberately not automatic (see
[Hunts and Rule Sets](./hunts.md)).

As implemented (M0): `yara-x =1.21.0` is exact-pinned in the workspace and — unlike
`sigma-rust`, which every consumer compiles — gated behind the api crate's `api` feature,
so client-only consumers (thorctl, the agent, thorpy) never build a YARA engine; the
event-handler depends on it directly. The dependency currently spells out the default
modules minus `zip-module` (its `tinyzip` dep was unfetchable through the dev proxy; a
revert note sits beside the dependency) — zip is not in the default module allowlist, so
behavior is unchanged.
