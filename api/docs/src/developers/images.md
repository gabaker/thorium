# Legacy Images and Pipelines

Before tools, Thorium ran **images** and chained them in **pipelines**. Thorium now runs
[tools](./tools.md) instead. This chapter covers what is left of images and how they convert, the
settings an Image tool still holds, and pipelines, which keep working with tools as their stages.

> **Images are read-only in this release.** Creating or updating an image returns `410 Gone`
> (`POST /api/images/` and `PATCH /api/images/{group}/{image}`), while images can still be listed,
> described, and deleted. Add new tools with `thorctl tools create`, and point pipeline stages at
> them with `thorctl pipelines stage-tool`.

### Old and new terms
---

| Before | Now | Notes |
| ------ | --- | ----- |
| Image | Image tool (kind `Image`) | The same settings, regrouped under `environment`, `execution`, and `data`, in a tool's versions. See [Configuring Images](./configuring_images.md) for each setting |
| Image tag or version | Tool instance (`<tool>@<version>`) | An instance is immutable; publishing a changed spec makes a new one |
| Pipeline stage names an image | Stage names a tool in the pipeline's `stage_tools` | Follows the tool's latest version, or pins one instance |
| `thorctl images edit`, `import`, and `bans create` | `thorctl tools create`, `publish`, `edit`, and `thorctl toolbox import` | Image writes are refused with `410` |
| Image bans and notifications | Model and pipeline bans; tool deprecation | Tools have no bans; deprecate a version to stop it being newly pinned (see [Bans](./bans.md)) |

### What still works on images
---

| Command | Status |
| ------- | ------ |
| `thorctl images get`, `describe` | Works: reads stored images |
| `thorctl images delete` | Works |
| `thorctl images notifications get`, `create`, `delete` | Works (see [Notifications](./notifications.md)) |
| `thorctl images edit`, `import`, `bans create`, `bans delete` | Refused with a message that points at tools, before an editor opens or a container is pushed |
| `thorctl images export` | Writes images to a directory; use `thorctl toolbox export` for tools |

The API routes that remain are `GET /api/images/{group}/`, `GET /api/images/{group}/details/`,
`GET /api/images/data/{group}/{image}`, `DELETE /api/images/{group}/{image}`, and the image
notification routes.

### How images become tools
---

- **On upgrade.** The upgrade to Agentic Workflows (revision `2026-10-v04`) converts each image to a
  tool with a first tool instance, pins each pipeline stage to it, and moves stored tool names to
  `legacy_tools`. The conversion cannot be undone except by restoring a backup, and it first waits
  for running reactions to finish (the drain). See [Upgrading to Agentic
  Workflows](../admins/agentic/upgrading.md) for admins.
- **By hand.** `thorctl tools snapshot <GROUP>/<IMAGE>` converts one stored image the same way the
  upgrade does, and `thorctl tools create --spec` creates an Image tool from a spec you write. See
  [Converting an image to a tool](./tools.md#converting-an-image-to-a-tool) and [Moving a pipeline to
  tools](./tools.md#moving-a-pipeline-to-tools).
- **From a toolbox.** `thorctl toolbox import` reads a legacy image toolbox by converting it to tools;
  `thorctl toolbox convert` writes the converted file. See [Converting a legacy
  toolbox](./toolbox.md#converting-a-legacy-toolbox).

### In this chapter
---

1. [Creating a Tool from Image Settings](./add_images.md): where an Image tool's settings come from.
2. [Configuring Images](./configuring_images.md): each setting, with its meaning and limits.
3. [More on Children (Samples)](./children.md): how a tool's children carry origin metadata.
4. [Building Pipelines](./build_pipelines.md): create, change, and run pipelines of tools.
5. [Importing, Exporting, and Editing Images and Pipelines](./import_export.md): the pipeline and
   image export formats, and `pipelines edit`.

Running a tool or pipeline has the developer role requirement of its scaler. If you don't have the
role, ask your Thorium admins.
