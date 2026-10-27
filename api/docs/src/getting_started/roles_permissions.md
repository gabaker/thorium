# Roles, Permissions, and Ownership

Thorium uses groups to grant access to resources and role-based permissions to limit the ability of individuals to
conduct certain operations on those resources.

## Resource Ownership and Access
---

All resources within Thorium including files and analysis pipelines are owned by the person that created them,
but uploaded to a group. Only group members and those with the `Admin` system role can access a group's resources or
even know that a particular resource like a file has been uploaded. This explicit groups-based access model helps to
prevent information leakage and supports multitenancy of the Thorium system. Different groups can use the same Thorium
instance without risking sensitive data being leaked across groups. In order for a user to have access to resources,
such as files, the user can be added to that resource's group or the resource can be reuploaded to one of the user's
existing groups.

## Roles
---

Roles within Thorium are scoped at two different levels: `System` and `Group`. The capabilities granted by `Group` roles
apply to resources within a specific group while `System` roles apply globally.

### System Roles

`System` roles primarily exist to limit individuals from conducting certain sensitive actions at a global level. Since
anyone with a Thorium account can create their own groups, there is no practical way to limit certain actions using
only `Group` roles. 

A Thorium account will only have one `System` role at a time: `User`, `Analyst`, `Developer`, `Admin`, or `Disabled`. When you first register
for an account, you are granted the `User` system role by default. This will allow you to conduct analysis within
Thorium, but does not allow you to create new analysis pipelines or give you any privileged access to data outside of
your groups. If your interactions with Thorium require you to add or modify existing pipelines or to publish tools (see
[Tools and Tool Instances](../developers/tools.md); images can no longer be created or edited), you will need a
Thorium `Admin` to give you the `Developer` role. The `Developer` role is considered a
privileged role because it effectively allows an account holder to execute arbitrary binaries/commands within
Thorium's sandboxed analysis environments.

The Thorium `Admin` role grants access to view and modify all resources within Thorium, irrespective of the resource's
group. In contrast, a `User` or `Developer` must still have the correct group membership and group role if they plan on
using the resources of that group. Each Thorium deployment should have at least one person with the `Admin` system role.
`Admins` help to curate the data hosted in Thorium and provide continuity when group members leave the hosting
organization.

The `Disabled` role allows an `Admin` to revoke a user's access to Thorium without deleting their account or any of
the data they have uploaded. A disabled account receives a `401 Unauthorized` response on every request, including
login, starting with the account's next request after the role is assigned. An `Admin` can re-enable a disabled
account at any time by assigning it any other `System` role. Admins cannot disable their own account.

The following table summarizes the abilities granted by Thorium's five `System` level roles and any limitations that
apply to those granted abilities:

| System Role | Abilities | Limited By |
| --- | ---------- | ---- |
| User | Can create groups and run existing pipelines, but cannot create or modify pipelines or tools. | Must have sufficient group role and group membership |
| Analyst | Can create groups, and can add, modify, and run analysis pipelines and tools.  Has global view into all data in Thorium. | None |
| Developer | Can create groups, and can add, modify, and run analysis pipelines and tools. | Must have sufficient group role and group membership |
| Admin | Can access, view, and modify all resources, change group membership and update System and Group roles. | None |
| Disabled | None. A disabled account receives a `401 Unauthorized` on every request, including login. | Cannot access Thorium at all until an `Admin` assigns another role |

You can view your `System` role with `GET /api/users/whoami` (the `role` field of the response). Admins change roles
and group membership

```bash
curl -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/users/whoami
```

### Group Roles

`Group` roles control your ability to conduct certain operations on the group's resources. Group resources can include
tools, pipelines, repos, files, tags, comments, and analysis tool results.

There are four group roles: `Owner`, `Manager`, `User`, and `Monitor`. Anyone with a Thorium account can create their
own groups. When you create a new group, you are automatically added as an `Owner` of the group. When you are added to
an existing group your role within the group will be assigned. `Group` roles and their associated capabilities are
defined in the following table.

| Ability | Owners | Managers | Users | Monitors |
| ---- | ---- | ---- | ---- | ---- |
| View Resources | yes | yes | yes | yes |
| Run Pipelines | yes | yes | yes | no |
| Upload/Create Resources[^1] | yes | yes | yes | no |
| Modify Resources[^1]  | all | all | self created only | no |
| Delete Resources | all | all | self created only | no |
| Group Membership | add/remove any member | add/remove non-owner members | read only | read only |
| Delete Group | yes | no | no | no |

 [^1]: For pipelines and tools this ability also requires a `Developer` or `Admin` `System` level role. Without the correct
`System` role, you will not be able to modify or create pipelines or tools even if you have the correct group role
(`Owner`, `Manager`, or `User`). However, you will still be able to run existing pipelines that other `Developers`
have added so long as you are not a `Monitor`.
