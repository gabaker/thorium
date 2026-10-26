//! Handles changing the tags of a reaction

use thorium::models::ReactionUpdate;
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
    thorium
        .reactions
        .update(&reaction.group, &reaction.id, &change)
        .await?;
    // say what changed; the API lists a reaction under its tags without rewriting the tags it
    // stores, so the tags a read returns are not a reliable record of this change
    println!(
        "Updated reaction {}: added tags [{}], removed tags [{}]",
        reaction.id,
        cmd.add_tag.join(", "),
        cmd.remove_tag.join(", ")
    );
    Ok(())
}
