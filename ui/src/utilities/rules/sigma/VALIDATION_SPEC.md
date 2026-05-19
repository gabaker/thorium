# Sigma Rule Editor Validation Spec

This document defines the validation behavior for Sigma rule fields in the code editor. Sigma rules follow the [SigmaHQ specification](https://github.com/SigmaHQ/sigma-specification).

## Syntax Validation

- YAML syntax errors are caught by the YAML parser and shown as errors
- JSON syntax errors are caught by `JSON.parse` pre-validation when in JSON mode
- Duplicate YAML keys are detected and flagged as errors
- Non-mapping documents (lists, scalars) are rejected

## Required Fields

| Field       | Severity | Message                               |
| ----------- | -------- | ------------------------------------- |
| `title`     | Error    | `Missing required field: 'title'`     |
| `logsource` | Error    | `Missing required field: 'logsource'` |
| `detection` | Error    | `Missing required field: 'detection'` |

## Known Fields

Unknown top-level fields produce an **info** diagnostic (not error/warning) since the Sigma spec is extensible. Known fields: `title`, `id`, `name`, `related`, `taxonomy`, `status`, `description`, `license`, `author`, `references`, `date`, `modified`, `logsource`, `detection`, `fields`, `falsepositives`, `level`, `tags`, `scope`.

## Field Validation

| Field            | Type     | Validation                                                                    |
| ---------------- | -------- | ----------------------------------------------------------------------------- |
| `title`          | string   | Warn if length > 256                                                          |
| `name`           | string   | Warn if length > 256                                                          |
| `taxonomy`       | string   | Warn if length > 256                                                          |
| `description`    | string   | Warn if length > 65535                                                        |
| `id`             | string   | Warn if not UUIDv4 format                                                     |
| `status`         | enum     | Error if not one of: stable, test, experimental, deprecated, unsupported      |
| `level`          | enum     | Error if not one of: informational, low, medium, high, critical               |
| `date`           | string   | Error if not YYYY-MM-DD format. Also accepts Date objects (YAML auto-parses). |
| `modified`       | string   | Error if not YYYY-MM-DD format                                                |
| `tags`           | string[] | Warn if entries don't match lowercase dot-separated pattern                   |
| `references`     | list     | Error if not an array                                                         |
| `fields`         | list     | Error if not an array                                                         |
| `falsepositives` | string[] | Warn if entries < 2 characters                                                |
| `scope`          | string[] | Warn if entries < 2 characters                                                |

## Logsource Validation

- Warn if `logsource` object has none of: `category`, `product`, `service`

## Detection Validation

| Check                                            | Severity |
| ------------------------------------------------ | -------- |
| `detection.condition` missing                    | Error    |
| Condition references undefined search identifier | Warning  |
| Unknown value modifier (e.g. `field\|badmod`)    | Error    |

## Related Entries Validation

Each entry in `related` array must have:

- `id` field (Error if missing)
- `type` field (Error if missing)
- `type` must be one of: derived, obsolete, merged, renamed, similar (Error if invalid)

## Null Handling

Sigma rules are standalone YAML documents, not API request bodies. There is no Rust struct with `Option<T>` semantics. All fields are validated as-is by the Sigma specification — null values on any required field produce a "missing" error since `null` is treated as absent.
