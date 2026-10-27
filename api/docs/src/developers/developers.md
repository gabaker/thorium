# Developers

Thorium developers have all the abilities of someone with the `User` system role, but have the added ability to create
and modify analysis tools and build pipelines and workflows from them. Just like a Thorium user, developers
can:

  - upload files and Git repositories
  - add and remove metadata tags on uploaded files and repositories
  - run a pipeline on a file or repository (called a reaction)
  - run tools and workflows directly (see [Running Tools and Workflows](../users/runs.md))
  - view reaction status and logs
  - view tool results
  - comment on files and upload comment attachments
  - create new groups

Additionally, developers can:

  - Create, modify, and delete pipelines (see [Building Pipelines](./build_pipelines.md)).
  - Create and publish [tools](./tools.md), and build [AI agents, toolsets, and workflows](./agents_workflows.md)
    that run in [harnesses](./harnesses.md). Creating these needs edit rights in the group rather
    than the developer role, but running a tool's workload needs the developer role for its scaler.

Images can no longer be created or modified; Image tools replace them (see [Legacy Images and
Pipelines](./images.md)).

A developer must have adequate group permissions (via their group role) to create, modify or delete a tool or
pipeline within a group. They must be an `Owner`, `Manager` or `User` within the group to create resources in that group.
The `Monitor` role grants view-only permissions and does not allow the group member to create, modify or delete group
resources.

### Where to read next
---

1. [Tools and Tool Instances](./tools.md): create, publish, inspect, and delete tools.
2. [AI Agents, Toolsets, and Workflows](./agents_workflows.md) and [Harnesses](./harnesses.md): build workflows.
3. [Editing Resources](./editing_resources.md): change any of them in your editor or from a script.
4. [Running Tools and Workflows](../users/runs.md): run what you built.
5. [Toolboxes](./toolbox.md): keep tools in git and move them between instances.
