# Reaction Status

A reaction is one run of a pipeline, a tool, or a workflow. Its status tells you how far the analysis
has gotten. Check it with Thorctl while a run is going, or from a script with the API.

### Statuses
---

After a reaction has been submitted, its initial status is `Created`. Reactions that have been scheduled by the Thorium
Scaler and picked up by an Agent process enter the `Started` state. They run until either the tool completes
successfully, returns an error code, or is terminated by Thorium for exceeding its runtime specification (resource
limits or max runtime). All failure states cause the reaction to enter the `Failed` state. Successful runs of every
tool in the pipeline cause the reaction to be marked as `Completed`.

| Status | Definition |
| ---- | ---- |
| Created | The reaction has been created but is not yet running. |
| Started | At least one stage of the reaction has started. |
| Completed | This reaction has completed successfully. |
| Failed | The reaction has failed due to an error. |

### Checking status with Thorctl
---

```bash
# list a pipeline's reactions, or only the ones in one status, and keep the list up to date
thorctl reactions get -g analysts -p static
thorctl reactions get -g analysts -p static --status started --watch
# describe one reaction, and wait until it reaches a status (exit code 3 if --timeout passes first)
thorctl reactions describe <REACTION> --format human
thorctl reactions describe <REACTION> --wait-for status=Completed --timeout 30m
# the same for tool and workflow runs
thorctl runs describe <REACTION> --wait-for status=Completed
```

`thorctl reactions create --watch` (or `-W`) follows the reactions it creates, and the run commands'
`--wait` and `--watch` flags follow a run (see [Running Tools and Workflows](../users/runs.md)).
Reactions that are stuck in `Created` are usually waiting for resources or have a banned tool; see
[Viewing Reaction Status and Logs](./viewing_reaction_logs.md) and [Bans](./bans.md).

### Reaction lifetimes
---

Once a reaction has reached its terminal state (`Completed` or `Failed`), the reaction status and logs see no
future updates. Thorium applies a lifespan of 2 weeks to reactions that have reached a terminal state. After this
lifespan has been reached, Thorium cleans up info about the expired reaction. This cleanup does not delete tool
results and only affects reaction metadata such as the reaction's status and logs. This helps to prevent infinite
growth of Thorium's high consistency in-memory database, Redis. Because of this cleanup, `thorctl reactions get`
may no longer list a reaction even though its tool results are still visible with `thorctl results get`.

### Using the API
---

| Task | Route |
| ---- | ----- |
| Read a reaction and its status | `GET /api/reactions/{group}/{id}` |
| List a pipeline's reactions in one status | `GET /api/reactions/status/{group}/{pipeline}/{status}/` |
| List the reactions of a group in one status | `GET /api/reactions/group/{group}/{status}/` |
| List the sub reactions of a reaction | `GET /api/reactions/sub/{group}/{reaction}/` |
