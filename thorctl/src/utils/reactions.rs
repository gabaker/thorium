//! Utility functions relating to reactions

use thorium::{Error, Thorium, models::Reaction};
use uuid::Uuid;

/// Search for a particular reaction in every group that a user is in; useful when
/// no specific reaction group is given
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `reaction_id` - The id of the reaction to find
pub async fn find_reaction_no_group(
    thorium: &Thorium,
    reaction_id: &Uuid,
) -> Result<Reaction, Error> {
    // get all groups for the current user
    let groups = super::groups::get_all_groups(thorium).await?;
    // concurrently query for the reaction in every group
    let reaction_search_results = futures::future::join_all(
        groups
            .into_iter()
            .map(|group| async move { thorium.reactions.get(&group, *reaction_id).await }),
    )
    .await;
    // look for a valid reaction among the results
    for res in reaction_search_results {
        if res.is_ok() {
            return res;
        }
    }
    // return a 404 NOT FOUND error if the reaction is not in any of the groups
    Err(Error::Thorium {
        code: http::StatusCode::NOT_FOUND,
        msg: Some(format!(
            "Reaction {reaction_id} not found in any of the user's groups"
        )),
    })
}

/// The column names of a reaction table
pub const REACTION_COLUMNS: &[&str] = &[
    "ID", "GROUP", "PIPELINE", "STATUS", "STAGE", "SAMPLES", "CREATOR",
];

/// Render a reaction as one row of a reaction table
///
/// # Arguments
///
/// * `reaction` - The reaction to render
#[must_use]
pub fn reaction_row(reaction: &Reaction) -> Vec<String> {
    // show the sample itself if there is one, or how many there are
    let samples = match reaction.samples.len() {
        0 => "-".to_owned(),
        1 => reaction.samples[0].clone(),
        len => format!("{len} samples"),
    };
    // build our row with one value per column
    vec![
        reaction.id.to_string(),
        reaction.group.clone(),
        reaction.pipeline.clone(),
        format!("{:?}", reaction.status),
        reaction.current_stage.to_string(),
        samples,
        reaction.creator.clone(),
    ]
}

/// Get the labeled fields shown for a reaction in human-readable output
///
/// # Arguments
///
/// * `reaction` - The reaction to get fields for
fn reaction_fields(reaction: &Reaction) -> Vec<(&'static str, String)> {
    // show a dash for empty lists
    let list = |items: &[String]| {
        if items.is_empty() {
            "-".to_owned()
        } else {
            items.join(", ")
        }
    };
    // label each field we show
    vec![
        ("Pipeline", reaction.pipeline.clone()),
        ("Status", format!("{:?}", reaction.status)),
        ("Creator", reaction.creator.clone()),
        (
            "Stage",
            format!(
                "{} ({}/{} jobs done)",
                reaction.current_stage,
                reaction.current_stage_progress,
                reaction.current_stage_length
            ),
        ),
        ("Samples", list(&reaction.samples)),
        ("Tags", list(&reaction.tags)),
        (
            "Parent",
            reaction
                .parent
                .map_or_else(|| "-".to_owned(), |parent| parent.to_string()),
        ),
        (
            "Sub-reactions",
            format!(
                "{}/{} completed",
                reaction.completed_sub_reactions, reaction.sub_reactions
            ),
        ),
        ("SLA", reaction.sla.to_rfc3339()),
    ]
}

/// Render a reaction as human-readable text
///
/// # Arguments
///
/// * `reaction` - The reaction to render
/// * `ansi` - Whether to style the text with ANSI escape codes
#[must_use]
pub fn reaction_human(reaction: &Reaction, ansi: bool) -> String {
    // start with the reaction's id and group
    let mut lines = vec![super::render::header(
        &reaction.id.to_string(),
        &reaction.group,
        ansi,
    )];
    // add one line per field
    lines.extend(
        reaction_fields(reaction)
            .into_iter()
            .map(|(name, value)| super::render::field(name, &value, ansi)),
    );
    lines.join("\n")
}

/// The reactions one command shows
#[derive(serde::Serialize)]
#[serde(transparent)]
pub struct ReactionsView(pub Vec<Reaction>);

impl super::output::View for ReactionsView {
    /// Render the reactions as a table
    fn table(&self) -> String {
        super::output::table_from(REACTION_COLUMNS, self.0.iter().map(reaction_row).collect())
    }

    /// Render the reactions as human-readable text
    ///
    /// # Arguments
    ///
    /// * `ansi` - Whether to style the text with ANSI escape codes
    fn human(&self, ansi: bool) -> String {
        // separate each reaction with a rule
        let separator = format!("\n{}\n", super::render::separator(ansi));
        self.0
            .iter()
            .map(|reaction| reaction_human(reaction, ansi))
            .collect::<Vec<String>>()
            .join(&separator)
    }
}
