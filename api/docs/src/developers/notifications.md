# Notifications

> If you're a Thorium admin looking for instructions on creating/deleting notifications, see
> [Create Notifications](../admins/notifications_admins.md).

Notifications are short pieces of information regarding specific entities in Thorium. They are often automatically created when an
entity is banned to inform the user of the ban and the reason for it (see [Bans](./bans.md)), but they can also be manually created by
Thorium admins.

## Notification Levels

Notifications are assigned a level depending on their severity, similar to log levels in other programs. Below is a table of levels
and a description of each one:

| Level | Description | Expires by default? |
| ----- | ----------- | -------- |
| Info  | The notification provides some helpful information about the entity that has little or no bearing on its function | Yes |
| Warn  | The notification warns users of a possible issue with the entity that may affect its function but doesn't completely disrupt it | Yes |
| Error | The notification alerts users of a serious issue that impedes the function of the entity | No |

When an entity receives a ban, a notification at the `Error` level is automatically created for the entity. The notification
is automatically deleted when the ban is deleted. If an entity has multiple bans, the entity will have multiple notifications,
one for each ban.

### Notification Expiration

Notifications can automatically "expire" (be deleted) according to the retention settings in the Thorium cluster config
(7 days by default). The third column of the above table defines the default expiration behavior of each notification level,
specifically that the `Info` and `Warn` levels will expire by default while the `Error` will not. This is because `Error`
notifications are most often associated with bans and should only be deleted once the ban has been removed. Levels' expiration
behaviors can be overridden on notification creation (see
[Creating Notifications - Expiration Behavior](../admins/notifications_admins.md#expiration-behavior) for more info).

## Viewing Notifications

### Thorctl

#### Image Notifications

You can view notifications for an image with Thorctl with the following command:

```
thorctl images notifications get <GROUP> <IMAGE>
```

This will provide a list of the image's notifications color-coded to their level (blue for `Info`, yellow for `Warn`, and red for `Error`).

#### Pipeline Notifications

You can view notifications for a pipeline with Thorctl with the following command:

```
thorctl pipelines notifications get <GROUP> <PIPELINE>
```

This will provide a list of the pipeline's notifications color-coded to their level (blue for `Info`, yellow for `Warn`, and red for `Error`).

### API

Notifications are also returned by the API: `GET /api/images/notifications/{group}/{image}` and
`GET /api/pipelines/notifications/{group}/{pipeline}` list them, and `GET /api/users/notifications/` lists yours.
Notifications are not shown in the Web UI yet.
