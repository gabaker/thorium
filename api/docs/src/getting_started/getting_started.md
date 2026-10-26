# Getting Started

<p align="center">
    <img src="./../static_resources/rocket.png"/>
</p>

This guide covers how to start using Thorium to analyze data at scale.

Thorium is driven with Thorctl (pronounced Thor-cuddle), our command line tool, and with its REST API for
scripts and integrations. Thorctl scales from one file to thousands: it uploads data, runs tools, workflows,
and pipelines, and downloads the results, and it works in a "headless" analysis environment where only a command
line is available. The guides in this book use Thorctl for their examples and name the API route behind each
command. The Web UI covers most of the same tasks; [Concepts](../concepts/concepts.md#the-web-ui) lists
its pages.

### First steps
---

1. [Register](./registration.md) for an account (or ask your admins to create one), then
   [install Thorctl and log in](./login.md).
2. Learn how [roles and permissions](./roles_permissions.md) and [groups](./adding_editing_groups.md) control
   what you can see and do.
3. Upload something and run something on it. Group, file, and tool names here are examples:

```bash
# create a group, upload a file to it, and note the SHA256 it prints
thorctl groups create analysts -d "Malware analysts"
thorctl files upload -g analysts ./sample.exe
# run a tool on the file and wait for it
thorctl run tool strings --sample <SHA256> --group analysts --wait
# download what it produced
thorctl results get --files <SHA256> --tools strings
```

4. Read [Concepts](../concepts/concepts.md) for how tools, workflows, runs, and models fit together, then
   [Running Tools and Workflows](../users/runs.md).

If you have already registered for a Thorium account you can skip to the [Login](./login.md) section of this
chapter.
