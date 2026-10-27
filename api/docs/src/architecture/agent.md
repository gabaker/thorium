# Agent
---

The Thorium agent facilitates the running of tools within a `reaction` by: 

- Downloading all required job data (samples, repos, etc.)
- Executing tool(s) during the job
- Streaming logs to the API
- Uploading results to the API
- Cleanup of some temporary job artifacts

This functionality allows Thorium to support arbitrary command line tools with limited to no customization of the
tool itself. The agent interacts with the API, abstracting all required knowledge of the Thorium system away from the
tool. As a result, any tool that can be run from a command line interface on bare-metal or in a containerized
environment can be integrated into Thorium with minimal developer effort.

Every job runs a tool instance: the agent is started with `--tool-instance <id>`, reads that tool
instance once, and claims jobs for it. In a workflow run it also launches the harness and offers the
`thorium-agent tool` CLI wrapper for running tools from inside the run. See
[Scheduling Tools and Runs](../admins/agentic/scheduling.md) and
[Harnesses](../developers/harnesses.md).

#### Agent arguments

The scaler and reactor start the agent as `thorium-agent <OPTIONS> <ENV>`, where `<ENV>` is `k8s`,
`bare-metal`, `windows`, or `kvm`. Admins don't normally run it by hand.

| Option | Meaning |
| ------ | ------- |
| `-c, --cluster` | The cluster this worker is in (required) |
| `--node` | The node this worker is on |
| `-g, --group` | The group to claim jobs from (required) |
| `--tool-instance` | The id of the tool instance to claim jobs for (required) |
| `-n, --name` | The name of this worker (required) |
| `-t, --trace` | The path of the tracing config (`/opt/thorium/tracing.yml` by default) |
| `-k, --keys` | The keys file to authenticate with (`keys.yml` by default); a worker launched without it builds its client from `THORIUM_URL` and `THORIUM_TOKEN` when both are set |
| `-l, --limbo` | How many seconds the agent waits without a job before exiting (5 by default) |

### FAQs
---

### How does the agent know what commands are required to run tools?

This depends on what type of scheduler this agent was spawned under:

| Scheduler | Method |
| --------- | ------ |
| K8s | The scaler inspects the Docker image in the registry |
| Windows | The scaler inspects the Docker image in the registry |
| Bare Metal | The tool instance's workload contains an entry point and command |
| External | Thorium does not spawn this and so it is left up to the spawner |

### Does the agent clean up after my tool runs?

The Thorium agent will cleanup certain artifacts after a reaction has completed. This includes any data that was
downloaded from the API at the start of a reaction and provided to the tool before it was executed. The
directory paths set in the Thorium image configuration for input files, repos, results, result files and children
files will all be cleaned up by the agent. If a tool uses directories outside of those set in the Thorium image 
configuration, the agent will not know to clean those up. Instead it is up to the tool to ensure those temporary 
file paths get cleaned up. For containerized tools, any file cleanup not handled by the Agent or tool itself will
automatically occur when the image is scaled down.