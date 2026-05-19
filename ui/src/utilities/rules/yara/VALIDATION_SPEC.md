# YARA Rule Editor Validation Spec

This document defines the validation behavior for YARA rules in the code editor. YARA rules follow the [YARA documentation](https://yara.readthedocs.io/).

## Syntax Validation

YARA uses its own syntax (not YAML/JSON). The `parseYaraText` parser handles:

- Rule structure parsing (rule name, sections, braces)
- String definition parsing (text, regex, hex)
- Condition parsing
- Parse errors are reported as error diagnostics with line/column positions

## Rule-Level Validation

| Check                                                   | Severity | Message                                               |
| ------------------------------------------------------- | -------- | ----------------------------------------------------- |
| Invalid rule name (must match `[a-zA-Z_][a-zA-Z0-9_]*`) | Error    | `Invalid rule name: '...'`                            |
| Duplicate rule name                                     | Warning  | `Duplicate rule name: '...'`                          |
| Missing `condition:` section                            | Error    | `Rule '...' is missing required 'condition:' section` |
| Empty condition                                         | Error    | `Rule '...' has an empty condition`                   |
| Unknown section (not meta/strings/condition)            | Error    | `Unknown section '...:'`                              |
| Duplicate section                                       | Error    | `Duplicate section '...:'`                            |

## Import Validation

| Check               | Severity |
| ------------------- | -------- |
| Unknown module name | Warning  |

Known modules: `pe`, `elf`, `cuckoo`, `magic`, `hash`, `math`, `dotnet`, `time`, `string`, `console`, `dex`, `macho`.

## Meta Section Validation

| Check              | Severity |
| ------------------ | -------- |
| Duplicate meta key | Error    |

## Strings Section Validation

| Check                                                     | Severity |
| --------------------------------------------------------- | -------- |
| Empty strings section (has `strings:` but no definitions) | Warning  |
| Duplicate string identifier                               | Error    |
| Invalid modifier for string type                          | Error    |
| Duplicate modifier on same string                         | Error    |

### Modifier Validity by String Type

| String Type     | Allowed Modifiers                                               |
| --------------- | --------------------------------------------------------------- |
| Text (`"..."`)  | ascii, wide, xor, base64, base64wide, fullword, nocase, private |
| Regex (`/.../`) | ascii, wide, nocase, fullword, private                          |
| Hex (`{...}`)   | private                                                         |

## Condition Validation

| Check                                                 | Severity |
| ----------------------------------------------------- | -------- |
| References undefined string (`$s1` not in strings)    | Error    |
| References strings but rule has no `strings:` section | Error    |
| Defined string never referenced in condition          | Warning  |
| Uses deprecated `entrypoint` keyword                  | Warning  |

## Tag Validation

| Check                      | Severity |
| -------------------------- | -------- |
| Duplicate tag on same rule | Error    |

## Null Handling

Not applicable — YARA is not a YAML/JSON format. There is no concept of null values in YARA syntax.
