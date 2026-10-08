# Groups

Groups are how Thorium will let users permission their pipelines and reactions. In
order for another user to use or see your pipeline they must also be in the
same group as that pipeline.

Before you can create anything in Thorium you need to either create or be a part of
the group you wish to place those objects in. You can create a group by POSTing
to the following endpoint.
```
<api_url>/groups/
```

In the body of that request you must put the name of the group you want to create
and optionally any other users you want to be in this group. You will be added as
the owner of this group automatically and cannot have any other roles. The roles
are the following with each role having all the abilities of the roles below them:

| role | abilities |
| --- | ---------- |
| owners | delete entire group and modify roles |
| managers | modify non-owner roles and delete other users jobs/pipelines |
| users | create jobs and delete their own jobs/pipelines |
| monitors | monitor jobs and pipelines |

The body you should post should look like this (with all role arrays being optional):

```json
{
  "name": "TestGroup",
  "owners": ["bob", "sarah"],
  "managers": ["todd"],
  "users": ["joe", "molly"],
  "monitors": ["dave"]
}
```

### LDAP Support

Users and groups in Thorium can also be backed by LDAP. In order for this to work you must
have configured ldap settings in your [Thorium config](../../admins/deploy/concepts.md#configuration-and-credentials). Its also important
to remember that LDAP metagroups and manual Thorium group permissions cannot be mixed within
the same role. So if you want to sync the owners role for a group with one or more LDAP
metagroups you cannot also assign a Thorium user that role directly. You must give them the
role through an LDAP metagroup. You can however mix LDAP and manual Thorium group permissions
across different roles. So owners and managers could be controlled through LDAP metagroups while
users and monitors are not.

In order to assign LDAP metagroup permissions at group creation you can change the body in the above
post to something like this (where each role is optional).

```json
{
  "name": "TestGroup",
  "ldap_owners": ["ldap-owners"],
  "ldap_managers": ["ldap-managers"],
  "ldap_users": ["ldap-users", "other-ldap-users"],
  "ldap_monitors": ["ldap-monitors"]
}
```

Remember you can mix different roles between LDAP and manual Thorium groups so this is also valid.

```json
{
  "name": "TestGroup",
  "owners": ["bob", "sara"],
  "ldap_managers": ["ldap-managers"],
  "users": ["joe", "molly"],
  "ldap_monitors": ["ldap-monitors"]
}
`
