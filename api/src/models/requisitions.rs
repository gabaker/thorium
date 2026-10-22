use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use uuid::Uuid;

use super::{Deadline, Pools, Resources, Worker};
#[cfg(feature = "client")]
use crate::Error;
use crate::same;

/// The pod label holding the tool instance a worker claims jobs for
pub const TOOL_INSTANCE_LABEL: &str = "thorium-tool-instance";

/// The pod label holding the run a per-run worker is limited to
pub const RUN_LABEL: &str = "thorium-run";

/// Extract a label or return an error
#[cfg(feature = "client")]
macro_rules! extract_label {
    ($obj:expr, $label:expr) => {
        match $obj.get($label) {
            Some(val) => val.clone(),
            None => return Err(Error::new(format!("Missing label {}", $label))),
        }
    };
}

/// Parse a tool instance or run id from a requisition part
///
/// # Arguments
///
/// * `raw` - The raw id to parse
/// * `kind` - What the id names, for the error message
#[cfg(feature = "client")]
fn parse_id(raw: &str, kind: &str) -> Result<Uuid, Error> {
    // the id must be a valid UUID
    Uuid::parse_str(raw).map_err(|_| Error::new(format!("{kind} '{raw}' is not a valid UUID")))
}

/// A requisition for a pod to be deployed
#[derive(Debug, Hash, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct Requisition {
    /// The user the requisition is for
    pub user: String,
    /// The group the requisition is for
    pub group: String,
    /// The tool instance whose jobs the requested workers claim
    pub tool_instance: Uuid,
    /// The run (reaction) the requested workers are limited to, for jobs of workflow runs and tool
    /// runs (for a harness's tool calls, the parent workflow run)
    #[serde(default)]
    pub run: Option<Uuid>,
}

impl Requisition {
    /// Create a new requisition object
    ///
    /// # Arguments
    ///
    /// * `user` - The user the requisition is for
    /// * `group` - The group the requisition is for
    /// * `tool_instance` - The tool instance whose jobs the requested workers claim
    /// * `run` - The run the requested workers are limited to, if any
    pub fn new<T: Into<String>>(user: T, group: T, tool_instance: Uuid, run: Option<Uuid>) -> Self {
        Requisition {
            user: user.into(),
            group: group.into(),
            tool_instance,
            run,
        }
    }

    /// Scope this requisition to a specific node
    ///
    /// # Arguments
    ///
    /// * `node` - The name of the node this req is scoped too
    pub fn to_scoped<T: Into<String>>(self, node: T) -> ScopedRequisition {
        ScopedRequisition {
            node: node.into(),
            req: self,
        }
    }

    /// Build the pod labels that identify this requisition
    ///
    /// The run label is only set for per-run requisitions.
    #[must_use]
    pub fn labels(&self) -> BTreeMap<String, String> {
        // every requisition is labeled with its user, group, and tool instance
        let mut labels = BTreeMap::default();
        labels.insert("user".to_owned(), self.user.clone());
        labels.insert("group".to_owned(), self.group.clone());
        labels.insert(
            TOOL_INSTANCE_LABEL.to_owned(),
            self.tool_instance.to_string(),
        );
        // only per-run requisitions carry their run
        if let Some(run) = &self.run {
            labels.insert(RUN_LABEL.to_owned(), run.to_string());
        }
        labels
    }

    /// Create a requisiton from labels on a pod
    ///
    /// This is used to compare currently living pods with pods that we have scaled this round.
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels from a pod in k8s
    #[cfg(feature = "client")]
    pub fn from_labels(labels: &BTreeMap<String, String>) -> Result<Requisition, Error> {
        // extract labels
        let user = extract_label!(labels, "user");
        let group = extract_label!(labels, "group");
        let tool_instance = extract_label!(labels, TOOL_INSTANCE_LABEL);
        // parse the tool instance and the optional run
        let tool_instance = parse_id(&tool_instance, "tool instance")?;
        let run = labels
            .get(RUN_LABEL)
            .map(|run| parse_id(run, "run"))
            .transpose()?;
        // cast to a requisition
        let req = Requisition {
            user,
            group,
            tool_instance,
            run,
        };
        Ok(req)
    }

    /// Parse the colon joined parts of a requisition
    ///
    /// The parts are `user:group:tool_instance` with an optional trailing `:run:<run>`.
    ///
    /// # Arguments
    ///
    /// * `parts` - The parts to parse
    /// * `raw` - The full string being parsed, for error messages
    #[cfg(feature = "client")]
    fn from_parts(parts: &[&str], raw: &str) -> Result<Self, Error> {
        // get the run if this is a per-run requisition
        let run = match parts.len() {
            3 => None,
            5 if parts[3] == "run" => Some(parse_id(parts[4], "run")?),
            _ => {
                return Err(Error::new(format!(
                    "{raw} can not be cast to a requisition!"
                )));
            }
        };
        // build our requisition
        let req = Requisition {
            user: parts[0].to_owned(),
            group: parts[1].to_owned(),
            tool_instance: parse_id(parts[2], "tool instance")?,
            run,
        };
        Ok(req)
    }
}

impl fmt::Display for Requisition {
    /// Cleanly print a requisition
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // write the user, group, and tool instance
        write!(f, "{}:{}:{}", self.user, self.group, self.tool_instance)?;
        // write the run for per-run requisitions
        if let Some(run) = &self.run {
            write!(f, ":run:{run}")?;
        }
        Ok(())
    }
}

#[cfg(feature = "client")]
impl TryFrom<&String> for Requisition {
    type Error = Error;

    /// Try to convert a colon joined string to a requisition
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw string to convert from
    fn try_from(raw: &String) -> Result<Self, Error> {
        // split this string by ':' into its seperate parts
        let parts = raw.split(':').collect::<Vec<_>>();
        Self::from_parts(&parts, raw)
    }
}

impl From<Deadline> for Requisition {
    /// Build the requisition a deadline needs a worker for
    ///
    /// # Arguments
    ///
    /// * `deadline` - The deadline to build a requisition for
    fn from(deadline: Deadline) -> Self {
        Requisition {
            user: deadline.creator,
            group: deadline.group,
            tool_instance: deadline.tool_instance,
            run: deadline.run,
        }
    }
}

impl From<&Deadline> for Requisition {
    /// Build the requisition a deadline needs a worker for
    ///
    /// # Arguments
    ///
    /// * `deadline` - The deadline to build a requisition for
    fn from(deadline: &Deadline) -> Self {
        Requisition {
            user: deadline.creator.clone(),
            group: deadline.group.clone(),
            tool_instance: deadline.tool_instance,
            run: deadline.run,
        }
    }
}

impl From<ScopedRequisition> for Requisition {
    /// Drop the node from a scoped requisition
    ///
    /// # Arguments
    ///
    /// * `scoped` - The scoped requisition to convert
    fn from(scoped: ScopedRequisition) -> Self {
        scoped.req
    }
}

impl From<&ScopedRequisition> for Requisition {
    /// Drop the node from a scoped requisition
    ///
    /// # Arguments
    ///
    /// * `scoped` - The scoped requisition to convert
    fn from(scoped: &ScopedRequisition) -> Self {
        scoped.req.clone()
    }
}

/// Extract a value from a hashmap or throw a generic error
#[cfg(feature = "client")]
macro_rules! extract_generic {
    ($map:expr, $key:expr) => {
        match $map.remove($key) {
            Some(val) => val,
            None => return Err(Error::new(format!("HashMap missing value {}", $key))),
        }
    };
}

#[cfg(feature = "client")]
impl TryFrom<BTreeMap<String, String>> for Requisition {
    type Error = Error;

    /// Try to cast a label map into a requisition
    ///
    /// # Arguments
    ///
    /// * `map` - The label map to cast
    fn try_from(mut map: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        // get the tool instance and the optional run
        let tool_instance = parse_id(&extract_generic!(map, TOOL_INSTANCE_LABEL), "tool instance")?;
        let run = map
            .remove(RUN_LABEL)
            .map(|run| parse_id(&run, "run"))
            .transpose()?;
        let req = Requisition {
            user: extract_generic!(map, "user"),
            group: extract_generic!(map, "group"),
            tool_instance,
            run,
        };
        Ok(req)
    }
}

impl PartialEq<BTreeMap<String, String>> for Requisition {
    /// Check if a pod's labels belong to this requisition
    ///
    /// # Arguments
    ///
    /// * `labels` - The labels to compare against
    fn eq(&self, labels: &BTreeMap<String, String>) -> bool {
        // if any of the values don't match then return false
        same!(Some(&self.user), labels.get("user"));
        same!(Some(&self.group), labels.get("group"));
        same!(
            Some(self.tool_instance.to_string()),
            labels.get(TOOL_INSTANCE_LABEL).cloned()
        );
        // the run label must match exactly, including being absent for shared requisitions
        same!(
            self.run.map(|run| run.to_string()),
            labels.get(RUN_LABEL).cloned()
        );
        true
    }
}

/// A requisition for a pod to be deployed to a specific node
#[derive(Debug, Hash, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub struct ScopedRequisition {
    /// The node this requisition is being handled by
    pub node: String,
    /// The requisition being spawned on this node
    pub req: Requisition,
}

impl ScopedRequisition {
    /// Create a new scoped requisition object
    ///
    /// # Arguments
    ///
    /// * `node` - The node this requisition is being handled by
    /// * `req` - The requisition being spawned on this node
    pub fn new<T: Into<String>>(node: T, req: Requisition) -> Self {
        ScopedRequisition {
            node: node.into(),
            req,
        }
    }
}

impl fmt::Display for ScopedRequisition {
    /// Cleanly print a requisition
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}:{}", self.node, self.req)
    }
}

#[cfg(feature = "client")]
impl TryFrom<&String> for ScopedRequisition {
    type Error = Error;

    /// Try to convert a string to a scoped requisition
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw string to convert from
    fn try_from(raw: &String) -> Result<Self, Error> {
        // split this string by ':' into its seperate parts
        let chunks = raw.split(':').collect::<Vec<_>>();
        // a scoped requisition needs a node in front of its requisition
        if chunks.len() < 2 {
            return Err(Error::new(format!(
                "{raw} can not be cast to a scoped requisition!",
            )));
        }
        // parse the requisition after the node
        let req = Requisition::from_parts(&chunks[1..], raw)?;
        Ok(ScopedRequisition {
            node: chunks[0].to_owned(),
            req,
        })
    }
}

// Updated info on a spawned resources on a specific cluster
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnedUpdate {
    /// The req that was tied to this pod
    pub req: Requisition,
    /// The node this req was spawned on
    pub node: String,
    /// The unique name for this resource
    pub name: String,
    /// The pool this worker was spawned in
    pub pool: Pools,
    /// The resources in use by this spawned resource
    pub resources: Resources,
    /// Whether this resource has been told to scale down yet or not
    pub scaled_down: bool,
}

impl From<Worker> for SpawnedUpdate {
    /// cast our worker to a spawned update
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to build a spawned update for
    fn from(worker: Worker) -> Self {
        // build the req that spawned this worker
        let req = Requisition::new(worker.user, worker.group, worker.tool_instance, worker.run);
        SpawnedUpdate {
            req,
            node: worker.node,
            name: worker.name,
            pool: worker.pool,
            resources: worker.resources,
            scaled_down: false,
        }
    }
}

#[cfg(test)]
mod tests;
