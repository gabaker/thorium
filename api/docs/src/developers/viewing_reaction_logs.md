# Viewing Reaction Status and Logs

The logs of a reaction are saved by the Thorium Agent and uploaded to the API. They hold the Agent's own
messages (when each stage starts and ends, what it downloaded, what it uploaded) and everything the stage's tool
wrote to stdout and stderr, so they are the first place to look when a run fails. This page covers reading them
with Thorctl; the status values are in [Reaction Status](./reaction_status.md).

### Reading logs
---

```bash
# print the logs of one reaction (the reaction's group is found for you, or add :<GROUP>)
thorctl reactions logs <REACTION-ID>
# write them to a directory, one subdirectory per pipeline, reaction, and stage
thorctl reactions logs <REACTION-ID> --output ./logs
# logs of every reaction of a pipeline or with a tag (the two filters are combined with OR)
thorctl reactions logs --pipelines static --groups analysts --output ./logs
thorctl reactions logs --tags triage-2026-10 --limit 20
# tool and workflow runs, optionally one stage only
thorctl runs logs <REACTION-ID>
thorctl runs logs <REACTION-ID> --stage triage
```

`reactions logs` prints at most `--log-limit` lines per stage (250000 by default). `--watch` re-reads the
logs until you interrupt it. The log of a stage is the combined output of the Agent and the tool: the Agent
downloads the repos and files the stage needs, then the tool's stderr and stdout appear, then the Agent
uploads any children, tags, and results and cleans up. The tool's output is also the place to validate the
command line that was passed to it.

The run commands print logs when you ask: `thorctl tools run`, `workflows run`, and `pipelines run` take
`--logs` to wait and then print the status updates and stage logs. A workflow run's harness output is in its
stage logs too (see [Running Tools and Workflows](../users/runs.md#results-of-a-run)).

### When a reaction doesn't finish
---

| Symptom | What to check |
| ------- | ------------- |
| The reaction stays `Created` | The tool's resources may be more than any node can offer, its registry path may be invalid, or the tool or pipeline may be banned. Check `thorctl pipelines describe` and `thorctl tools describe` for bans, and ask an admin to look at the Scaler logs if nothing is wrong (see [Jobs Stuck At Created](../admins/common_issues/jobs_stuck_at_created.md)) |
| A stage fails | Read that stage's logs. For a pipeline to succeed every stage must exit with code 0 |
| A run is refused when created | The error names the cause: a missing tool on a stage, a ban, or a group you can't run in. See [Running Tools and Workflows](../users/runs.md#common-errors) |

### Using the API
---

| Task | Route |
| ---- | ----- |
| Read all logs of a reaction | `GET /api/reactions/logs/{group}/{id}` |
| Read one stage's logs | `GET /api/reactions/logs/{group}/{id}/{stage}` |
