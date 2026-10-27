# Revoking Your Token

If for some reason you need to revoke your Thorium token, log out through the API: `POST /api/users/logout` replaces
your token with a new one, so the old token stops working at once. Thorctl has no command that revokes your main token;
`thorctl tokens delete <NAME>` deletes scoped tokens only, after asking (pass `--skip-confirm` to delete without a prompt;
see [Run-scoped tokens](./runs.md#run-scoped-tokens)).

```bash
curl -X POST -H "Authorization: Bearer $TOKEN_B64" https://<URL>/api/users/logout
```

The route answers `204`. When you call it with a scoped token, only that scoped token is replaced, and a run token can't
be logged out (`401`). Admins can log another user out with `GET /api/users/logout/<USERNAME>`.

Afterwards run `thorctl login <URL>` again to get a new token for Thorctl.

Reactions run as your user and with your user's Thorium token. As a result, revoking your token will cause any
currently `Running` reactions to fail. This includes reactions in the `Running` state or reactions in the `Created`
state that start to run before the revocation process completes. You can always resubmit reactions that fail after
you have revoked your token.
