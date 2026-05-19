# Pipeline Editor Validation Spec

This document defines the validation behavior for every field in the `PipelineRequest` API structure.

**Key:**

- **Nullable** = `Option<T>` in Rust. The API deserializes `null` to `None`. The editor allows null silently.
- **Defaulted** = `#[serde(default)]` in Rust. If the field is missing, a default is used.
- **Required** = No `Option`, no `default`. The field must be present with a valid value.

## Top-Level Fields (`PipelineRequest`)

| Field         | Rust Type                       | Null? | Default         | Validator Behavior                                                                                             |
| ------------- | ------------------------------- | ----- | --------------- | -------------------------------------------------------------------------------------------------------------- |
| `group`       | `String`                        | No    | None (required) | Error: `'group' must be a string`                                                                              |
| `name`        | `String`                        | No    | None (required) | Error: `'name' must be a string`                                                                               |
| `order`       | `Value` (JSON)                  | No    | None (required) | Error if not array. Each entry must be string or string[]. Image names validated against group when available. |
| `sla`         | `Option<u64>`                   | Yes   | -               | Allow null. Error if present and not a number.                                                                 |
| `triggers`    | `HashMap<String, EventTrigger>` | No    | `{}`            | Error on null for non-Option. Each trigger must be `"NewSample"` or a `{Tag: {...}}` object.                   |
| `description` | `Option<String>`                | Yes   | -               | Allow null.                                                                                                    |

## Order Image Validation

When valid image names are available for the pipeline's group (fetched asynchronously), each image name in the `order` array is checked against the set. Unknown images produce errors: `"Image '<name>' not found in group '<group>'"`. Validation is skipped when image names haven't been loaded yet or the group doesn't match.

## Trigger Validation

Each trigger entry must be either:

- The string `"NewSample"`
- An object with a `Tag` key containing `tag_types` (string array), `required` (object), and `not` (object)

Invalid trigger values produce errors. Missing sub-fields in Tag triggers produce suggestions.
