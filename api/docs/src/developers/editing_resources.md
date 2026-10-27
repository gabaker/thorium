# Editing Resources

Most things you create with Thorctl can be changed afterwards in two ways: **interactively**, by
opening the resource as a document in your text editor, or **non-interactively**, from flags and
files a script can pass. Both go through the same checks, show the same diff, and call the same
update, so what an editor session does and what a `--set` does can't drift apart. This page covers
the commands, the editor session, and the non-interactive forms.

### What you can edit
---

| Resource | Open in an editor | Change without one | Print the document |
| -------- | ----------------- | ------------------ | ------------------ |
| A tool's spec (publishes a new version) | `thorctl tools edit TOOL[@VERSION]` | `thorctl tools edit TOOL --set ...` | `thorctl tools spec TOOL[@VERSION]` |
| A tool itself (name, description, groups, tags) | `thorctl tools edit TOOL --root` | `thorctl tools update TOOL` | |
| A toolset | `thorctl toolsets edit` | `thorctl toolsets update` | `thorctl toolsets spec` |
| An AI agent | `thorctl agents edit` | `thorctl agents update` | `thorctl agents spec` |
| A workflow | `thorctl workflows edit` | `thorctl workflows update` | `thorctl workflows spec` |
| A model endpoint | `thorctl models endpoints edit` | `thorctl models endpoints update` | `thorctl models endpoints spec` |
| A model profile | `thorctl models profiles edit` | `thorctl models profiles update` | `thorctl models profiles spec` |
| A pipeline (including each stage's tool) | `thorctl pipelines edit` | `thorctl pipelines update` | |

Images can't be edited any more because Thorium no longer accepts image updates; `thorctl images edit`
stops with a pointer to the tool commands. Credentials have no editor flow, so a secret is never
written to an editor buffer.

**`edit` versus `update`.** `update` never opens an editor: it changes the resource from the flags
and files you give it, and fails when there is nothing to change. `edit` opens the editor when you
give it no change, and behaves exactly like `update` when you do (`thorctl agents edit triage-lead
--set config.max_turns=20` never opens an editor). Use `edit` by hand and `update` in scripts.

### The document
---

Each resource is shown as a YAML document that names things instead of carrying ids: an agent lists
its tools by name and version, a workflow lists its agents by name, a pipeline lists the tool
behind each stage. Print one with the resource's `spec` command (YAML by default, `--format json`
for JSON, `--output FILE` to write a file, which replaces an existing file the way a shell
redirect does):

```bash
thorctl agents spec triage-lead
thorctl workflows spec malware-triage --format json
thorctl toolsets spec static-analysis --output static-analysis.yaml
```

A `spec` document is also what `update --file` reads, so `spec` followed by `update --file -` is a
no-change, and what `toolsets create --spec` and `workflows create --spec` read. The documents
refuse keys they don't know, so a typo fails before anything is sent.

Keys written `*like-this*` are **read-only**: they are shown so you know what you are editing
(`*id*`, `*kind*`, `*latest*`, `*groups*` on model documents) and are ignored on save. If you change
one in the editor or in a `--file`, Thorctl prints `Warning: Ignoring edits to read-only field(s): *id*`
and goes on with the rest. `--set` and `--unset` refuse a read-only field outright.

### Editing in your editor
---

```bash
thorctl agents edit triage-lead
thorctl tools edit strings@1.0.0 --version 1.0.1
thorctl pipelines edit static
thorctl workflows edit malware-triage --editor "code --wait"
```

Thorctl fetches the resource, writes its document to a private temporary file, opens your editor on
it, and waits for the editor to exit. The editor is chosen in the order described in
[Choosing an editor](./import_export.md#choosing-an-editor): `--editor`, the `default_editor` in your
Thorctl config, `$VISUAL`, `$EDITOR`, then `vi`. Use a command that waits until the file is closed
(`code --wait`, not `code`). Editing needs a terminal on stdin and stderr; without one the command
fails with `Editing agent requires an interactive terminal; pass --file, --set, --unset or --patch to
edit without one`.

The buffer starts with a comment header that is ignored when the file is read back:

```yaml
# Editing agent 'triage-lead' (id 6f1c5a52-0d7b-4a3e-9d0b-2f1f2f6a8b11)
# Save and quit to review the changes. Leave the file unchanged or empty to cancel.
# Lines starting with '#' are comments.
# Fields written as *field* are read-only: edits to them are ignored.
*id*: 6f1c5a52-0d7b-4a3e-9d0b-2f1f2f6a8b11
name: triage-lead
...
```

What happens when you save and quit:

1. **Nothing changed, or the buffer is empty.** The edit is cancelled with `Cancelled.` and exit
   code 0 (an unchanged document is reported as `No changes detected for agent 'triage-lead'.`).
2. **The text doesn't parse or doesn't validate.** An unknown key, a bad value, a name that doesn't
   exist, or a missing required field is reported before any request is sent, and you choose
   **Edit** (reopen the editor with your text) or **Cancel**. The same menu appears when the server
   refuses the update, so you can fix the problem without retyping.
3. **Diff and confirm.** The fields that change are shown, then you are asked `Apply these changes to
   agent 'triage-lead'?` (the default is yes). Answer no and the edit is cancelled.
4. **Remote-change guard.** Just before the update, Thorctl reads the resource again. If it changed
   since you opened it, nothing is applied and the error says `Agent 'triage-lead' changed remotely;
   re-run or pass --force`. Your text is saved to the recovery file below.
5. **Apply.** On success it prints `Agent 'triage-lead' updated successfully!`.

The diff is a unified diff of the two documents as YAML with a little context around each change
(removed lines in red with a `-`, added lines in green with a `+`, and `  ...` between distant
changes). Only editable fields are compared:

```text
Changes to toolset 'static-analysis':
  name: static-analysis
- description: Static tools
+ description: Static analysis tools
  groups:
  - analysts
```

**Recovery file.** Text you saved but that was never applied (you cancelled after an error, declined
the confirmation, or the guard refused) is written to a file only you can read in your temporary
directory, `thorium-edit-<resource>-<uuid>.yml`. The message names it (`Your edits were saved to
'/tmp/thorium-edit-agent-triage-lead-....yml'`, or `... (your edits were saved to '...')` at the end
of an error), so nothing you typed is lost: fix the problem and pass the file back with
`update --file`.

**Skipping the question.** `--skip-confirm` (alias `--yes`) skips the confirmation after an editor
session. The non-interactive forms below never ask, so scripts don't need it.

### Changing a resource without an editor
---

Give `edit` or `update` any of these and no editor opens. They can be combined and are applied in this
order: `--file` (or the current document), then each `--patch`, then every `--set` and `--unset` in
the order you typed them.

| Flag | What it does |
| ---- | ------------ |
| `--file FILE` | Replace the whole document with a YAML or JSON file; `-` reads stdin |
| `--patch JSON` | Apply an [RFC 7386](https://www.rfc-editor.org/rfc/rfc7386) merge patch given as JSON or YAML text, or as `@FILE`; repeatable. A `null` value removes a key |
| `--set PATH=VALUE` | Set one field; repeatable |
| `--unset PATH` | Remove one field; repeatable |

Examples for each kind of resource:

```bash
# an agent: one setting, then a whole document
thorctl agents update triage-lead --set config.max_turns=20
thorctl agents update triage-lead --file lead.spec.yaml

# a toolset: replace its tool list, or clear the selector
thorctl toolsets update static-analysis --set 'tools=[{tool: strings}, {tool: yara-scan, version: 1.0.0}]'
thorctl toolsets update static-analysis --unset selector

# a workflow: a merge patch
thorctl workflows update malware-triage --patch '{"limits": {"max_alive_sub_agents": 8}}'

# a tool's root, and a new version of its spec
thorctl tools update strings --set 'groups=[analysts,reversers]'
thorctl tools edit strings --set Image.execution.timeout=600 --bump patch

# a model endpoint and profile
thorctl models endpoints update anthropic --set description="Group Anthropic key"
thorctl models profiles update opus-careful --set parameters.temperature=0.1

# a pipeline, including the tool behind a stage
thorctl pipelines update static --set stage_tools.strings.version=1.1.0
thorctl pipelines update static --patch @static.patch.yaml
```

**Paths.** A path is dotted (`config.max_turns`), with `[N]` for list items (`groups[1]`). A key that
contains `.`, `[`, or `\` escapes them with a backslash. `--set` creates missing objects along the
path, replaces an item in range, appends at the end of a list (`tools[2]` on a two-item list), and
fails beyond that. `--unset` of a path that isn't there does nothing, so a script can run again, but
going through a value that isn't an object is an error.

**Values.** Only `true`, `false`, `null`, `~`, and JSON numbers leave string form (`--set
config.max_turns=5` is a number); quotes force a string (`--set 'description="5"'`); text starting
with `[` or `{` is read as a YAML flow list or mapping (`--set 'groups=[a,b]'`); anything else is a
string (`0x10`, `+5`, and `a: b` stay strings).

**Lists must be complete.** A document lists every list field of its resource, so a `--file` that
leaves one out is an error instead of silently emptying the list; empty one deliberately with
`--set tools=[]`.

**Typed flags.** The `update` commands also have flags for the common changes (`--rename`,
`-d/--description`, `--add-group`, `--remove-group`, `--tag`, `--order`, `--stage-tool`, and so
on; see each command's help). Depending on the resource they are turned into the same edits and applied before
yours, or must not be mixed with the document flags (tools and models refuse a mix with `not both`).
A flag that does nothing on its own, such as `--dry-run`, `--force`, or `-o` with no change, is an
error on the resources that check for it.

### Checking before you apply
---

- `--dry-run` shows the diff and stops: nothing is sent. The result says so (`Dry run: agent
  'triage-lead' was not changed.`).
- `-o json` or `-o yaml` prints the result as a document, with the human diff on stderr so stdout
  stays a document:

  ```json
  {"kind":"agent","id":"6f1c5a52-0d7b-4a3e-9d0b-2f1f2f6a8b11","name":"triage-lead","status":"updated",
   "changed":true,"dry_run":false,"diff":[{"path":"config.max_turns","old":12,"new":20}],"resource":{}}
  ```

  `status` is `updated`, `no_changes`, `dry_run`, or `cancelled`, and `resource` is the document as
  re-read after the update.
- **The remote-change guard** applies to every form: if someone else changed the resource between
  Thorctl's two reads, nothing is applied. Pass `--force` to apply anyway, which overwrites what
  they changed. For a spec edit of a tool, `--force` keeps the label worked out from the first read,
  so a concurrent publish of the same label is refused with `409`; pass `--version` as well.

### Exit codes
---

| Exit code | Meaning |
| --------- | ------- |
| `0` | The update was applied, there was nothing to change, it was a dry run, or you cancelled (an empty buffer, or declining the confirmation) |
| `1` | The edit was invalid or refused: a validation error, a server refusal, a remote change, or cancelling after an error (the recovery file is named) |
| `2` | The flags were bad, such as a `--set` with no `=` or two conflicting flags |

Ctrl-C while the editor is open is handled by the editor; if Thorctl itself is interrupted (or the terminal closes) it removes the temporary directory that held the document. Image edits are not available; see the note above.
