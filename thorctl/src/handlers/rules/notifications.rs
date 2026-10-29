//! Handles detection rule notification related commands

use owo_colors::OwoColorize;
use thorium::models::{NotificationParams, NotificationRequest};
use thorium::{Error, Thorium};

use crate::args::rules::{
    CreateRuleNotification, DeleteRuleNotification, GetRuleNotifications, RuleNotifications,
};
use crate::err_not_admin;
use crate::utils;

use super::get_rule;

/// Get a detection rule's notifications
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The get rule notifications command to execute
async fn get_notifications(thorium: &Thorium, cmd: &GetRuleNotifications) -> Result<(), Error> {
    // make sure this entity exists and is actually a detection rule
    let entity = get_rule(thorium, &cmd.rule).await?;
    // get this rule's notifications using the resolved id
    let notifications = thorium.entities.get_notifications(entity.id).await?;
    // print this rule's notifications
    utils::notifications::print_notifications(&notifications, cmd.opts.ids);
    Ok(())
}

/// Create a detection rule notification
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The create rule notification command to execute
async fn create_notification(thorium: &Thorium, cmd: &CreateRuleNotification) -> Result<(), Error> {
    // make sure this entity exists and is actually a detection rule
    let entity = get_rule(thorium, &cmd.rule).await?;
    // create a notification request
    let req =
        NotificationRequest::new(cmd.notification.msg.clone(), cmd.notification.level.clone());
    // create the notification params
    let params = NotificationParams {
        expire: cmd.notification.expire,
    };
    // create the notification using the resolved id (admins only)
    err_not_admin!(
        thorium
            .entities
            .create_notification(entity.id, &req, &params)
            .await,
        "create rule notifications"
    );
    // show that this notification was created
    println!(
        "{} notification for rule '{}' ({})",
        "Created".bright_green(),
        entity.name,
        entity.id
    );
    Ok(())
}

/// Delete a detection rule notification
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The delete rule notification command to execute
async fn delete_notification(thorium: &Thorium, cmd: &DeleteRuleNotification) -> Result<(), Error> {
    // make sure this entity exists and is actually a detection rule
    let entity = get_rule(thorium, &cmd.rule).await?;
    // delete the notification using the resolved id (admins only)
    err_not_admin!(
        thorium
            .entities
            .delete_notification(entity.id, &cmd.notification_id)
            .await,
        "delete rule notifications"
    );
    // show that this notification was deleted; the rule's id is omitted so
    // the two-uuid line stays narrow
    println!(
        "{} notification {} for rule '{}'",
        "Deleted".bright_green(),
        cmd.notification_id,
        entity.name
    );
    Ok(())
}

/// Handle the rule notification subcommands
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `cmd` - The rule notifications subcommand to execute
pub async fn handle(thorium: &Thorium, cmd: &RuleNotifications) -> Result<(), Error> {
    // call the right rule notifications handler
    match cmd {
        RuleNotifications::Get(cmd) => get_notifications(thorium, cmd).await,
        RuleNotifications::Create(cmd) => create_notification(thorium, cmd).await,
        RuleNotifications::Delete(cmd) => delete_notification(thorium, cmd).await,
    }
}
