# Delete a User

Only admins can delete the Thorium accounts of users. Neither Thorctl nor `thoradm` has a command for it, so an admin
deletes an account with the API:

```bash
curl -X DELETE -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/users/delete/<USERNAME>
```

The route answers `204` on success, `404` when the user doesn't exist, and `401` unless you use a full admin token (a
scoped token, including a run token, is refused). Deleting an account doesn't delete the files and other resources
the user uploaded: groups own those (see [Delete a Group](./delete_group.md)).

To lock an account out without deleting it, give it the `Disabled` system role instead (see [Roles, Permissions, and
Ownership](../getting_started/roles_permissions.md#system-roles)). To make a user's current token stop working, log
them out with `GET /api/users/logout/<USERNAME>`.
