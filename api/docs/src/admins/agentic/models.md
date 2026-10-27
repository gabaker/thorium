# Model Endpoints for Admins

Group owners and managers register the model providers their groups call (model endpoints and
credentials), and users can add their own; see [Models](../../users/models.md). Admins decide which
provider endpoints a deployment may call at all.

### The allowed endpoint list
---

The allowed model endpoint list is a system setting, `models.allowed_endpoints`. Each entry names a
provider and optionally an exact base url:

| Field | Meaning |
| ----- | ------- |
| `provider` | One of `OpenAi`, `Anthropic`, `Bedrock`, `Vertex`, `Gemini`, `AzureOpenAi`, `LiteLlm`, or `OpenAiCompatible` |
| `base_url` | The exact url allowed. Unset allows only the provider's default public url (for `Bedrock` and `Vertex`, the regional default urls) |

Urls are compared exactly apart from one trailing `/`. **An empty list allows nothing**, so no
model endpoint can be created until an admin allows one. Creating an endpoint outside the list, or
changing an endpoint's provider to one outside it, is refused. The consistency scan that runs with a
settings update bans group and user endpoints that are no longer on the list
(`NotAllowedByDeployment`) and clears that ban once they are allowed again; runs never resolve a
model on a banned endpoint.

Removing an entry takes effect at once for runs that already exist, even before a scan bans the
endpoint. A run's harness gets its models when its pod is spawned, and the scaler then refuses an
endpoint that is no longer on the list or carries a `NotAllowedByDeployment` ban. A run still
waiting for its harness pod is failed with the stage logs `run models can't be delivered` and the
reason, which names the endpoint. A harness that is already running keeps the models it was given.
Allowing the endpoint again doesn't restart a failed run; its owner retries it, and the retry picks
models again from what the deployment allows then.

`thoradm settings` has no option for this list yet. Update it with the API's system settings route,
using an admin's token:

```bash
curl -X PATCH "$THORIUM_URL/api/system/settings" \
  -H "Authorization: Token $(echo -n "$TOKEN" | base64)" -H "Content-Type: application/json" \
  -d '{"models": {"add_endpoints": [
        {"provider": "Anthropic", "base_url": null},
        {"provider": "LiteLlm", "base_url": "https://litellm.example.com/v1"}
      ]}}'
```

The update also takes `remove_endpoints` (a list of the same entries) and `clear_endpoints: true`.
`thoradm settings get` and `GET /api/system/settings` print the current list.

### Bans
---

Every model endpoint, catalog model, profile, and credential can carry bans. Thorium adds and
clears every kind but `Generic` itself, during catalog refreshes, settings scans, and updates:

| Ban | Meaning |
| --- | ------- |
| `Generic` | A ban set by hand with a message; never cleared automatically |
| `ModelRemoved` | The provider's model listing no longer includes this catalog model (models are banned rather than deleted, and unbanned when they return) |
| `CredentialRejected` | The provider rejected the credential (rotating the credential clears it) |
| `CredentialExpired` | The credential's `expires` time has passed, or the provider reported it expired |
| `EndpointUnreachable` | The endpoint could not be reached |
| `BrokenReference` | A referenced entity (an endpoint's credential or a profile's endpoint) is missing or unusable |
| `NotAllowedByDeployment` | The endpoint is not on the allowed endpoint list |

Owners and managers of any of an item's groups add and clear `Generic` bans with
`thorctl models bans add <ENTITY> --message <MESSAGE>` (`--model <MODEL>` bans one catalog model of
an endpoint) and `thorctl models bans clear <ENTITY> <BAN>`. Only admins add bans of the other
kinds. Every model ban writes a notification for the people it affects, and clearing it deletes the
notification.

A run resolves its models when it is created and skips banned items. Bans added later don't change
the models a run was given, with two exceptions that are checked again when the run's harness pod is
spawned: a ban on a credential the run uses, and an endpoint the deployment no longer allows (a
`NotAllowedByDeployment` ban, or an endpoint no longer on the allowed list). Either one makes the
scaler fail the run once with a stage log naming the reason instead of spawning its harness (see
[Scheduling Tools and Runs](./scheduling.md)). Other bans on the run's endpoint, catalog model, or
profile don't stop it. A harness that is already running keeps its models.

### Admin-only model routes
---

These routes return secrets and refuse any caller that is not an admin using their full token:

| Route | Returns |
| ----- | ------- |
| `GET /api/entities/models/credentials/{id}/secret` | A group credential with its secret |
| `GET /api/users/user/{username}/model-credentials/{endpoint}` | A user's credential for an endpoint |
| `GET /api/reactions/{group}/{id}/run-models` | A run's resolved models with their credentials' secrets; `404` when the run has no live run token scoped to that run, its creator can't edit in the run group, or it has no model bindings, `409` when a bound credential or endpoint was deleted, a bound credential is banned, or a bound endpoint is banned `NotAllowedByDeployment` or no longer on the allowed endpoint list |
| `GET /api/reactions/{group}/{id}/run-token` | A run's run-scoped token; `404` when the run has no live token, the token stored under the run's token name isn't scoped to that run, or the run's creator can't edit in the run group |

The event handler uses the first two to refresh catalogs, and the scaler uses the last two to start
a run's pods.

### Where secrets are kept
---

Model credential secrets are stored in Thorium's database and are never returned by the API to
anyone but admins through the routes above: every other response clears them and keeps a
fingerprint (the first 12 hex characters of the secret's SHA-256). They are copied in plaintext:

- into `thoradm` backups (see [Admin Command Line Tool](../thoradm/thoradm.md))
- into the env of a run's harness pod on K8s, where anyone who can read pod specs in the run's
  namespace can see them, together with the run's token. Thorium deployments must not grant
  non-admin users pod read (`get`, `list`, or `watch`) in group namespaces, because on K8s pod read
  is equivalent to reading these credentials; see
  [Protecting run secrets on K8s](./scheduling.md#protecting-run-secrets-on-k8s) for who else can
  read pod specs and how to limit it
