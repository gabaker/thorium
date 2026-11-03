//! Handles changing the tags of a reaction

use thorium::models::{Reaction, ReactionUpdate};
use thorium::{Error, Thorium};

use crate::args::reactions::UpdateReaction;
use crate::handlers::runs::find_run;

/// Add and remove the tags a reaction is listed under
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The update command to execute
pub async fn retag(thorium: &Thorium, cmd: &UpdateReaction) -> Result<(), Error> {
    // find the reaction and the group it is in
    let reaction = find_run(thorium, cmd.reaction, cmd.group.as_deref()).await?;
    // show the change and stop when asked to
    if cmd.dry_run {
        println!(
            "Would update reaction {}: add tags [{}], remove tags [{}]",
            reaction.id,
            cmd.add_tag.join(", "),
            cmd.remove_tag.join(", ")
        );
        return Ok(());
    }
    // send the tag changes
    let change = ReactionUpdate {
        add_tags: cmd.add_tag.clone(),
        remove_tags: cmd.remove_tag.clone(),
        ..ReactionUpdate::default()
    };
    let updated = thorium
        .reactions
        .update(&reaction.group, &reaction.id, &change)
        .await?;
    // say what changed and which tags the reaction now has
    println!("{}", retag_message(&updated, cmd));
    Ok(())
}

/// Describe a finished retag: the requested tag changes and the tags the reaction now has
///
/// # Arguments
///
/// * `updated` - The reaction the API returned after the update
/// * `cmd` - The update command that was executed
#[must_use]
pub fn retag_message(updated: &Reaction, cmd: &UpdateReaction) -> String {
    // list the requested changes, then the tags the API stored
    format!(
        "Updated reaction {}: added tags [{}], removed tags [{}]; tags are now [{}]",
        updated.id,
        cmd.add_tag.join(", "),
        cmd.remove_tag.join(", "),
        updated.tags.join(", ")
    )
}
