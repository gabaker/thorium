use std::collections::BTreeMap;
use thorium::models::runtime::DEFAULT_PROJECTED_RUNTIME;
use thorium::models::{
    EntityKinds, EntityMetadata, HarnessInterface, Image, OrphanedRunPolicy, Pools, ScrubbedUser,
    ToolSpec, ToolVersion, WorkerDeleteMap, WorkerStatus, WorkerUpdate, WorkloadSpec,
};
use thorium::{Error, Thorium};
use tracing::instrument;
use uuid::Uuid;

use crate::args::Args;

/// How a Harness tool instance is launched and cleaned up after
#[derive(Debug, Clone, Default)]
pub struct Harness {
    /// Where the harness reads its inputs and writes its outputs, and how it's launched
    pub interface: HarnessInterface,
    /// What happens to tool runs the harness started but never collected
    pub orphaned_runs: OrphanedRunPolicy,
}

/// The tool instance a worker runs, read once when the worker starts
#[derive(Debug, Clone)]
pub struct Workload {
    /// The name of the tool this is an instance of
    pub tool: String,
    /// The version of the tool instance
    pub version: ToolVersion,
    /// The workload the tool instance runs
    pub spec: WorkloadSpec,
    /// How the harness is launched and cleaned up after, when the tool instance is a Harness
    pub harness: Option<Harness>,
    /// The current names of the tools the workload's dependency settings name, by id
    pub dependency_names: BTreeMap<Uuid, String>,
}

/// Get the current names of the tools a workload's dependency settings name
///
/// A tool that no longer exists, can't be seen, or isn't a tool is named by its id, which no
/// tool's output carries, so its restriction never widens to other tools.
///
/// # Arguments
///
/// * `thorium` - The client to read the tools with
/// * `spec` - The workload whose dependency settings name the tools
async fn dependency_names(
    thorium: &Thorium,
    spec: &WorkloadSpec,
) -> Result<BTreeMap<Uuid, String>, Error> {
    // read each listed tool once
    let mut names = BTreeMap::new();
    for id in spec.data.dependencies.tools() {
        let name = match thorium.entities.get(id).await {
            // a tool is named by its current name
            Ok(entity) if entity.kind == EntityKinds::Tool => entity.name,
            // an entity of another kind keeps its id as its name
            Ok(_) => id.to_string(),
            // so does a tool that's gone or hidden
            Err(error) if matches!(error.status().map(|code| code.as_u16()), Some(401 | 404)) => {
                id.to_string()
            }
            // any other failure is a failure to start
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to get dependency tool {id}: {}",
                    error.msg().unwrap_or_else(|| error.to_string())
                )));
            }
        };
        names.insert(id, name);
    }
    Ok(names)
}

impl Workload {
    /// Fetch a tool instance and the workload it runs
    ///
    /// # Arguments
    ///
    /// * `thorium` - The client to fetch the tool instance with
    /// * `tool_instance` - The id of the tool instance
    #[instrument(name = "Workload::fetch", skip(thorium), err(Debug))]
    pub async fn fetch(thorium: &Thorium, tool_instance: Uuid) -> Result<Self, Error> {
        // get the tool instance, naming it in any error
        let entity = match thorium.entities.get(tool_instance).await {
            Ok(entity) => entity,
            Err(error) => {
                return Err(Error::new(format!(
                    "Failed to get tool instance {tool_instance}: {}",
                    error.msg().unwrap_or_else(|| error.to_string())
                )));
            }
        };
        // only a tool instance runs a workload
        let EntityMetadata::ToolInstance(instance) = entity.metadata else {
            return Err(Error::new(format!(
                "{tool_instance} is a {}, not a tool instance",
                entity.kind
            )));
        };
        // instances are named after their tool and version
        let tool = entity
            .name
            .strip_suffix(&format!("@{}", instance.version))
            .unwrap_or(&entity.name)
            .to_owned();
        // get the workload of an Image or Harness tool instance
        let (spec, harness) = match instance.spec {
            ToolSpec::Image(spec) => (spec, None),
            ToolSpec::Harness(harness) => (
                harness.workload,
                Some(Harness {
                    interface: harness.interface,
                    orphaned_runs: harness.orphaned_runs,
                }),
            ),
            ToolSpec::Agentic(_) => {
                return Err(Error::new(format!(
                    "tool instance {tool_instance} is an Agentic tool instance, which has no workload to run"
                )));
            }
        };
        // get the current names of the tools its dependency settings name
        let dependency_names = dependency_names(thorium, &spec).await?;
        Ok(Workload {
            tool,
            version: instance.version,
            spec,
            harness,
            dependency_names,
        })
    }

    /// Project this workload into the in-memory image the job runner reads
    ///
    /// The image's dependency lists hold the current names of the tools the spec lists, which
    /// the results and children filters match.
    ///
    /// # Arguments
    ///
    /// * `group` - The group this worker claims jobs in
    #[must_use]
    pub fn project(&self, group: &str) -> Image {
        // the projection is named after the tool, so results are stored under its name
        let mut image = self
            .spec
            .project_image(group, &self.tool, DEFAULT_PROJECTED_RUNTIME);
        // name each listed tool by its current name, or its id if it couldn't be read
        let name = |id: &Uuid| {
            self.dependency_names
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_string())
        };
        let deps = &self.spec.data.dependencies;
        image.dependencies.results.images = deps.results.tools.iter().map(name).collect();
        image.dependencies.children.images = deps.children.tools.iter().map(name).collect();
        image.dependencies.filesystems.images = deps.filesystems.tools.iter().map(name).collect();
        image
    }
}

/// The tool instance to claim and run jobs for
pub struct Target {
    /// The name of this worker
    pub name: String,
    /// The group to claim jobs in
    pub group: String,
    /// The tool instance to claim jobs for
    pub tool_instance: Uuid,
    /// The tool instance this worker runs
    pub workload: Workload,
    /// The in-memory projection of the tool instance's workload
    pub image: Image,
    /// The user this target is for
    pub user: ScrubbedUser,
    /// The client to use for this image
    pub thorium: Thorium,
    /// What pool of resources this worker was spawned under
    pub pool: Pools,
    /// The run this worker serves, when it's a per-run worker
    pub run: Option<Uuid>,
}

impl Target {
    /// Tell Thorium this target is now running jobs
    ///
    /// # Arguments
    ///
    /// * `args` - The args used to spawn this agent
    /// * `status` - The status to set
    #[instrument(name = "Target::update_worker", skip(self))]
    pub async fn update_worker(&self, status: WorkerStatus) -> Result<(), Error> {
        // build the update for this worker
        let update = WorkerUpdate::new(status);
        // tell Thorium we are now running
        self.thorium
            .system
            .update_worker(&self.name, &update)
            .await?;
        Ok(())
    }

    /// Removes a no longer active worker
    ///
    /// # Arguments
    ///
    /// * `args` - The args used to spawn this agent
    pub async fn remove_worker(&self, args: &Args) -> Result<(), Error> {
        // get the scaler this worker is running under
        let scaler = args.scaler();
        // build the worker delete map
        let deletes = WorkerDeleteMap::default().add(&self.name);
        // tell Thorium we are now running
        self.thorium.system.delete_workers(scaler, &deletes).await?;
        Ok(())
    }
}

impl std::fmt::Display for Target {
    // This trait requires `fmt` with this exact signature.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        // write this target to the formatter
        write!(f, "{}:{} - {}", self.group, self.tool_instance, self.name)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use uuid::Uuid;

    /// A projection names each listed tool by its current name, or by its id when the tool
    /// couldn't be read
    #[test]
    fn projections_name_listed_tools() {
        // a workload listing a named tool and an unreadable one
        let named = Uuid::new_v4();
        let unreadable = Uuid::new_v4();
        let mut workload = crate::libs::test_server::workload(None);
        let deps = &mut workload.spec.data.dependencies;
        deps.results.tools = vec![named, unreadable];
        deps.children.tools = vec![named];
        deps.filesystems.tools = vec![unreadable];
        workload.dependency_names = BTreeMap::from([(named, "strings".to_owned())]);
        // the projection's lists hold the names in the listed order
        let image = workload.project("corn");
        assert_eq!(
            image.dependencies.results.images,
            vec!["strings".to_owned(), unreadable.to_string()]
        );
        assert_eq!(
            image.dependencies.children.images,
            vec!["strings".to_owned()]
        );
        assert_eq!(
            image.dependencies.filesystems.images,
            vec![unreadable.to_string()]
        );
    }
}
