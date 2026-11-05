# Registering the ThorPal Harness

`thorpal-harness` runs a workflow run's entry agent with ThorPal. It is a minimal harness: it
supports one model API and credential kind, calls only tools reached through Thorium's API, runs
only the entry agent, and analyzes one sample. This page lists the `thorctl` commands that register
it, a model for it, and the tools its agents call.

What it supports:

| Area | Supported |
| ---- | --------- |
| Model | `AnthropicMessages` on an `Anthropic` endpoint, with an `ApiKey` credential |
| Tools | `ThoriumMcp` and `Container` tools, bound to the `ThoriumApi` mechanism |
| Agents | The entry agent only: a run whose entry agent delegates to sub-agents fails at start |
| Inputs | The run's first sample; a run without a sample fails at start |

It also refuses, before its first model call, a run with a tool that requires approval and a tool
bound to any other mechanism. A refused run exits with `2` and writes the reason to `status.json`
and the stage logs. A run that fails after it starts (a model call that fails, for example) exits
with `1`, and a run still going after `THORPAL_HARNESS_TIMEOUT_SECS` (600 in the image) exits with
`3`. A run that finishes writes its answer, `conversation.jsonl`, `traces.jsonl`, and `status.json`
and exits with `0`. The harness reads its model, endpoint, and key only from the run's models file,
never from env vars.

### Building the image
---

The image holds the `thorpal-harness` binary built from this repo. `thorpal/toolbox/build-harness-image.sh`
builds the binary, stages it with `thorpal/toolbox/harness/Dockerfile`, builds
`<REGISTRY>/thorpal-harness:<TAG>`, and prints the image reference (`--dry-run` prints the commands
instead, and `--help` lists every option):

```bash
REGISTRY=<REGISTRY> thorpal/toolbox/build-harness-image.sh --tag <TAG> --push
```

By hand, the same build is:

```bash
cargo build --release -p thorpal --bin thorpal-harness
mkdir thorpal-image
cp target/release/thorpal-harness thorpal/toolbox/harness/Dockerfile thorpal-image/
docker build -t <REGISTRY>/thorpal-harness:<TAG> thorpal-image
docker push <REGISTRY>/thorpal-harness:<TAG>
```

`--from-source` builds without a Rust toolchain on the host instead:
`thorpal/toolbox/harness/Dockerfile.source` compiles the binary in the image build from this repo's
committed `HEAD`, with the git dependencies at the commits `Cargo.lock` pins.

The image builds from Ubuntu 24.04 (pinned by digest) by default, installs `ca-certificates` and
the OpenSSL 3 libraries, and runs as an unprivileged user; pass `--base-image <image>` to the script
(`--build-arg IMAGE=<image>` to `docker build`) to use another base. The binary links glibc and OpenSSL 3 (`libssl.so.3`, `libcrypto.so.3`)
dynamically and checks TLS against the system CA bundle, so the base needs apt and a glibc at least
as new as the one it was built against (2.39 for a build on Ubuntu 24.04, so `ubuntu:24.04` or
Debian 13 work and Debian 12 does not).

The binary prints the capabilities its Harness tool declares:

```bash
$ thorpal-harness --capabilities
{
  "mechanisms": [
    "ThoriumApi"
  ],
  "file_io": true,
  "model_apis": [
    "AnthropicMessages"
  ],
  "credential_kinds": [
    "ApiKey"
  ],
  "per_agent_models": false
}
```

### Letting the harness reach its model
---

Harness pods may only reach the Thorium API and DNS by default. An admin allows the model endpoint's url
and creates a network policy that lets the harness's pods reach it (this example uses a model
proxy at `http://10.0.0.5:18300`):

```bash
curl -X PATCH "$THORIUM_URL/api/system/settings" \
  -H "Authorization: Token $(echo -n "$TOKEN" | base64)" -H "Content-Type: application/json" \
  -d '{"models": {"add_endpoints": [{"provider": "Anthropic", "base_url": "http://10.0.0.5:18300"}]}}'
```

```yaml
# model-egress.yaml
egress:
  - allowed_ips:
      - cidr: 10.0.0.5/32
    ports:
      - port: 18300
        protocol: TCP
```

```bash
thorctl network-policies create --name thorpal-model-egress --groups <GROUP> --rules-file model-egress.yaml
```

### Proxies and custom CA certificates
---

The harness's model client and its Thorium client both take their proxy and CA settings from the
standard env vars, which the Harness tool sets in `environment.K8s.env`:

| Env var | Effect |
| ------- | ------ |
| `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` | The proxy for https urls, for http urls, and for either when its own is unset. The upper-case name wins when both cases are set, and an empty value means no proxy |
| `NO_PROXY` | The hosts reached without the proxy. List the Thorium API host (the scaler adds it unless the admin turned off `agent_auto_bypass_proxy_for_api`), or tool calls go through the proxy too |
| `SSL_CERT_FILE` | A PEM bundle that **replaces** the system CA certificates, so it must hold the public roots the model needs as well as your own |
| `SSL_CERT_DIR` | Directories of CA certificates (`:` separated) that replace the system's |

With a proxy, the network policy must allow the proxy's address instead of the model's. A custom
CA bundle is usually mounted from a ConfigMap in the group's namespace:

```bash
cat /etc/ssl/certs/ca-certificates.crt corp-root.pem > bundle.pem
kubectl -n <GROUP_NAMESPACE> create configmap thorpal-ca --from-file=ca.pem=bundle.pem
```

```yaml
# added to thorpal.yaml under Harness.workload.environment.K8s
env:
  HTTPS_PROXY: http://proxy.example.com:3128
  NO_PROXY: thorium-api.thorium.svc.cluster.local.,.svc,.cluster.local
  SSL_CERT_FILE: /etc/thorpal-ca/ca.pem
volumes:
  - name: thorpal-ca
    archetype: ConfigMap
    mount_path: /etc/thorpal-ca
    read_only: true
    config_map: {optional: false}
```

At start the harness logs the settings in effect to the stage logs, with any user and password
removed from the proxy urls:

```text
thorpal-harness: network: calls use proxy HTTPS_PROXY=http://proxy.example.com:3128 (NO_PROXY=thorium-api.thorium.svc.cluster.local.,.svc,.cluster.local); CA certificates from SSL_CERT_FILE=/etc/thorpal-ca/ca.pem
```

Without any of them it logs `no proxy is set, so calls go direct` and `CA certificates from the
system store`. An `SSL_CERT_FILE` that is set but empty still replaces the system store, and is
logged as `SSL_CERT_FILE=""`. A listed `SSL_CERT_DIR` directory that doesn't exist is logged as a
warning. A run whose CA settings load no certificates (an `SSL_CERT_FILE` that can't be read or
holds none, and no `SSL_CERT_DIR` directory that holds any) is refused with exit code `2` before its
first model call, and the reason is written to `status.json`.

### The Harness tool
---

```yaml
# thorpal.yaml
Harness:
  workload:
    environment:
      K8s:
        image: <REGISTRY>/thorpal-harness:<TAG>
        scheduling:
          resources: {cpu: 500, memory: 512}
          spawn_limit: Unlimited
        network_policies: [thorpal-model-egress]
    execution:
      args:
        entrypoint: [/usr/local/bin/thorpal-harness]
      timeout: 900
    data:
      dependencies:
        samples: {strategy: Disabled}
        ephemeral: {strategy: Disabled}
      display_type: String
  capabilities:
    mechanisms: [ThoriumApi]
    file_io: true
    model_apis: [AnthropicMessages]
    credential_kinds: [ApiKey]
    per_agent_models: false
  orphaned_runs: Delete
```

The capabilities are the ones `thorpal-harness --capabilities` prints. The harness reads the run's
files and samples from the standard paths and refuses arguments it doesn't know, so the spec turns
off passing sample and ephemeral file paths as arguments; without it every run fails at start
with `unknown argument /tmp/thorium/ephemeral/run.json`. The harness writes its answer as plain
text, so `display_type: String` stores it as text; without it the answer is stored as JSON, the
default. The network policy must exist in the tool's groups before the tool is created.

```bash
thorctl tools create --spec thorpal.yaml --name thorpal --group <GROUP> --version 0.1.0
```

### Tools for its agents
---

A `Container` tool runs an Image tool instance as a sub run of the workflow run:

```yaml
# scan.yaml
Agentic:
  function_name: scan
  title: Scan
  model_description: Run the scanner on files in Thorium and return its summary
  input_schema:
    type: object
    properties:
      samples: {type: array, items: {type: string}}
      label: {type: string}
  backing:
    Container:
      tool_instance: <IMAGE_TOOL_INSTANCE_ID>
      inputs:
        args:
          label: {Kwarg: "--label"}
        samples: samples
      outputs:
        max_result_bytes: 65536
  timeout: 300
```

A `ThoriumMcp` tool calls one of the Thorium MCP server's tools by name, with the run's token. This
one wraps `get_sample`:

```yaml
# sample-info.yaml
Agentic:
  function_name: sample_info
  title: Sample info
  model_description: Get basic info about a file in Thorium by its SHA256
  input_schema:
    type: object
    properties:
      sha256: {type: string}
    required: [sha256]
  backing:
    ThoriumMcp:
      tool: get_sample
  timeout: 60
```

```bash
thorctl tools create --spec scan.yaml --name scan --group <GROUP> --version 1.0.0
thorctl tools create --spec sample-info.yaml --name sample-info --group <GROUP> --version 1.0.0
```

### A model to run it with
---

An owner or manager of the group adds a group credential, an `Anthropic` endpoint that uses it,
and a profile for the model (the credential's secret is read from the file and never printed):

```yaml
# credential.yaml
kind: ApiKey
```

```yaml
# endpoint.yaml
provider:
  Anthropic:
    base_url: http://10.0.0.5:18300
credential: <CREDENTIAL_ID>
models:
  - model_id: claude-haiku-4-5-20251001
    capabilities: {tool_calling: true}
```

```yaml
# profile.yaml
endpoint: <ENDPOINT_ID>
model_id: claude-haiku-4-5-20251001
api: AnthropicMessages
parameters: {max_output_tokens: 1024}
```

```bash
thorctl models credentials create --spec credential.yaml --name model-key --group <GROUP> --secret-file <KEY_FILE>
thorctl models endpoints create --spec endpoint.yaml --name model-endpoint --group <GROUP>
thorctl models profiles create --spec profile.yaml --name haiku --group <GROUP>
```

A user can instead set their own credential for the group's endpoint:

```bash
thorctl models settings set-credential model-endpoint --spec credential.yaml --secret-file <KEY_FILE>
```

### An agent and a workflow
---

```yaml
# analyst.yaml
system_prompt: You analyze files in Thorium with the tools you are given.
prompt: Scan the input file with scan, look it up with sample_info, then summarize both results.
models:
  - model_id: claude-haiku-4-5-20251001
max_turns: 10
```

```bash
thorctl agents create --config analyst.yaml --name analyst --group <GROUP>
thorctl agents link-tool analyst scan
thorctl agents link-tool analyst sample-info
thorctl agents set-harness analyst thorpal
thorctl workflows create analyze --group <GROUP> --agent analyst --entry analyst
thorctl instances get workflow analyze --wait-for instance-revision=1
```

Every agent of a run must resolve to the same model (`per_agent_models` is `false`); a run whose
agents resolve to different models is refused with `400` when it is created.

### Running it
---

```bash
thorctl workflows run 'analyze#r1' -g <GROUP> --sample <SHA256>
thorctl runs describe <REACTION> -g <GROUP>
thorctl results show <REACTION>
```

The answer is stored as a result on the sample under the workflow instance's key (`analyze#r1`),
with `conversation.jsonl`, `traces.jsonl`, and `status.json` as its result files. Each `Container`
tool call is a sub run of the workflow run, and the harness's output is in the run's stage logs.
