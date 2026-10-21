//! Logic for interacting with notifications in the database

use tracing::instrument;

use crate::{
    models::backends::NotificationSupport,
    models::schema::scylla::NotificationsTableRef,
    models::schema::{QueryError, text_of},
    models::{Notification, NotificationLevel, NotificationRow},
    utils::{ApiError, Shared},
};

/// Save a notification to scylla
///
/// # Arguments
///
/// * `notification` - The notification to save
/// * `expire` - Whether or not this notification should automatically expire
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::notifications::create", skip_all, err(Debug))]
pub async fn create<N: NotificationSupport>(
    notification: Notification<N>,
    expire: Option<bool>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // determine whether or not this notification should automatically expire
    // if no explicit setting was given, the notification should only expire if it's not an error
    let expire = expire.unwrap_or_else(|| notification.level != NotificationLevel::Error);
    // the key is stored as the text it binds as
    let key = text_of(&notification.key).map_err(QueryError::from)?;
    let kind = N::notification_type();
    let row = NotificationsTableRef {
        kind: &kind,
        key: &key,
        created: notification.created,
        id: notification.id,
        msg: &notification.msg,
        level: &notification.level,
        ban_id: notification.ban_id,
    };
    if expire {
        // save the notification to scylla
        shared
            .scylla
            .queries
            .notifications
            .insert(&shared.scylla.session, &row)
            .await?;
    } else {
        // save the notification to scylla with no expiration
        shared
            .scylla
            .queries
            .notifications
            .insert_no_expire(&shared.scylla.session, &row)
            .await?;
    }
    Ok(())
}

/// Get all notifications for an entity at the given key
///
/// # Arguments
///
/// * `key` - The entity's unique key
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::notifications::get_all", skip_all, err(Debug))]
pub async fn get_all<N: NotificationSupport>(
    key: &N::Key,
    shared: &Shared,
) -> Result<Vec<Notification<N>>, ApiError> {
    // query for the notifications, binding the key as the text it's stored as
    let stored = text_of(key).map_err(QueryError::from)?;
    let rows = shared
        .scylla
        .queries
        .notifications
        .get(&shared.scylla.session, &N::notification_type(), &stored)
        .await?;
    // instance a list of notification with the right size
    let mut notifs = Vec::with_capacity(rows.len());
    // build our notifications
    for row in rows {
        // try to deserialie this row
        let NotificationRow {
            created,
            id,
            msg,
            level,
            ban_id,
            ..
        } = row?;
        // build this notification and add it to our list; its stored key is the one we bound
        notifs.push(Notification {
            key: key.clone(),
            created,
            id,
            msg,
            level,
            ban_id,
        });
    }
    Ok(notifs)
}

/// Delete a specific notification
///
/// # Arguments
///
/// * `notification` - The notification to delete
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::notifications::delete", skip_all, err(Debug))]
pub async fn delete<N: NotificationSupport>(
    notification: &Notification<N>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // delete the notification in scylla, binding the key as the text it's stored as
    let key = text_of(&notification.key).map_err(QueryError::from)?;
    shared
        .scylla
        .queries
        .notifications
        .delete(
            &shared.scylla.session,
            &N::notification_type(),
            &key,
            notification.created,
            notification.id,
        )
        .await?;
    Ok(())
}

/// Deletes all notifications for a given entity
///
/// # Arguments
///
/// * `key` - The entity's unique key
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::notifications::delete_all", skip_all, err(Debug))]
pub async fn delete_all<N: NotificationSupport>(
    key: &N::Key,
    shared: &Shared,
) -> Result<(), ApiError> {
    // delete all of the entity's notifications in scylla, binding the key as stored
    let key = text_of(key).map_err(QueryError::from)?;
    shared
        .scylla
        .queries
        .notifications
        .delete_all(&shared.scylla.session, &N::notification_type(), &key)
        .await?;
    Ok(())
}
