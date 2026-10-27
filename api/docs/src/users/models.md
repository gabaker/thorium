# Models

Workflow runs call AI models. Agents name the models they accept by model id, and Thorium finds an
endpoint and a credential for each one when a run is created, so agents never carry anyone's
endpoint or key. This page covers setting up the model endpoints, credentials, and profiles a group
or a user provides, with `thorctl models`.

| Item | What it is | Who manages it |
| ---- | ---------- | -------------- |
| Model endpoint | A model provider a group can call, with its connection settings, an optional group credential, a catalog of the models it serves, and model ids it should never resolve to | Owners and managers of every one of its groups |
| Model credential | A secret that pays for an endpoint's calls, of a kind the provider accepts | Owners and managers of every one of its groups |
| Model profile | One catalog model on one endpoint, with the protocol to call it with and parameters | Anyone who can edit in its groups |
| Your model settings | Your own endpoints, your own credentials for your or your groups' endpoints, and your default profiles | You |

An admin must first put the provider and url on the deployment's allowed endpoint list; until then
no endpoint can be created (see [Model Endpoints for Admins](../admins/agentic/models.md)).

Providers are `OpenAi`, `Anthropic`, `Bedrock`, `Vertex`, `Gemini`, `AzureOpenAi`, `LiteLlm` (a
LiteLLM gateway), and `OpenAiCompatible` (vLLM, Ollama, and other servers that speak the OpenAI
API). Credential kinds are `ApiKey`, `AwsAccessKey`, `AwsAssumeRole`, `AwsBearerToken`,
`GcpCredentialsJson`, and `AzureEntraClient`.

### Group endpoints and credentials
---

Create a credential from a spec and a secret file (`-` reads the secret from stdin). Secrets are
never printed or returned; Thorium shows a fingerprint instead.

```yaml
# credential.yaml
kind: ApiKey
expires: 2027-01-01T00:00:00Z   # optional
```

```bash
thorctl models credentials create --spec credential.yaml --name anthropic-key --group analysts \
  --secret-file - < ./anthropic.key
```

Then create an endpoint that uses it:

```yaml
# endpoint.yaml
provider:
  Anthropic: {}
credential: <CREDENTIAL_ID>
```

```bash
thorctl models endpoints create --spec endpoint.yaml --name anthropic --group analysts
```

Creating an endpoint, changing its connection, or rotating a credential it uses queues a catalog
refresh: the event handler asks the provider which models it serves and adds them to the endpoint's
catalog. Ask for one by hand with `thorctl models refresh <ENDPOINT>` (add
`--wait-for <field>=<value>` to wait for it). A model the provider stops listing is banned rather than
removed, and unbanned when it returns.

```bash
thorctl models endpoints get
thorctl models endpoints describe anthropic
thorctl models catalogs get anthropic
thorctl models catalogs describe anthropic claude-3-haiku
# stop resolving runs to a model on this endpoint, or allow it again
thorctl models endpoints update anthropic --disable-model claude-3-haiku
thorctl models credentials rotate anthropic-key --secret-file ./new.key
```

Credentials have `get`, `describe`, `update`, and `delete` commands too. They show fingerprints, never
secrets:

```bash
thorctl models credentials get
thorctl models credentials describe anthropic-key
# change when a credential stops working, or forget the expiration
thorctl models credentials update anthropic-key --expires 2027-06-01T00:00:00Z
thorctl models credentials update anthropic-key --clear-expires
thorctl models credentials delete anthropic-key
```

`credentials update` also takes `--clear-session-token`, which removes an AWS session token.
`credentials create` and `credentials rotate` take `--session-token-file` (`-` reads stdin) to store
an AWS session token with the secret.

`models endpoints update` also takes `--provider`, `--credential`, `--clear-credential`,
`--add-models`, `--remove-model`, `--enable-model`, and `--description`. An endpoint can't be deleted
while a profile uses it, and a credential can't be deleted while an endpoint uses it. Delete an
endpoint with `thorctl models endpoints delete <ENDPOINT>`.

#### Editing an endpoint

`models endpoints spec` prints the document that `edit` shows, and `edit` opens it in your editor.
Only owners and managers of the endpoint's groups can change it. The document holds the endpoint's
`name`, `description`, `provider`, `credential` (by name), `disabled_models`, and `catalog`; its `*id*`
and `*groups*` are shown for context and ignored on save. Secrets are never in the document, the
buffer, the diff, or the output.

```bash
# print the document, to a file or stdout
thorctl models endpoints spec anthropic --output anthropic.yaml
# open it in your editor, then review and confirm the changes
thorctl models endpoints edit anthropic
# change it without an editor
thorctl models endpoints update anthropic --file anthropic.yaml
thorctl models endpoints update anthropic --set description="Group Anthropic key" --dry-run
thorctl models endpoints update anthropic --set 'disabled_models=[claude-3-haiku]'
```

Catalog entries that came from a refresh look like manual ones in the document; editing one turns
it into a manual entry and leaving one out removes it. The flags `--provider`, `--credential`,
`--add-models`, `--disable-model`, and the rest can't be combined with `--file`, `--patch`, `--set`,
or `--unset`, and a lone `--dry-run`, `--force`, `--skip-confirm`, or `-o` with nothing to change
is an error. See [Editing Resources](../developers/editing_resources.md) for the flags the two
commands share. There is no editor flow for credentials, so a secret is never shown.

### Profiles
---

A profile pins one catalog model on one endpoint with a protocol and parameters:

```yaml
# profile.yaml
endpoint: <ENDPOINT_ID>
model_id: claude-opus-4
api: AnthropicMessages
parameters: {temperature: 0.2, max_output_tokens: 8192}
```

```bash
thorctl models profiles create --spec profile.yaml --name opus-careful --group analysts
thorctl models profiles get
thorctl models profiles describe opus-careful
thorctl models profiles delete opus-careful
```

Change a profile with `thorctl models profiles update <PROFILE>`, which takes `--endpoint`,
`--model-id`, `--api`, `--parameters`, `--limits`, `--clear-limits`, `--capabilities`, and
`--clear-capabilities`. `--limits` and `--capabilities` read a YAML or JSON file that replaces the
catalog entry's, and the `--clear-` flags go back to the catalog entry's.

A profile has the same `spec`, `edit`, and document forms of `update` as an endpoint. Its document
holds `name`, `description`, `endpoint`, `model_id`, `api`, `parameters`, `limits`, and
`capabilities` (with `*id*` and `*groups*` for context):

```bash
thorctl models profiles spec opus-careful
thorctl models profiles edit opus-careful
thorctl models profiles update opus-careful --set parameters.temperature=0.1
thorctl models profiles update opus-careful --file opus-careful.yaml --dry-run
thorctl models profiles update opus-careful --clear-limits
```

Protocols are `OpenAiChat`, `OpenAiResponses`, `AnthropicMessages`, `BedrockConverse`,
`BedrockInvoke`, `GeminiGenerateContent`, `GeminiInteractions`, and `VertexRawPredict`. Parameters
can be `temperature`, `top_p`, `max_output_tokens`, `thinking_budget_tokens`, and `stop_sequences`;
Thorctl refuses a spec or `--parameters` file with any other key.

### Your own model settings
---

You can call models with your own endpoints and keys, which only your runs use:

```bash
thorctl models settings add-endpoint --spec my-endpoint.yaml
thorctl models settings set-credential my-gateway --spec credential.yaml --secret-file ./my.key
thorctl models settings set-default opus-careful
thorctl models settings get
```

`my-endpoint.yaml` holds the endpoint's `name`, `provider`, and optionally a `models` catalog and
`disabled_models`. `set-credential` also accepts a group endpoint, so you can pay for a group
endpoint with your own key. `set-default` sets the profiles tried, in order, when an agent accepts
any model; with no profiles it clears them. `update-endpoint` disables
(`--disable-model`) or enables (`--enable-model`) model ids on one of your endpoints, `remove-endpoint`
removes an endpoint and your credential for it, and `remove-credential` removes only the credential.
`set-credential` takes `--session-token-file` (`-` reads stdin) for an AWS session token, and
without `--secret-file` it keeps the stored secret.

### How a run picks its models
---

When a workflow run is created, each agent gets one model:

1. The agent's `models` are tried in priority order, or the workflow's default models when the agent
   has none.
2. For each model, the run group's endpoints and credentials are tried first, then yours, limited
   to the levels the workflow allows (`--model-auth`).
3. A candidate is skipped when it is banned, disabled on the endpoint, on your endpoint, or on the
   harness, not on the deployment's allowed list, below the agent's model requirements, or uses a
   protocol or credential kind the harness can't use.

If any agent can't get a model, or a harness that runs every agent on one model would get different
ones, the run is refused with `400` and nothing is created; the message lists every model with the
reason it was skipped at each level. The choices are recorded on the run (see
`thorctl runs describe`) without secrets, and a retry picks again.

### Bans and notifications
---

Endpoints, catalog models, profiles, and credentials are banned automatically when a refresh finds
a problem (a model removed, a credential rejected or expired, an endpoint unreachable, a broken
reference, or an endpoint no longer allowed by the deployment), and the ban clears when the problem
is fixed. Owners and managers can ban an item by hand and clear bans:

```bash
thorctl models bans get
thorctl models bans describe <BAN_ID>
thorctl models bans add anthropic --model claude-3-haiku --message "Too slow for triage"
thorctl models bans clear anthropic <BAN_ID>
```

Every ban sends a notification to the people it affects. Read yours with
`GET /api/users/notifications/`; the Web UI doesn't show them yet.

### Using the API
---

Model endpoints, profiles, and credentials are entities (kinds `ModelEndpoint`, `ModelProfile`, and
`ModelCredential`), so scripts use the entities routes (the `entities` document of the
[REST API Reference](../architecture/api.md#rest-api-reference)). Your own settings and notifications
are part of your user (the `users` document).

| Task | Route |
| ---- | ----- |
| Create, list, read, change, delete | `POST /api/entities/`, `GET /api/entities/`, `GET`, `PATCH`, `DELETE /api/entities/{id}` |
| Refresh an endpoint's catalog | `POST /api/entities/models/endpoints/{id}/refresh` |
| Read an item's ban notifications | `GET /api/entities/{id}/notifications` |
| Read your notifications | `GET /api/users/notifications/` |
| Change your own model settings (the user's `settings`) | `PATCH /api/users/` |

Common errors: `401` when you are not an owner or manager of the item's groups, `404` for an item you can't
see, `409` when deleting an endpoint or credential that something still references, and `400` when a workflow
run can't get a model for every agent (the message lists the reason for each model; see
[How a run picks its models](#how-a-run-picks-its-models)).
