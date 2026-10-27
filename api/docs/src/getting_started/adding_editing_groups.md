# Creating/Editing Groups

All resources within Thorium including files, analysis pipelines, and tools are owned by a user and uploaded to a
group. Access to modify group resources is granted by your role within the group. If you want to learn more about
Thorium's roles and permissions system, you can read [this](./roles_permissions.md) page. You can create a group with
Thorctl; managing group membership uses the API (see [API](#api)).

## Thorctl
---
Create a group that you own with `thorctl groups create`:

```bash
thorctl groups create analysts -d "Malware analysts"
thorctl groups create reversers --owner alice --owner bob
```

The name takes lowercase letters, numbers, and dashes. `-d/--description` says what the group is for,
and each `--owner <USER>` makes another user a direct owner; you are always an owner, because the
server adds the caller. Use `thorctl groups get` and `thorctl groups describe <GROUP>` to see groups
you can access.

## Group Roles
---

You can add users to different group roles. As we described in the previous chapter, group roles
are how you define the abilities a group member has within the group. Roles and their abilities are defined in the table
below. Group resources can include tools, pipelines, repos, files, tags, comments, and tool results.

| Ability | Owners | Managers | Users | Monitors |
| ---- | ---- | ---- | ---- | ---- |
| View Resources | yes | yes | yes | yes |
| Run Pipelines | yes | yes | yes | no |
| Upload/Create Resources | yes | yes | yes| no |
| Modify/Delete Resources | all | all | self owned only | none |
| Group Membership | add/remove any member | add/remove non-owner members | no | no |
| Delete Group | yes | no | no | no |

A Thorium user can be added to a group either as a direct user or as part of a metagroup. This functionality allows you to use
an external group membership system (ie LDAP/IDM) to grant access to Thorium resources.

| type | description |
| ---- | ----------- |
| direct user | A single user in Thorium |
| metagroups | A group of users that is defined in LDAP |

By default, metagroup info is updated every 10 minutes or when a Thorium group is updated. This means that
when a user is added or removed from a metagroup it may take up to 10 minutes for that change to be visible in Thorium.

## API
---

Thorctl creates, lists, and describes groups; changing a group's membership has no Thorctl command yet and uses the API.

| Method | Route | Description |
|--------|-------|-------------|
| `POST` | `/api/groups/` | Create a group (JSON: `name`, `description`, and `owners`, `managers`, `users`, `monitors` each with `direct` users and `metagroups`) |
| `PATCH` | `/api/groups/<GROUP>` | Add or remove members (JSON: each role has `direct_add`, `direct_remove`, `metagroups_add`, `metagroups_remove`), change the description |
| `GET` | `/api/groups/<GROUP>/details` | One group's details |
| `DELETE` | `/api/groups/<GROUP>` | Delete a group (see [Delete a Group](../admins/delete_group.md)) |

```bash
curl -X PATCH -H "Authorization: Bearer $TOKEN_B64" -H 'Content-Type: application/json' \
  https://<URL>/api/groups/analysts -d '{"users": {"direct_add": ["alice"]}}'
```

Owners and managers can change membership (managers can't change owners).
