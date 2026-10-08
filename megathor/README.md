# megathor

Ansible playbooks that deploy Thorium onto an existing Kubernetes cluster on bare-metal servers or
VMs: they prepare the cluster (Rook/Ceph storage, Traefik ingress) and install the Thorium Helm
charts (`deploy/charts`) with values generated from the inventory (`inventory/group_vars`).

The full guide (inventory, vault, variables, S3 backends, external services, offline installs,
upgrades, conversion of pre-Helm deployments, and removal) is the page "Production Clusters
(megathor)" in the Thorium docs ([api/docs/src/admins/deploy/megathor.md](../api/docs/src/admins/deploy/megathor.md)).

## Prerequisites

A controller with python3, pip, `kubectl`, Helm 3.8+, and a kube config for the cluster:

```bash
python3 -m pip install -r requirements.txt
ansible-galaxy collection install kubernetes.core amazon.aws community.general --upgrade
```

## Quick start

`inventory/local.ini` puts `localhost` in `[single_node]`, which applies the single-node defaults
in `inventory/group_vars/single_node.yml`; remove it from that group for a multi-node cluster.
Replace the `<...>` placeholders in `inventory/group_vars`, create the vault password file, and
run the playbook:

```bash
openssl rand -base64 32 > artifacts/vault_pass && chmod 600 artifacts/vault_pass
ansible-playbook -i inventory/local.ini deploy.yml --vault-password-file artifacts/vault_pass -v
```

## Upgrades

Set `thorium_upgrade_target_revision` (and `thorium_upgrade_approvals` for steps that change data)
when the deployment waits in `UpgradeRequired`; see "Upgrading Thorium" in the Thorium docs
([api/docs/src/admins/deploy/upgrades.md](../api/docs/src/admins/deploy/upgrades.md)). A deployment
made by megathor before the Helm charts must be converted first; see "Converting a Pre-Helm
Deployment" ([api/docs/src/admins/deploy/convert-to-helm.md](../api/docs/src/admins/deploy/convert-to-helm.md)).

## Checking template changes

`scripts/render-values.py` renders a values template (`roles/thorium/templates/values.yaml.j2`,
`roles/infra_operators/templates/values.yaml.j2`) the way Ansible would (python3 with Jinja2 and
PyYAML, no Ansible or cluster needed), so the generated chart values can be checked with
`helm lint`/`helm template`. The Deployment Tools workflow (`.github/workflows/deploy-tools.yml`)
runs it for the defaults and each `group_vars` variant; run its render steps locally to reproduce
it.
