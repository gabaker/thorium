# Troubleshooting User Issues

It can be difficult to troubleshoot a user's issues without seeing what they see. The Web UI has an admin-only feature
that masquerades as another user; Web UI instructions will be added with the new interface. With Thorctl and the API,
admins can instead:

- list a user's groups with `GET /api/users/user/<USERNAME>`, and read any group or resource, since admins can see
  every group's data;
- read a user's run with `thorctl runs describe <REACTION>` and `thorctl runs logs <REACTION>`, and a reaction with
  `thorctl reactions describe <REACTION>` and `thorctl reactions logs <REACTION>` (see
  [Running Tools and Workflows](../users/runs.md));
- log a user out with `GET /api/users/logout/<USERNAME>`, which replaces their token so they log in again.
