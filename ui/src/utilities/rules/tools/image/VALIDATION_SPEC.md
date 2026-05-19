# Image Editor Validation Spec

This document defines the validation behavior for every field in the `ImageRequest` API structure. It maps each field to its Rust type, whether the API accepts `null`, and what the editor validator should do.

**Key:**

- **Nullable** = `Option<T>` in Rust. The API deserializes `null` to `None`. The editor allows null silently.
- **Defaulted** = `#[serde(default)]` in Rust. If the field is missing, a default is used. But an explicit `null` on a non-Option type will fail deserialization.
- **Required** = No `Option`, no `default`. The field must be present with a valid value.

## Top-Level Fields (`ImageRequest`)

| Field               | Rust Type                         | Null? | Default         | Validator Behavior                                                    |
| ------------------- | --------------------------------- | ----- | --------------- | --------------------------------------------------------------------- |
| `group`             | `String`                          | No    | None (required) | Error: `'group' must be a string`                                     |
| `name`              | `String`                          | No    | None (required) | Error: `'name' must be a string`                                      |
| `version`           | `Option<ImageVersion>`            | Yes   | -               | Allow null. Validate SemVer/Custom structure if present.              |
| `scaler`            | `ImageScaler`                     | No    | `K8s`           | Error on null: `'scaler' must be one of: ...`                         |
| `image`             | `Option<String>`                  | Yes   | -               | Allow null (clears the image URL).                                    |
| `lifetime`          | `Option<ImageLifetime>`           | Yes   | -               | Allow null (object validator already handles). Populate suggestion.   |
| `timeout`           | `Option<u64>`                     | Yes   | -               | Allow null (clears timeout).                                          |
| `resources`         | `ResourcesRequest`                | No    | Default struct  | Error on null. Populate suggestion for null objects.                  |
| `spawn_limit`       | `SpawnLimits`                     | No    | `Unlimited`     | Error on null: must be number, 'Unlimited', or {Basic: n}.            |
| `volumes`           | `Vec<Volume>`                     | No    | `[]`            | K8s only. Error on null.                                              |
| `env`               | `HashMap<String, Option<String>>` | No    | `{}`            | K8s only. Error on null.                                              |
| `args`              | `ImageArgs`                       | No    | Default struct  | Error on null. Populate suggestion.                                   |
| `modifiers`         | `Option<String>`                  | Yes   | -               | Allow null (clears modifiers).                                        |
| `description`       | `Option<String>`                  | Yes   | -               | Allow null (clears description).                                      |
| `security_context`  | `Option<SecurityContext>`         | Yes   | `None`          | K8s only. Allow null (object validator handles). Populate suggestion. |
| `collect_logs`      | `bool`                            | No    | `true`          | Error on null: `must be a boolean`.                                   |
| `generator`         | `bool`                            | No    | `false`         | Error on null: `must be a boolean`.                                   |
| `dependencies`      | `Dependencies`                    | No    | Default struct  | Error on null. Populate suggestion.                                   |
| `display_type`      | `OutputDisplayType`               | No    | `JSON`          | Error on null: `must be one of: ...`                                  |
| `output_collection` | `OutputCollection`                | No    | Default struct  | Error on null. Populate suggestion.                                   |
| `child_filters`     | `ChildFilters`                    | No    | Default struct  | Error on null. Populate suggestion.                                   |
| `clean_up`          | `Option<Cleanup>`                 | Yes   | -               | Allow null. Populate suggestion.                                      |
| `kvm`               | `Option<Kvm>`                     | Yes   | -               | Kvm only. Allow null. Populate suggestion.                            |
| `network_policies`  | `HashSet<String>`                 | No    | `[]`            | K8s only. Error on null.                                              |

## `resources` (`ResourcesRequest`)

CPU and memory have custom serde deserializers that accept numbers, floats, or strings with unit suffixes (`1000m`, `4Gi`, `0.5`). The editor schema uses `FieldValueType.Number` for simplicity (suggestions insert plain numbers in mCPU/MiB).

| Field               | Rust Type                   | Null? | Default        | Notes                                      |
| ------------------- | --------------------------- | ----- | -------------- | ------------------------------------------ |
| `cpu`               | `u64` (custom deser)        | No    | 0              | Accepts: `1000`, `1000m`, `0.5`, `"1000m"` |
| `memory`            | `u64` (custom deser)        | No    | 0              | Accepts: `4096`, `"4Gi"`, `"512Mi"`        |
| `ephemeral_storage` | `u64` (custom deser)        | No    | `0`            | Same as memory                             |
| `nvidia_gpu`        | `u64`                       | No    | `0`            | K8s only                                   |
| `amd_gpu`           | `u64`                       | No    | `0`            | K8s only                                   |
| `burstable`         | `BurstableResourcesRequest` | No    | Default struct | K8s only                                   |

Note: `worker_slots` exists on the response `Resources` struct but NOT on `ResourcesRequest`. The editor schema includes it but the API does not accept it on create/update.

### `resources.burstable` (`BurstableResourcesRequest`)

| Field    | Rust Type            | Null? | Notes                                 |
| -------- | -------------------- | ----- | ------------------------------------- |
| `cpu`    | `u64` (custom deser) | No    | Same custom deser as resources.cpu    |
| `memory` | `u64` (custom deser) | No    | Same custom deser as resources.memory |

## `lifetime` (`ImageLifetime`)

| Field     | Rust Type | Null? | Notes                          |
| --------- | --------- | ----- | ------------------------------ |
| `counter` | `String`  | No    | Expected: `"jobs"` or `"time"` |
| `amount`  | `u64`     | No    |                                |

## `args` (`ImageArgs`)

| Field          | Rust Type             | Null? | Default        | Notes                             |
| -------------- | --------------------- | ----- | -------------- | --------------------------------- |
| `entrypoint`   | `Option<Vec<String>>` | Yes   | `None`         |                                   |
| `command`      | `Option<Vec<String>>` | Yes   | `None`         |                                   |
| `reaction`     | `Option<String>`      | Yes   | -              |                                   |
| `repo`         | `Option<String>`      | Yes   | -              |                                   |
| `commit`       | `Option<String>`      | Yes   | -              |                                   |
| `output`       | `ArgStrategy`         | No    | `None` variant | Enum: None, Append, Kwarg(string) |
| `output_files` | `ArgStrategy`         | No    | `None` variant | Same as output                    |

## `security_context` (`SecurityContext`) — K8s only

| Field                        | Rust Type     | Null? | Default | Notes |
| ---------------------------- | ------------- | ----- | ------- | ----- |
| `user`                       | `Option<i64>` | Yes   | -       |       |
| `group`                      | `Option<i64>` | Yes   | -       |       |
| `allow_privilege_escalation` | `bool`        | No    | `false` |       |

## `dependencies` (`Dependencies`)

All sub-sections have `#[serde(default)]` — missing sections use defaults.

### `dependencies.samples` (`SampleDependencySettings`)

| Field      | Rust Type                | Null? | Default                  |
| ---------- | ------------------------ | ----- | ------------------------ |
| `location` | `String`                 | No    | `"/tmp/thorium/samples"` |
| `kwarg`    | `Option<String>`         | Yes   | `None`                   |
| `strategy` | `DependencyPassStrategy` | No    | `Disabled`               |
| `naming`   | `FileNamingStrategy`     | No    | `Sha256`                 |

### `dependencies.repos` (`RepoDependencySettings`)

| Field      | Rust Type                | Null? | Default                |
| ---------- | ------------------------ | ----- | ---------------------- |
| `location` | `String`                 | No    | `"/tmp/thorium/repos"` |
| `kwarg`    | `Option<String>`         | Yes   | `None`                 |
| `strategy` | `DependencyPassStrategy` | No    | `Disabled`             |

### `dependencies.tags` (`TagDependencySettings`)

| Field      | Rust Type                | Null? | Default               |
| ---------- | ------------------------ | ----- | --------------------- |
| `enabled`  | `bool`                   | No    | `false`               |
| `location` | `String`                 | No    | `"/tmp/thorium/tags"` |
| `kwarg`    | `Option<String>`         | Yes   | `None`                |
| `strategy` | `DependencyPassStrategy` | No    | `Directory`           |

### `dependencies.children` (`ChildrenDependencySettings`)

| Field      | Rust Type                | Null? | Default                   |
| ---------- | ------------------------ | ----- | ------------------------- |
| `enabled`  | `bool`                   | No    | `false`                   |
| `images`   | `Vec<String>`            | No    | `[]`                      |
| `location` | `String`                 | No    | `"/tmp/thorium/children"` |
| `kwarg`    | `Option<String>`         | Yes   | `None`                    |
| `strategy` | `DependencyPassStrategy` | No    | `Directory`               |

### `dependencies.ephemeral` (`EphemeralDependencySettings`)

| Field      | Rust Type                | Null? | Default                    |
| ---------- | ------------------------ | ----- | -------------------------- |
| `location` | `String`                 | No    | `"/tmp/thorium/ephemeral"` |
| `kwarg`    | `Option<String>`         | Yes   | `None`                     |
| `strategy` | `DependencyPassStrategy` | No    | `Disabled`                 |
| `names`    | `Vec<String>`            | No    | `[]`                       |

### `dependencies.results` (`ResultDependencySettings`)

| Field      | Rust Type                | Null? | Default                        |
| ---------- | ------------------------ | ----- | ------------------------------ |
| `images`   | `Vec<String>`            | No    | `[]`                           |
| `location` | `String`                 | No    | `"/tmp/thorium/prior-results"` |
| `kwarg`    | `KwargDependency`        | No    | `None` variant                 |
| `strategy` | `DependencyPassStrategy` | No    | `Disabled`                     |
| `names`    | `Vec<String>`            | No    | `[]`                           |

### `dependencies.cache` (`CacheDependencySettings`)

| Field              | Rust Type                        | Null? | Default                |
| ------------------ | -------------------------------- | ----- | ---------------------- |
| `location`         | `String`                         | No    | `"/tmp/thorium/cache"` |
| `generic`          | `GenericCacheDependencySettings` | No    | Default struct         |
| `use_parent_cache` | `bool`                           | No    | `false`                |
| `enabled`          | `bool`                           | No    | `true`                 |

### `dependencies.cache.generic` (`GenericCacheDependencySettings`)

| Field      | Rust Type                | Null? | Default    |
| ---------- | ------------------------ | ----- | ---------- |
| `kwarg`    | `Option<String>`         | Yes   | `None`     |
| `strategy` | `DependencyPassStrategy` | No    | `Disabled` |

## `output_collection` (`OutputCollection`)

| Field           | Rust Type                  | Null? | Default                   |
| --------------- | -------------------------- | ----- | ------------------------- |
| `handler`       | `OutputHandler`            | No    | `Files`                   |
| `files`         | `FilesHandler`             | No    | Default struct            |
| `as_filesystem` | `bool`                     | No    | `false`                   |
| `children`      | `String`                   | No    | `"/tmp/thorium/children"` |
| `auto_tag`      | `HashMap<String, AutoTag>` | No    | `{}`                      |
| `groups`        | `Vec<String>`              | No    | `[]`                      |

### `output_collection.files` (`FilesHandler`)

| Field          | Rust Type     | Null? | Default                        |
| -------------- | ------------- | ----- | ------------------------------ |
| `results`      | `String`      | No    | `"/tmp/thorium/results"`       |
| `result_files` | `String`      | No    | `"/tmp/thorium/result-files"`  |
| `entities`     | `String`      | No    | `"/tmp/thorium/entities.json"` |
| `tags`         | `String`      | No    | `"/tmp/thorium/tags"`          |
| `names`        | `Vec<String>` | No    | `[]`                           |

## `child_filters` (`ChildFilters`)

| Field                | Rust Type         | Null? | Default |
| -------------------- | ----------------- | ----- | ------- |
| `mime`               | `HashSet<String>` | No    | `[]`    |
| `file_name`          | `HashSet<String>` | No    | `[]`    |
| `file_extension`     | `HashSet<String>` | No    | `[]`    |
| `submit_non_matches` | `bool`            | No    | `false` |

## `clean_up` (`Cleanup`)

| Field              | Rust Type     | Null? | Notes    |
| ------------------ | ------------- | ----- | -------- |
| `job_id`           | `ArgStrategy` | No    |          |
| `results`          | `ArgStrategy` | No    |          |
| `result_files_dir` | `ArgStrategy` | No    |          |
| `script`           | `String`      | No    | Required |

## `kvm` (`Kvm`) — Kvm scaler only

| Field   | Rust Type | Null? | Notes    |
| ------- | --------- | ----- | -------- |
| `xml`   | `String`  | No    | Required |
| `qcow2` | `String`  | No    | Required |

## `volumes[]` (`Volume`) — K8s only

| Field        | Rust Type           | Null? | Default | Notes                                            |
| ------------ | ------------------- | ----- | ------- | ------------------------------------------------ |
| `name`       | `String`            | No    | -       | Required                                         |
| `archetype`  | `VolumeTypes`       | No    | -       | Required. Enum: HostPath, ConfigMap, Secret, NFS |
| `mount_path` | `String`            | No    | -       | Required                                         |
| `sub_path`   | `Option<String>`    | Yes   | -       |                                                  |
| `read_only`  | `bool`              | No    | `false` |                                                  |
| `kustomize`  | `bool`              | No    | `false` |                                                  |
| `host_path`  | `Option<HostPath>`  | Yes   | -       |                                                  |
| `config_map` | `Option<ConfigMap>` | Yes   | -       |                                                  |
| `secret`     | `Option<Secret>`    | Yes   | -       |                                                  |
| `nfs`        | `Option<NFS>`       | Yes   | -       |                                                  |

## Scaler-Dependent Fields

Fields that only apply to certain scaler types. The editor suppresses suggestions for irrelevant fields and strips them before API submission.

| Field                  | Applicable Scalers | API Enforcement                               |
| ---------------------- | ------------------ | --------------------------------------------- |
| `volumes`              | K8s                | Accepted by API for all, but only used by K8s |
| `security_context`     | K8s                | Accepted by API for all                       |
| `network_policies`     | K8s                | **Rejected by API** for non-K8s               |
| `env`                  | K8s                | Accepted by API for all                       |
| `kvm`                  | Kvm                | Accepted by API for all                       |
| `resources.nvidia_gpu` | K8s                | Accepted by API for all                       |
| `resources.amd_gpu`    | K8s                | Accepted by API for all                       |
| `resources.burstable`  | K8s                | Accepted by API for all                       |
