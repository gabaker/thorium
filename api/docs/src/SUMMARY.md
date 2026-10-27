# Summary

- [Intro](./intro.md)
# Getting Started

- [Getting Started](./getting_started/getting_started.md)
    - [Registration](./getting_started/registration.md)
    - [Login](./getting_started/login.md)
    - [Roles and Permissions](./getting_started/roles_permissions.md)
    - [Adding/Editing Groups](./getting_started/adding_editing_groups.md)
    - [Command Line Tool](./getting_started/thorctl.md)
- [Concepts](./concepts/concepts.md)

# Tools, Toolsets, and Agents

- [Tool Developers](./developers/developers.md)
    - [Tools and Tool Instances](./developers/tools.md)
    - [AI Agents, Toolsets, and Workflows](./developers/agents_workflows.md)
    - [Harnesses](./developers/harnesses.md)
    - [Editing Resources](./developers/editing_resources.md)
    - [Generators](./developers/generators.md)

# Running

- [Running Tools and Workflows](./users/runs.md)
    - [Spawning Reactions](./users/spawning_reactions.md)
    - [Reaction Status](./developers/reaction_status.md)
    - [Viewing Reaction Status and Logs](./developers/viewing_reaction_logs.md)

# Models and Bans

- [Models](./users/models.md)
- [Bans](./developers/bans.md)
- [Notifications](./developers/notifications.md)

# Results and Data

- [Users](./users/users.md)
    - [Results and Provenance](./users/viewing_results.md)
    - [Uploading Files](./users/uploading_files.md)
    - [Viewing Files](./users/viewing_files.md)
    - [Tagging Files](./users/tagging_files.md)
    - [Associating Data](./users/associations.md)
    - [Search](./users/search.md)
    - [Downloading Files](./users/downloading.md)
    - [Commenting on Files](./users/commenting.md)
    - [Revoking Your Token](./users/revoking_token.md)
    - [More on Children (Samples)](./developers/children.md)

# Legacy Images and Pipelines

- [Legacy Images and Pipelines](./developers/images.md)
    - [Creating a Tool from Image Settings](./developers/add_images.md)
    - [Configuring Images](./developers/configuring_images.md)
    - [Building Pipelines](./developers/build_pipelines.md)
    - [Importing, Exporting, and Editing Images and Pipelines](./developers/import_export.md)

# Toolboxes

- [Toolboxes](./developers/toolbox.md)
    - [Toolbox Directories and Image Builds](./developers/toolbox_directories.md)

# Upgrading

- [Upgrading: Changes to Tool Commands](./developers/tool_cli_changes.md)
- [Upgrading to Agentic Workflows](./admins/agentic/upgrading.md)
- [Release Notes](./release_notes/release_notes.md)
    - [Agentic Workflow Support](./release_notes/agentic_workflows.md)

# Administration

- [Admins](./admins/admins.md)
    - [Architecture](./architecture/architecture.md)
        - [API](./architecture/api.md)
        - [Scaler](./architecture/scaler.md)
        - [Agent](./architecture/agent.md)
        - [Reactor](./architecture/reactor.md)
        - [Tracing/Logging](./architecture/tracing.md)
        - [Event Handler](./architecture/event-handler.md)
        - [Operator](./architecture/operator.md)
        - [Thorctl Edit Framework](./architecture/thorctl-edit.md)
        - [Testing Upgrades and Schema Changes](./architecture/testing-upgrades.md)
    - [Deploying Thorium](./admins/deploy/deploy.md)
        - [Deployment Concepts](./admins/deploy/concepts.md)
        - [Planning a Deployment](./admins/deploy/planning.md)
        - [Install with Helm](./admins/deploy/deploy-helm.md)
        - [Configure the Helm Charts](./admins/deploy/helm-configuration.md)
        - [Operating a Deployment](./admins/deploy/operate.md)
        - [Upgrading Thorium](./admins/deploy/upgrades.md)
        - [Converting a Pre-Helm Deployment](./admins/deploy/convert-to-helm.md)
        - [Deploy the Operator Without Helm](./admins/deploy/deploy-thorium.md)
        - [Development Clusters (minithor)](./admins/deploy/minithor.md)
        - [Production Clusters (megathor)](./admins/deploy/megathor.md)
        - [Bring Your Own Infrastructure](./admins/deploy/infrastructure.md)
            - [Traefik](./admins/deploy/install-traefik.md)
            - [Rook](./admins/deploy/install-rook.md)
            - [Redis](./admins/deploy/install-redis.md)
            - [Scylla](./admins/deploy/install-scylla.md)
            - [Elasticsearch (ECK)](./admins/deploy/install-elastic.md)
            - [Tracing (Quickwit and Jaeger)](./admins/deploy/install-tracing.md)
        - [ThoriumCluster and Operator Reference](./admins/deploy/thoriumcluster.md)
        - [Chart Values Reference](./admins/deploy/chart-values.md)
            - [Example Values Files](./admins/deploy/example-values.md)
                - [Production](./admins/deploy/example-production.md)
                - [Minimal Site](./admins/deploy/example-site.md)
                - [External Services](./admins/deploy/example-external.md)
                - [Proxy](./admins/deploy/example-proxy.md)
                - [Offline Mirror](./admins/deploy/example-offline.md)
        - [Troubleshooting Deployments](./admins/deploy/troubleshooting.md)
    - [Operations](./admins/operations.md)
        - [Troubleshoot User Issues](./admins/troubleshoot_user_issues.md)
        - [Delete A Group](./admins/delete_group.md)
        - [Delete A User](./admins/delete_user.md)
        - [Ban Things in Thorium](./admins/bans_admins.md)
        - [Create Notifications](./admins/notifications_admins.md)
        - [Setting Up KVM VMs for Reactor](./admins/reactor_kvm_setup.md)
    - [Tools, Runs, and Models](./admins/agentic/agentic.md)
        - [The Thorium MCP Server](./admins/agentic/mcp.md)
        - [Model Endpoints for Admins](./admins/agentic/models.md)
        - [Scheduling Tools and Runs](./admins/agentic/scheduling.md)
    - [Admin Command Line Tool](./admins/thoradm/thoradm.md)
        - [Taking Backups](./admins/thoradm/taking_backups.md)
    - [Common Issues](./admins/common_issues.md)
        - [Jobs Stuck At Created](./admins/common_issues/jobs_stuck_at_created.md)
    - [Network Policies](./admins/network_policies/network_policies.md)
        - [Configuring Network Policies](./admins/network_policies/configure_network_policies.md)
- [Getting Help](help/help.md)
    - [FAQ](./help/faq.md)
    - [Contact Us](./help/contact_us.md)
