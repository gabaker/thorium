//! The document people edit to change a pipeline
//!
//! The view names the tool behind each stage instead of carrying ids, so it reads the same in
//! every deployment and can be written by hand. `pipelines create --spec`, `pipelines edit` and
//! `pipelines update` all read and write it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thorium::Error;
use thorium::models::{EventTrigger, Pipeline};

use crate::utils::pipelines::stage_tools::StageToolRef;

/// The editable parts of a pipeline
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineView {
    /// What the pipeline does
    #[serde(default)]
    pub description: Option<String>,
    /// The seconds a reaction has to meet its SLA (the deployment's default when left out)
    #[serde(default)]
    pub sla: Option<u64>,
    /// The stages in run order; stages in one group run together
    #[serde(default)]
    pub order: Vec<Vec<String>>,
    /// The events that start the pipeline, by trigger name
    #[serde(default)]
    pub triggers: BTreeMap<String, EventTrigger>,
    /// The tool that runs each stage, by stage name
    #[serde(default)]
    pub stage_tools: BTreeMap<String, StageToolRef>,
}

impl PipelineView {
    /// Build the view of a stored pipeline
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline as stored
    /// * `tools` - The tool behind each of its stages, by name
    #[must_use]
    pub fn new(pipeline: &Pipeline, tools: &BTreeMap<String, StageToolRef>) -> Self {
        PipelineView {
            description: pipeline.description.clone(),
            sla: Some(pipeline.sla),
            order: pipeline.order.clone(),
            triggers: pipeline.triggers.clone().into_iter().collect(),
            stage_tools: tools.clone(),
        }
    }

    /// Check the view the way the API will, so a mistake is reported before anything is sent
    ///
    /// Every stage needs a tool, and only stages in the order can have one.
    pub fn check(&self) -> Result<(), Error> {
        // a pipeline runs at least one stage
        if self.order.is_empty() || self.order.iter().any(Vec::is_empty) {
            return Err(Error::new(
                "A pipeline needs at least one stage, and no stage group may be empty (see `order`)",
            ));
        }
        // stage names are real and used once
        let mut stages = BTreeSet::new();
        for stage in self.order.iter().flatten() {
            if stage.trim().is_empty() {
                return Err(Error::new("A stage in `order` has no name"));
            }
            if !stages.insert(stage.as_str()) {
                return Err(Error::new(format!(
                    "Stage '{stage}' appears more than once in `order`"
                )));
            }
        }
        // every stage runs a tool
        let missing: Vec<&str> = stages
            .iter()
            .filter(|stage| !self.stage_tools.contains_key(**stage))
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(Error::new(format!(
                "Every stage needs a tool; no tool is set for: {} (add each under `stage_tools`, e.g. --stage-tool STAGE=TOOL)",
                missing.join(", ")
            )));
        }
        // a tool backs a stage that exists
        let stray: Vec<&str> = self
            .stage_tools
            .keys()
            .filter(|stage| !stages.contains(stage.as_str()))
            .map(String::as_str)
            .collect();
        if !stray.is_empty() {
            return Err(Error::new(format!(
                "`stage_tools` names stages that are not in `order`: {}",
                stray.join(", ")
            )));
        }
        // a tool needs a name
        if let Some(stage) = self
            .stage_tools
            .iter()
            .find(|(_, tool)| tool.tool.trim().is_empty())
            .map(|(stage, _)| stage)
        {
            return Err(Error::new(format!("Stage '{stage}' names no tool")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::PipelineView;
    use crate::utils::pipelines::stage_tools::StageToolRef;

    /// Build a view with the given stages and tools
    ///
    /// # Arguments
    ///
    /// * `order` - The stage groups
    /// * `tools` - The tool of each stage
    fn view(order: &[&[&str]], tools: &[(&str, &str)]) -> PipelineView {
        PipelineView {
            description: None,
            sla: None,
            order: order
                .iter()
                .map(|group| group.iter().map(|stage| (*stage).to_owned()).collect())
                .collect(),
            triggers: BTreeMap::default(),
            stage_tools: tools
                .iter()
                .map(|(stage, tool)| {
                    (
                        (*stage).to_owned(),
                        StageToolRef {
                            tool: (*tool).to_owned(),
                            version: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// A view whose stages all have tools is accepted
    #[test]
    fn complete_view_is_accepted() {
        let view = view(
            &[&["a", "b"], &["c"]],
            &[("a", "x"), ("b", "x"), ("c", "y")],
        );
        view.check().unwrap();
    }

    /// A stage without a tool is refused, naming the stage
    #[test]
    fn stage_without_tool_is_refused() {
        let err = view(&[&["a"], &["b"]], &[("a", "x")]).check().unwrap_err();
        assert!(err.to_string().contains("no tool is set for: b"), "{err}");
    }

    /// A tool for a stage that is not in the order is refused
    #[test]
    fn tool_for_unknown_stage_is_refused() {
        let err = view(&[&["a"]], &[("a", "x"), ("z", "x")])
            .check()
            .unwrap_err();
        assert!(err.to_string().contains("not in `order`: z"), "{err}");
    }

    /// An empty order, an empty group and a repeated stage are refused
    #[test]
    fn malformed_order_is_refused() {
        assert!(view(&[], &[]).check().is_err());
        assert!(view(&[&["a"], &[]], &[("a", "x")]).check().is_err());
        let err = view(&[&["a"], &["a"]], &[("a", "x")]).check().unwrap_err();
        assert!(err.to_string().contains("more than once"), "{err}");
    }

    /// Unknown keys are refused so a typo is not silently ignored
    #[test]
    fn unknown_keys_are_refused() {
        let doc = "order: [[a]]\nstage_tool:\n  a: {tool: x}\n";
        assert!(serde_norway::from_str::<PipelineView>(doc).is_err());
        let doc = "order: [[a]]\nstage_tools:\n  a: {tool: x, versoin: 1}\n";
        assert!(serde_norway::from_str::<PipelineView>(doc).is_err());
    }

    /// A tool without a version follows the tool's latest version
    #[test]
    fn version_defaults_to_latest() {
        let doc = "order: [[a]]\nstage_tools:\n  a: {tool: x}\n";
        let view = serde_norway::from_str::<PipelineView>(doc).unwrap();
        assert_eq!(view.stage_tools["a"].version, None);
        view.check().unwrap();
    }
}
