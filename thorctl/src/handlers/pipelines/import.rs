//! Pipeline import support for thorctl
//!
//! Loads pipeline configs from an on-disk export directory and applies them
//! through the shared conflict engine. A config names the tool behind each stage (by name,
//! with an optional version) rather than by id, so an export imports into any deployment whose
//! target group has tools of those names. The orchestration of the import lives in the shared
//! on-disk import driver (`imports::disk`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thorium::models::{EventTrigger, PipelineRequest};
use thorium::{CtlConf, Error, Thorium};

use super::edit::PipelineData;
use crate::handlers::imports::kind::PipelineKind;
use crate::handlers::imports::rollback::Journal;
use crate::handlers::imports::{self, ApplyCtx, ApplyOutcome, categorize, create};
use crate::handlers::progress::Bar;
use crate::utils::pipelines::stage_tools::{self, StageToolRef};

/// How a pipeline config names the tool behind a stage
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StageToolEntry {
    /// A tool and an optional version, as exports write them
    Ref(StageToolRef),
    /// `TOOL`, `TOOL@VERSION`, or a tool or tool instance id, for configs written by hand
    Text(String),
}

impl StageToolEntry {
    /// The tool this entry names
    pub fn tool(&self) -> Result<StageToolRef, Error> {
        match self {
            StageToolEntry::Ref(tool) => Ok(tool.clone()),
            StageToolEntry::Text(text) => StageToolRef::parse(text),
        }
    }
}

/// A pipeline as an export directory stores it
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// The group the pipeline was exported from
    pub group: String,
    /// The name of the pipeline
    pub name: String,
    /// The stages in run order, as a list of stages or of groups of stages
    pub order: Value,
    /// The seconds a reaction has to meet the pipeline's SLA
    #[serde(default)]
    pub sla: Option<u64>,
    /// The events that start the pipeline
    #[serde(default)]
    pub triggers: HashMap<String, EventTrigger>,
    /// What the pipeline does
    #[serde(default)]
    pub description: Option<String>,
    /// The tool behind each stage, by stage name
    #[serde(default)]
    pub stage_tools: BTreeMap<String, StageToolEntry>,
}

impl From<PipelineData> for PipelineConfig {
    /// Build the config an export writes for a pipeline
    ///
    /// # Arguments
    ///
    /// * `data` - The pipeline and the tool behind each of its stages
    fn from(data: PipelineData) -> Self {
        PipelineConfig {
            group: data.pipeline.group,
            name: data.pipeline.name,
            order: Value::from(data.pipeline.order),
            sla: Some(data.pipeline.sla),
            triggers: data.pipeline.triggers,
            description: data.pipeline.description,
            stage_tools: data
                .tools
                .into_iter()
                .map(|(stage, tool)| (stage, StageToolEntry::Ref(tool)))
                .collect(),
        }
    }
}

impl PipelineConfig {
    /// Build the request that creates this pipeline, with its stage tools still unresolved
    ///
    /// # Arguments
    ///
    /// * `group` - The group the pipeline is imported into
    fn request(&self, group: &str) -> PipelineRequest {
        // the config's own fields carry over, in the group being imported into
        let mut request = PipelineRequest::new(group, &self.name, self.order.clone());
        request.sla = self.sla;
        request.triggers.clone_from(&self.triggers);
        request.description.clone_from(&self.description);
        request
    }

    /// Check that every stage has a tool, so an export from before pipelines had tools is
    /// refused before any request is sent
    ///
    /// # Arguments
    ///
    /// * `path` - The config's path, for the message
    pub fn check_stage_tools(&self, path: &Path) -> Result<(), Error> {
        // read the stages out of the order
        let request = self.request(&self.group);
        let order = request
            .deserialize_image_order()
            .map_err(|err| Error::new(format!("Pipeline config '{}': {err}", path.display())))?;
        // every stage needs a tool, and an old export has none
        let missing: Vec<&str> = order
            .into_iter()
            .flatten()
            .filter(|stage| !self.stage_tools.contains_key(*stage))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(Error::new(format!(
            "Pipeline config '{}' gives no tool for stage(s): {}. Pipelines need a tool for every \
             stage, and this config predates stage tools. Add a \"stage_tools\" entry per stage \
             (for example \"scan\": {{\"tool\": \"strings\", \"version\": null}}), or create the \
             pipeline with `thorctl pipelines create` and set stages with `thorctl pipelines \
             stage-tool`.",
            path.display(),
            missing.join(", ")
        )))
    }
}

/// Load a pipeline config from the export directory and point it at our group
///
/// The config must name a tool for every stage; nothing is requested from Thorium.
///
/// # Arguments
///
/// * `import_dir` - The export directory holding `pipelines/<name>.json`
/// * `name` - The name of the pipeline whose config we are loading
pub async fn load_config(import_dir: &Path, name: &str) -> Result<PipelineConfig, Error> {
    // read the config from disk
    let path = import_dir.join("pipelines").join(format!("{name}.json"));
    let data = tokio::fs::read_to_string(&path).await.map_err(|err| {
        Error::new(format!(
            "Failed to read pipeline config '{}': {err}",
            path.display()
        ))
    })?;
    // parse it
    let config: PipelineConfig = serde_json::from_str(&data).map_err(|err| {
        Error::new(format!(
            "Failed to parse pipeline config '{}': {err}; each \"stage_tools\" entry is \
             {{\"tool\": NAME, \"version\": LABEL or null}} or \"NAME[@LABEL]\"",
            path.display()
        ))
    })?;
    // the file stem and the config's own name must agree
    if config.name != name {
        return Err(Error::new(format!(
            "The pipeline config '{}' is named '{}' but its file name implies '{name}'; \
             rename the file to '{}.json' or set its name to '{name}'",
            path.display(),
            config.name,
            config.name,
        )));
    }
    // every stage needs a tool
    config.check_stage_tools(&path)?;
    Ok(config)
}

/// Turn a loaded config into the request that creates or updates the pipeline in our group
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group to import the pipeline into
/// * `config` - The pipeline's config
pub async fn to_request(
    thorium: &Thorium,
    group: &str,
    config: &PipelineConfig,
) -> Result<PipelineRequest, Error> {
    // name the tool of each stage
    let refs = config
        .stage_tools
        .iter()
        .map(|(stage, entry)| Ok((stage.clone(), entry.tool()?)))
        .collect::<Result<BTreeMap<String, StageToolRef>, Error>>()?;
    // find the tools in the target group
    let ids = stage_tools::resolve(thorium, group, &refs)
        .await
        .map_err(|err| {
            Error::new(format!(
                "Pipeline '{}': {err}; import or create the tool in group '{group}' first",
                config.name
            ))
        })?;
    // build the request with the tools in place
    let mut request = config.request(group);
    request.stage_tools = ids;
    Ok(request)
}

/// Apply the categorized pipelines to Thorium according to the conflict mode
///
/// New pipelines are created and existing ones go through the shared conflict
/// dispatch (`imports::apply_existing`).
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `conf` - The Thorctl config (used for the default editor)
/// * `new` - The pipelines that don't exist in Thorium yet
/// * `existing` - The pipelines that already exist in Thorium
/// * `ctx` - The shared apply settings (mode, editor, prompting, workers)
/// * `progress` - The progress bar
/// * `journal` - The journal to record applied changes in
pub async fn apply_pipelines(
    thorium: &Thorium,
    conf: &CtlConf,
    new: Vec<&categorize::CategorizedPipeline>,
    existing: Vec<&categorize::CategorizedPipeline>,
    ctx: &ApplyCtx<'_>,
    progress: &Bar,
    journal: &Journal,
) -> Result<ApplyOutcome, Error> {
    // create the pipelines that don't exist yet, collecting per-pipeline failures
    let mut failures =
        create::import_new_pipelines(thorium, new, ctx.workers, progress, journal).await;
    // handle existing pipelines according to the conflict mode (shared dispatch)
    let existing = imports::apply_existing::<PipelineKind>(
        thorium,
        conf,
        existing,
        ctx.mode,
        ctx.editor,
        ctx.can_prompt,
        ctx.workers,
        progress,
        journal,
    )
    .await?;
    failures.extend(existing.failures);
    Ok(ApplyOutcome {
        outcome: existing.outcome,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::path::Path;

    use serde_json::json;
    use thorium::models::Pipeline;
    use uuid::Uuid;

    use super::{PipelineConfig, PipelineData, StageToolEntry};
    use crate::utils::pipelines::stage_tools::StageToolRef;

    /// Parse a config from JSON
    ///
    /// # Arguments
    ///
    /// * `value` - The config document
    fn config(value: &serde_json::Value) -> PipelineConfig {
        serde_json::from_value(value.clone()).unwrap()
    }

    /// An export without stage tools is refused, naming the stages and the commands to use
    #[test]
    fn config_without_stage_tools_is_refused() {
        let old = config(&json!({
            "group": "g", "name": "p", "order": [["a"], ["b"]], "sla": 10,
        }));
        let err = old
            .check_stage_tools(Path::new("exports/pipelines/p.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exports/pipelines/p.json"), "{err}");
        assert!(err.contains("stage(s): a, b"), "{err}");
        assert!(err.contains("pipelines create"), "{err}");
        assert!(err.contains("pipelines stage-tool"), "{err}");
        // a stage with a tool is not named
        let partial = config(&json!({
            "group": "g", "name": "p", "order": ["a", "b"],
            "stage_tools": {"a": {"tool": "x"}},
        }));
        let err = partial
            .check_stage_tools(Path::new("p.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("stage(s): b"), "{err}");
    }

    /// A config with a tool for every stage passes, in each way a tool can be written
    #[test]
    fn config_with_stage_tools_is_accepted() {
        let full = config(&json!({
            "group": "g", "name": "p", "order": [["a", "b"], ["c"]],
            "stage_tools": {
                "a": {"tool": "x", "version": null},
                "b": {"tool": "y", "version": "1.0.0"},
                "c": "z@2.0.0",
            },
        }));
        full.check_stage_tools(Path::new("p.json")).unwrap();
        assert_eq!(
            full.stage_tools["b"].tool().unwrap(),
            StageToolRef {
                tool: "y".to_owned(),
                version: Some("1.0.0".to_owned())
            }
        );
        assert_eq!(full.stage_tools["c"].tool().unwrap().to_string(), "z@2.0.0");
    }

    /// An order that is not a list of stages is refused with the file named
    #[test]
    fn config_with_a_bad_order_is_refused() {
        let bad = config(&json!({"group": "g", "name": "p", "order": {"a": 1}}));
        let err = bad.check_stage_tools(Path::new("p.json")).unwrap_err();
        assert!(err.to_string().contains("p.json"), "{err}");
    }

    /// An export writes each stage's tool by name, and the config reads back the same
    #[test]
    fn export_writes_tool_names() {
        let pipeline = Pipeline {
            group: "g".to_owned(),
            name: "p".to_owned(),
            creator: "tester".to_owned(),
            order: vec![vec!["a".to_owned()], vec!["b".to_owned()]],
            sla: 60,
            triggers: HashMap::new(),
            description: None,
            bans: HashMap::new(),
            stage_tools: BTreeMap::from([("a".to_owned(), Uuid::new_v4())]),
        };
        let tools = BTreeMap::from([
            (
                "a".to_owned(),
                StageToolRef {
                    tool: "scan".to_owned(),
                    version: None,
                },
            ),
            (
                "b".to_owned(),
                StageToolRef {
                    tool: "sort".to_owned(),
                    version: Some("1.0.0".to_owned()),
                },
            ),
        ]);
        let exported = PipelineConfig::from(PipelineData { pipeline, tools });
        let text = serde_json::to_string(&exported).unwrap();
        // no id appears in the file
        assert!(text.contains(r#""stage_tools":{"a":{"tool":"scan","version":null},"b":{"tool":"sort","version":"1.0.0"}}"#), "{text}");
        let again: PipelineConfig = serde_json::from_str(&text).unwrap();
        assert_eq!(again.stage_tools, exported.stage_tools);
        assert!(matches!(again.stage_tools["a"], StageToolEntry::Ref(_)));
        again.check_stage_tools(Path::new("p.json")).unwrap();
    }
}
