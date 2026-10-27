# Harnesses

A **harness** is the program that runs an AI agent's model and tool loop inside a workflow run: it
reads the run's files, calls the model, carries out the model's tool calls, and writes the answer.
Harnesses are published and versioned as tools of kind `Harness` (see
[Tools and Tool Instances](./tools.md)), and an agent is attached to one with
`thorctl agents set-harness` (see [AI Agents, Toolsets, and Workflows](./agents_workflows.md)).
Harnesses run only in K8s.

### The harness spec
---

```yaml
Harness:
  workload:                       # a K8s workload, as in an Image tool
    environment:
      K8s:
        image: registry.example.com/my-harness:1.0
        scheduling:
          resources: {cpu: 2000, memory: 4096}
          spawn_limit: Unlimited
        network_policies: [allow-model-egress]
    execution:
      args: {command: [/usr/local/bin/my-harness]}
  interface:                      # optional
    args:                         # how each file path is passed to the harness
      run: {Kwarg: "--run"}
      agent: {Kwarg: "--agent"}
      tools: {Kwarg: "--tools"}
      models: {Kwarg: "--models"}
      results: {Kwarg: "--answer"}
    tool_access:                  # optional; overrides the deployment defaults
      cli: [/opt/thorium/thorium-agent, tool, run]
  capabilities:
    mechanisms: [ThoriumApi, Cli, Mcp]
    file_io: true
    model_apis: [AnthropicMessages, OpenAiChat]
    credential_kinds: [ApiKey]
    per_agent_models: true
  tool_policy:
    denied_effects: [Destructive]
    max_result_bytes: 65536       # 0 = unlimited
  orphaned_runs: Warn             # Delete (default) or Warn
```

Only `workload` is required; it must run in K8s. The other parts:

| Part | Meaning |
| ---- | ------- |
| `interface.args` | For each of `run`, `agent`, `tools`, `models`, `thorium_keys`, `results`, `result_files`, and `reaction`: `None` (the default, not passed), `Append`, or `{Kwarg: <key>}`. They are appended after the workload's command line (see [Input paths](#input-paths-and-the-answers-display-type)) in that order |
| `interface.inputs`, `interface.outputs` | The file paths, which default to the layout under [Run files](#run-files). The models file (`inputs.models`) and the Thorium keys file (`inputs.thorium_keys`) hold credentials, so each must be an absolute path without `.`, `..`, or empty segments and outside `/tmp/thorium/result-files` and the answer path, whose files are uploaded as results. A harness with either file there is refused when it is published (`400`) |
| `interface.tool_access` | The CLI wrapper command and the MCP url this harness's runs use instead of the deployment's `thorium.tool_access` defaults |
| `interface.mcp_config` | Optionally, the path and format (`Neutral`, `AgentPlugins`, `ClaudeCode`, `Codex`, `GeminiCli`, `VsCode`, or `Goose`) of the MCP config file the harness reads. The path must be absolute and can't replace one of the harness's own files. Thorium checks it when the harness is published but doesn't write the file: the harness builds its MCP client config from the `Mcp` entries in `tools.json` |
| `capabilities.mechanisms` | How the harness can call tools behind Thorium: `ThoriumApi`, `Cli`, and `Mcp`. `Native` can't be listed; it comes from the native tools the harness provides |
| `capabilities.file_io` | Whether the harness can hand files to tools and take files back |
| `capabilities.model_apis`, `credential_kinds` | The model protocols the harness speaks and the credential kinds it can use; runs only resolve to models it can call |
| `capabilities.per_agent_models` | Whether each agent of a run may use a different model. When `false`, a run whose agents resolve to different models is refused |
| `tool_policy` | Effects whose tools the harness refuses to bind, and the largest tool result it accepts |
| `orphaned_runs` | What happens to tool runs still going when the harness exits: `Delete` fails each unfinished one and deletes it (so the workflow run still completes); `Warn` only logs them |

#### Input paths and the answer's display type

The workload's `data` settings apply to a harness job like to any other job. Two of them matter for
most harnesses:

```yaml
Harness:
  workload:
    # environment and execution as above
    data:
      dependencies:
        samples: {strategy: Disabled}
        ephemeral: {strategy: Disabled}
      display_type: String
```

- `data.dependencies`: by default the agent passes the path of every downloaded sample and
  ephemeral file as an argument in the workload's command line, before the `interface.args`
  values. The run's `run.json`, `agent.json`, and `tools.json` are ephemeral files,
  so a harness launched with the defaults gets their paths plus the sample paths as positional
  arguments. A harness that reads its files from the standard paths and takes no positional
  arguments sets `samples` and `ephemeral` to `{strategy: Disabled}`; the files are still
  downloaded.
- `data.display_type`: the answer in `/tmp/thorium/results` is stored with this display type, which
  is `JSON` by default. A harness whose answer is plain text sets `String`.

### Binding tools
---

When a workflow instance is built, each agent's tools are bound to a mechanism, in priority order
`Native`, `ThoriumApi`, `Cli`, `Mcp`: the first one both the tool and the entry agent's harness
support. `ThoriumMcp` and `Container` tools support `ThoriumApi`, `Cli`, and `Mcp`; `HarnessNative`
tools support only `Native`. A tool can't be bound, and the workflow instance is Invalid with every
reason listed, when it needs file I/O and the harness has no `file_io`, has an effect the harness
denies, is a native tool the harness version doesn't provide, or shares no mechanism with the
harness. The bindings are part of the workflow instance's digest.

### Native tools
---

A tool built into a harness is published as an agentic tool with a `HarnessNative` backing (no
`timeout` is needed, and `config` is an optional JSON string passed through unchanged):

```yaml
Agentic:
  function_name: web_search
  model_description: Search the web with the harness's built in search
  input_schema: {type: object, properties: {}}
  backing:
    HarnessNative:
      tool: web_search
```

A harness version lists the native tool instances it provides when it is published, so publish the
native tools first:

```bash
thorctl tools publish my-harness --version 1.1.0 --spec harness.yaml \
  --provides <NATIVE_TOOL_INSTANCE_ID>
```

`--provides` can be repeated, up to twice `thorium.events.max_tools_per_agent` (256 by default); a
longer list is refused with `400`. The list is frozen into the harness version and its spec hash, so the
same spec with another list is a new version. Native tools can't be run directly.

### Model values in the pod
---

Two settings sit on the harness tool itself, are edited without publishing, and apply to every run
from then on:

```bash
# export the entry agent's API key under the standard name, and its base url under another name
thorctl tools harness-env my-harness --set OpenAiApiKey --set OpenAiBaseUrl=LITELLM_BASE_URL
# export nothing
thorctl tools harness-env my-harness --clear
# never resolve a run of this harness to these model ids (and allow one again)
thorctl tools harness-models my-harness --disable-model gpt-4o --enable-model gpt-5
```

The values given to `--set` replace the whole list. Each value is exported under its standard env
var name unless renamed: `ModelId` (`THORIUM_MODEL`), `ModelsFile` (`THORIUM_MODELS_FILE`),
`OpenAiApiKey`, `OpenAiBaseUrl`, `AnthropicApiKey`, `AnthropicBaseUrl`, `AwsAccessKeyId`,
`AwsSecretAccessKey`, `AwsSessionToken`, `AwsRegion`, `AwsConfigFile`, `AwsProfile`,
`AwsBearerTokenBedrock`, `GoogleApplicationCredentials`, `GoogleCloudProject`,
`GoogleCloudLocation`, `GeminiApiKey`, `AzureOpenAiApiKey`, `AzureOpenAiEndpoint`,
`AzureApiVersion`, `AzureTenantId`, `AzureClientId`, `AzureClientSecret`, and
`AzureClientCertificatePath`. Nothing is exported by default. The values are the entry agent's
resolved model and credential, and they are written into the pod spec in plaintext.

A value can't be renamed to a name Thorium or the system depends on: `THORIUM_URL`,
`THORIUM_TOKEN`, and `THORIUM_MODELS`, anything starting with `THORIUM_` (including
`THORIUM_KEYS__*`), `PATH`, `HOME`, anything starting with `LD_`, and anything ending in `_PROXY`.
The names are compared whatever their case, so `https_proxy` is reserved too. Publishing or
updating a harness env that renames a value to one of them is refused (`400`), naming the value.
The standard names above, such as `THORIUM_MODEL`, stay as they are.

### Run files
---

Every per-run pod has these env vars:

| Variable | Value |
| -------- | ----- |
| `THORIUM_URL` | The API url |
| `THORIUM_TOKEN` | The run's [run-scoped token](../users/runs.md#run-scoped-tokens) |
| `THORIUM_MODELS` | Harness pods only: every agent's resolved model and credential, as JSON. The agent removes it from the harness's env |

And the harness reads and writes these files (the paths are the defaults):

| File | Written by | Content |
| ---- | ---------- | ------- |
| `/tmp/thorium/ephemeral/run.json` | API | The run's inputs, reaction, workflow instance and digest, entry agent, limits, result size limit, and resolved models (no secrets) |
| `/tmp/thorium/ephemeral/agent.json` | API | The entry agent and its direct sub-agents |
| `/tmp/thorium/ephemeral/tools.json` | API | Each bound tool's model-facing descriptor and how to invoke it, keyed by tool instance id |
| `/tmp/thorium/ephemeral/models.json` | agent (mode 0600) | The models and credentials of the run's agents |
| `/tmp/thorium/keys.yml` | agent (mode 0600) | A keys file holding the API url and the run's token |
| `/tmp/thorium/tool-runs.jsonl` | CLI wrapper | The tool runs the wrapper started |
| `/tmp/thorium/results` | harness | The answer, stored as the run's result |
| `/tmp/thorium/result-files/` | harness | Result files; `conversation.jsonl`, `traces.jsonl`, and `status.json` are always uploaded |

The API writes `run.json`, `agent.json`, and `tools.json` once, when the run is created; a reaction
update that would replace any of them is refused with `401`, so a harness only reads files Thorium
generated for its run.

Results and result files are uploaded only when the harness exits with `0`. A non-zero exit fails
the run and uploads nothing. The harness's stdout and stderr stream to the stage logs like any job's.

### Running tools from a harness
---

A harness bound to `Cli` runs tools behind Thorium with the CLI wrapper in the agent binary (the
`thorium.tool_access.cli` command):

```bash
/opt/thorium/thorium-agent tool run <TOOL_INSTANCE_ID> --input '{"depth": 2, "samples": ["<SHA256>"]}'
/opt/thorium/thorium-agent tool status <REACTION>
/opt/thorium/thorium-agent tool results <REACTION>
```

`tool run` also takes `--input-file`, `--output <DIR>` (the tool's output path by default,
`/tmp/thorium/tool-results/<function_name>`), `--timeout <SECONDS>` (which can only shorten the
tool's own timeout), and `--no-wait` (print the reaction id right after starting the run). Each
command prints one JSON object and exits with the codes listed in
[Running Tools and Workflows](../users/runs.md#tool-run-output). Inside a workflow run the wrapper
only runs tool instances listed in the run's `tools.json` (it refuses all tools when that file is
missing), and every run it starts is a sub run of the workflow run that shares its token, queue,
and deadline. A harness bound to `Mcp` calls the
[Thorium MCP server](../admins/agentic/mcp.md) at the url in each tool's `tools.json` entry with
the run's token instead, and `ThoriumApi` harnesses call the API directly with it.
