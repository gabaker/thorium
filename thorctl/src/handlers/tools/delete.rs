//! Deleting tool versions and tools

use thorium::models::ToolInstanceSummary;
use thorium::{Error, Thorium};

use super::target::resolve;
use crate::args::tools::DeleteTool;
use crate::utils;

/// What a delete command will remove
#[derive(Debug, Clone)]
pub enum Plan {
    /// One version of a tool
    Version {
        /// The version's label
        label: String,
        /// The version's id
        id: uuid::Uuid,
    },
    /// A tool and the versions that go with it, newest first
    Tool {
        /// The versions to delete before the tool
        versions: Vec<ToolInstanceSummary>,
    },
}

/// Check what a delete may remove
///
/// # Arguments
///
/// * `name` - The tool's name
/// * `version` - The version the target named, if any
/// * `versions` - The tool's versions, newest first
/// * `all_versions` - Whether `--all-versions` was given
pub fn plan(
    name: &str,
    version: Option<(String, uuid::Uuid)>,
    versions: Vec<ToolInstanceSummary>,
    all_versions: bool,
) -> Result<Plan, Error> {
    // a named version is deleted alone
    if let Some((label, id)) = version {
        if all_versions {
            return Err(Error::new(
                "--all-versions deletes a whole tool; name the tool without a version",
            ));
        }
        return Ok(Plan::Version { label, id });
    }
    // a tool with versions needs them deleted first, which the user must ask for
    if !versions.is_empty() && !all_versions {
        return Err(Error::new(format!(
            "Tool {name} still has {} version(s); pass --all-versions to delete them with the tool, or delete one with `thorctl tools delete {name}@VERSION`",
            versions.len()
        )));
    }
    Ok(Plan::Tool { versions })
}

/// Delete a tool version, or a tool and its versions
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete command
pub async fn delete(thorium: &Thorium, cmd: &DeleteTool) -> Result<(), Error> {
    // find the tool and the version the target names
    let found = resolve(thorium, &cmd.tool, false).await?;
    let version = match (&found.instance, found.explicit) {
        (Some(entity), true) => Some((
            found.instance_meta("delete")?.version.to_string(),
            entity.id,
        )),
        _ => None,
    };
    // list the versions only a whole-tool delete cares about
    let versions = if version.is_none() {
        thorium.entities.tool_instances(found.tool.id).await?
    } else {
        Vec::new()
    };
    let plan = plan(&found.tool.name, version, versions, cmd.all_versions)?;
    // describe what will go
    let what = match &plan {
        Plan::Version { label, .. } => format!("version {label} of tool {}", found.tool.name),
        Plan::Tool { versions } if versions.is_empty() => format!("tool {}", found.tool.name),
        Plan::Tool { versions } => format!(
            "tool {} and its {} version(s)",
            found.tool.name,
            versions.len()
        ),
    };
    // a dry run only says what would go
    if cmd.dry_run {
        println!("Would delete {what}");
        return Ok(());
    }
    // deleting can't be undone, so confirm unless told not to
    if !cmd.skip_confirm {
        utils::require_confirm_terminal("--skip-confirm")?;
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("Delete {what}?"))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Cancelled.");
            return Ok(());
        }
    }
    // delete the versions first, newest first, then the tool
    match plan {
        Plan::Version { id, .. } => {
            thorium
                .entities
                .delete(id)
                .await
                .map_err(utils::delete_refusal)?;
        }
        Plan::Tool { versions } => {
            for version in &versions {
                thorium.entities.delete(version.id).await.map_err(|err| {
                    Error::new(format!(
                        "Failed to delete version {} of tool {}: {}",
                        version.version,
                        found.tool.name,
                        utils::delete_refusal(err)
                    ))
                })?;
            }
            thorium
                .entities
                .delete(found.tool.id)
                .await
                .map_err(utils::delete_refusal)?;
        }
    }
    println!("Deleted {what}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use thorium::models::{ToolInstanceSummary, ToolVersion};
    use uuid::Uuid;

    use super::{Plan, plan};

    /// Build a version summary
    ///
    /// # Arguments
    ///
    /// * `label` - The version label
    fn summary(label: &str) -> ToolInstanceSummary {
        ToolInstanceSummary {
            id: Uuid::new_v4(),
            version: ToolVersion::from(label),
            spec_hash: "hash".to_owned(),
            deprecated: false,
            created: Utc::now(),
        }
    }

    /// A tool with versions needs `--all-versions`, a version deletes alone, and a tool
    /// without versions deletes by itself
    #[test]
    fn plans_what_to_delete() {
        // a tool with versions is refused without --all-versions
        let versions = vec![summary("1.1.0"), summary("1.0.0")];
        let err = plan("scan", None, versions.clone(), false).unwrap_err();
        assert!(err.to_string().contains("--all-versions"), "{err}");
        // --all-versions deletes them with the tool
        let planned = plan("scan", None, versions.clone(), true).unwrap();
        assert!(matches!(planned, Plan::Tool { versions: found } if found.len() == 2));
        // a tool without versions deletes alone
        let planned = plan("scan", None, Vec::new(), false).unwrap();
        assert!(matches!(planned, Plan::Tool { versions } if versions.is_empty()));
        // a named version deletes alone, and can't be combined with --all-versions
        let id = Uuid::new_v4();
        let planned = plan("scan", Some(("1.0.0".into(), id)), Vec::new(), false).unwrap();
        assert!(
            matches!(planned, Plan::Version { label, id: found } if label == "1.0.0" && found == id)
        );
        assert!(plan("scan", Some(("1.0.0".into(), id)), Vec::new(), true).is_err());
    }
}
