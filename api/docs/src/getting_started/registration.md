# Registering For A New Thorium Account

Before you can login to Thorium you need to register for an account. Thorctl has no registration command; an account is
created with `POST /api/users/` and a JSON body with a `username`, `password`, and `email`. If your Thorium instance is
using LDAP then use your LDAP password, otherwise you choose your new password here.

```bash
curl -X POST https://<URL>/api/users/ -H 'Content-Type: application/json' \
  -d '{"username": "alice", "password": "<PASSWORD>", "email": "alice@example.com"}'
```

The response holds your token. A new account gets the `User` system role (see [Roles, Permissions, and
Ownership](./roles_permissions.md)); roles other than `User`, local accounts, and skipping email verification need the
deployment's secret key and are meant for admins. A name that is taken is refused with `409`.

Once you have an account, [log in](./login.md).
