# Thorctl Edit Framework

Most of Thorctl's `edit` and `update` commands run on one shared flow in `thorctl/src/handlers/edit`.
A resource that implements the `Editable` trait gets, from one implementation, an editor session, the
non-interactive forms (`--file`, `--patch`, `--set`, `--unset`), a diff, a confirmation, a
remote-change guard, `--dry-run`, and result output in text, JSON, or YAML. This page is for people
adding a new editable resource. How users see all of this is in [Editing
Resources](../developers/editing_resources.md).

### The pieces
---

| Piece | Where | What it is |
| ----- | ----- | ---------- |
| `Editable` | `handlers/edit/editable.rs` | The trait a resource implements: fetch, view, validate, update, send |
| `EditArgs` | `args/helpers.rs` | The shared flags to flatten into an `edit` or `update` command |
| `edit_with`, `update_with` | `handlers/edit/flow.rs` | The two entry points a command handler calls |
| `EditOutcome` | `handlers/edit/editable.rs` | What an edit did (`Updated`, `NoChanges`, `DryRun`, `Cancelled`), its diff, and any recovery file |
| `Seams` | `handlers/edit/seams.rs` | Terminal checks and scripted prompt answers, read once from the environment |
| `utils::patch` | `utils/patch.rs` | Merge patches and the `--set` and `--unset` path rules |
| `utils::spec_io` | `utils/spec_io.rs` | Reading YAML or JSON documents and rendering them (`spec` commands) |

Images and the import merge editor use the older editor loop in `handlers/imports/editor.rs`; the
framework reuses its editor launch, error menu, and recovery file helpers.

### Adding an editable resource
---

**1. Define the view.** The view is the document people edit. It must be `Serialize`,
`Deserialize`, and `Clone`; refuse unknown keys with `#[serde(deny_unknown_fields)]`; name things
(agents, tools, groups) instead of carrying ids; and hold nothing that changes on every read, because
the remote-change guard compares two reads of it. Mark read-only context fields with a key written
`*like-this*` (`#[serde(rename = "*id*", default)]`); the flow ignores edits to them, warns, and
refuses `--set` on them. A list the view omits must be an error rather than an empty list, so make list
fields required (no `#[serde(default)]`) when an omission would silently remove data.

**2. Implement `Editable`.**

| Item | Purpose |
| ---- | ------- |
| `type Client`, `type Target` | What talks to Thorium (the `Thorium` client in production, an in-memory fake in tests) and what picks the resource (an id, or a name and group) |
| `type Data`, `type View`, `type Update` | The resource as fetched, the document, and the request that applies a change |
| `const KIND` | The word used in messages and the JSON `kind` (`Agent`) |
| `const FIELD_ORDER` | The top-level key order of the YAML (see `utils::curated_yaml`) |
| `static_fields()` | Extra read-only keys by name, beyond the `*key*` convention |
| `fetch`, `to_view`, `id`, `name` | Read the resource and describe it |
| `validate` | Check an edited view against the fetched resource before any request (names that exist, rules between fields) |
| `calculate_update` | Turn the edited view into the request, resolving names to ids with the client; return `None` when nothing changed |
| `send_update` | Send the request |
| `header` | The comment block at the top of the editor buffer (the default names the resource, how to cancel, and the read-only markers) |

`calculate_update` must ignore static fields when it builds the request, and `to_view` must be
deterministic for an unchanged resource. Errors from `validate` and `calculate_update` are shown to the
person, who may reopen the editor, so make their messages name the field.

**3. Add the flags.** Flatten `EditArgs` into the command structs (`#[clap(flatten)] pub edit:
EditArgs`) for `edit` and `update`. `EditArgs` is a hand-written `clap::Args` over a derived struct so
that `--set` and `--unset` keep command-line order (it reads their positions with `indices_of`); do not
replace it with a plain derive. Give `update` typed flags only when they have a clear meaning, and turn
them into `EditOp::Set` edits placed before the user's own (as `toolsets update` does) so the shared
flow, diff, and guard apply to them too.

**4. Call the flow.** In the handler, find the target, then call `edit_with::<MyEditable>(client, conf,
&target, &args)` for `edit` or `update_with::<MyEditable>(client, &target, &args)` for `update`; both
print the outcome. `update_with` never opens an editor and fails with `Nothing to update ...` when
`args` holds no change source. For a `spec` command, build the view with `to_view` and print it with
`utils::curated_yaml` or `curated_json`, so that `spec | update --file -` is a no-change round trip.

### What the flow does
---

`run` (in `flow.rs`) takes an `Editable`, the flags, a `Mode` (`Edit` or `Update`), and the `Seams`:

1. Decide whether an editor is needed (no `--file`, `--patch`, `--set`, or `--unset`), and fail early
   if it is needed but there is no terminal, or if the mode is `Update`.
2. Fetch the resource and snapshot its view as a JSON document.
3. Build the candidate document: the editor's text, or `--file`, then each `--patch`, then every `--set`
   and `--unset` in order. A buffer that is blank or only comments cancels the edit.
4. Parse it into the view type and run `validate`. In an editor session an error offers the **Edit** or
   **Cancel** menu; Edit reopens the editor with the person's own text.
5. Warn about changes to read-only fields, diff the editable fields, and call `calculate_update`. No
   request means `NoChanges`.
6. Print the unified diff and stop for `--dry-run`.
7. After an editor session without `--skip-confirm`, ask for confirmation; declining is `Cancelled`.
8. Run the remote-change guard: read the resource again and refuse if its view differs from the first
   read, unless `--force`.
9. Call `send_update`; on a refusal the editor session offers Edit or Cancel again.

Text that was saved but never applied is written to `thorium-edit-<kind>-<name>-<uuid>.yml` in a
private file under the temp directory, and the error or message names it. Cancelling after an error
is exit 1 through an `Error` that names the file; an empty buffer or a declined confirmation is a plain
cancel (exit 0). The result is printed by `print_outcome` in text, or as the JSON document `EditOutcome::to_json`
builds (with the human diff on stderr so stdout stays a document).

### Test seams
---

The flow reads three hidden environment variables so tests can drive the real binary without a
terminal. They are for tests and scripts that exercise the flow; they are not part of the supported
command line.

| Variable | Effect |
| -------- | ------ |
| `THORCTL_EDIT_ASSUME_TTY=1` | Treat the session as a terminal for the editor check only, so `edit` runs an `--editor` script without a real terminal |
| `THORCTL_EDIT_PROMPT=cancel\|reedit` | Answer the Edit/Cancel menu when stdin isn't a terminal (`reedit` reopens the editor), and `cancel` also declines the confirmation. Without it, an error cancels (writing the recovery file) and a confirmation is accepted |
| `THORCTL_EDIT_BEFORE_APPLY=COMMAND` | Run a shell command after the first read and before the remote-change check, so a test can change the resource in between. It is inherited by any `thorctl` the command starts, so a hook that runs an `update` must clear it first (`env -u THORCTL_EDIT_BEFORE_APPLY thorctl ...`) or it recurses |

`Seams` reads these once (`Seams::from_env`), so the flow's unit tests build a `Seams` value instead of
touching the process environment.

### Testing a new resource
---

Three layers of tests are used, each with its own helper:

- **Unit tests of the view.** Test `to_view`, `validate`, and `calculate_update` directly, as the
  `handlers/*/edit/tests` modules do. The framework's own tests (`handlers/edit/tests`) drive `run`
  against a fake resource and an in-memory store (`tests/fake.rs`), which shows the shape a fake needs:
  an `Editable` with `Client` set to the store.
- **Integration tests of the binary** (`thorctl/tests`, which run against a test Thorium API). Pass an editor script as `--editor`. `EditorScript` in
  `tests/common/editor.rs` writes a small shell script that performs one `Step` per invocation
  (`Replace`, `Write`, `Append`, `Noop`, or `Fail`), repeating the last, and records the buffer it was
  shown, so a test can assert on the header comment and read-only markers:

  ```rust
  let script = EditorScript::replace("max_turns: 12", "max_turns: 20");
  let editor = script.editor();
  let out = user
      .run("agents-edit", &["agents", "edit", "triage-lead", "--editor", &editor])
      .await;
  assert_eq!(script.invocations(), 1);
  assert!(script.seen(0).unwrap().contains("# Editing agent 'triage-lead'"));
  ```

  Set `THORCTL_EDIT_ASSUME_TTY=1` for the run, and `THORCTL_EDIT_PROMPT=cancel|reedit` to answer
  prompts. `recovery_path` finds the recovery file an error names.
- **Real terminal tests.** `Pty` (also in `tests/common/editor.rs`) runs a program on a pseudo-terminal
  so the `dialoguer` prompts that need `/dev/tty` can be driven: `Pty::spawn`, `send`, and `read_until`.
  The framework has one pty smoke test; resources rarely need their own.

`tests/common/editor.rs` uses only the standard library, `libc`, and `uuid`, so the integration tests
and thorctl's own unit tests (through a `#[path]` include) share it. Test the remote-change guard with a
`THORCTL_EDIT_BEFORE_APPLY` hook that changes the resource, and the `--force` path by repeating it with
the flag.
