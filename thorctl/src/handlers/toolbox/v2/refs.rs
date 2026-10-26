//! Symbolic references that stand where a tool spec wants an id
//!
//! A manifest names other things by name so it is portable between Thorium instances. Inside a
//! tool spec the API wants ids, so a spec holds `ref:tool/NAME` (the tool, following its latest)
//! or `ref:tool/NAME@LABEL` (that version's tool instance) in those slots. The slots are listed
//! in [`SLOTS`]; a unit test fails when the spec model gains an id field that isn't listed.

use serde_json::Value;
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

/// The prefix every reference starts with
pub const REF_PREFIX: &str = "ref:";

/// A reference to another item of a toolbox
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Ref {
    /// A tool by name, optionally pinned to a version label
    Tool {
        /// The name of the tool
        name: String,
        /// The version label that is pinned (unset follows the tool's latest)
        label: Option<String>,
    },
    /// A toolset by name
    Toolset(String),
    /// An agent by name
    Agent(String),
}

impl FromStr for Ref {
    /// A malformed reference fails with a message saying why
    type Err = String;

    /// Parse `ref:tool/NAME[@LABEL]`, `ref:toolset/NAME`, or `ref:agent/NAME`
    ///
    /// # Arguments
    ///
    /// * `raw` - The reference text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // every reference starts with the prefix
        let rest = raw
            .strip_prefix(REF_PREFIX)
            .ok_or_else(|| format!("'{raw}' does not start with '{REF_PREFIX}'"))?;
        // split the kind from the rest
        let (kind, target) = rest
            .split_once('/')
            .ok_or_else(|| format!("'{raw}' is not of the form ref:KIND/NAME"))?;
        // a target is a name, plus a label for tools
        let (name, label) = match target.split_once('@') {
            Some((name, label)) => (name, Some(label)),
            None => (target, None),
        };
        // names are checked the way manifest names are
        super::types::check_name(name).map_err(|err| format!("'{raw}': {err}"))?;
        // build the reference of the right kind
        match (kind, label) {
            ("tool", Some(label)) => {
                super::types::check_label(label).map_err(|err| format!("'{raw}': {err}"))?;
                Ok(Ref::Tool {
                    name: name.to_owned(),
                    label: Some(label.to_owned()),
                })
            }
            ("tool", None) => Ok(Ref::Tool {
                name: name.to_owned(),
                label: None,
            }),
            ("toolset", None) => Ok(Ref::Toolset(name.to_owned())),
            ("agent", None) => Ok(Ref::Agent(name.to_owned())),
            ("toolset" | "agent", Some(_)) => {
                Err(format!("'{raw}': only tool references take an @LABEL"))
            }
            (other, _) => Err(format!(
                "'{raw}': unknown reference kind '{other}' (use tool, toolset, or agent)"
            )),
        }
    }
}

impl fmt::Display for Ref {
    /// Write this reference in its text form
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write each kind the way it is parsed
        match self {
            Ref::Tool { name, label: None } => write!(f, "{REF_PREFIX}tool/{name}"),
            Ref::Tool {
                name,
                label: Some(label),
            } => write!(f, "{REF_PREFIX}tool/{name}@{label}"),
            Ref::Toolset(name) => write!(f, "{REF_PREFIX}toolset/{name}"),
            Ref::Agent(name) => write!(f, "{REF_PREFIX}agent/{name}"),
        }
    }
}

/// What kind of id a spec slot takes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// The id of a tool (follows its latest instance), written `ref:tool/NAME`
    ToolId,
    /// The id of a tool instance (a pinned version), written `ref:tool/NAME@LABEL`
    ToolInstanceId,
}

/// A place in a tool spec that holds an id
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    /// The dotted path to the slot, with `*` standing for every element of a list
    pub path: &'static str,
    /// The kind of id the slot holds
    pub kind: SlotKind,
}

/// Every place in a tool spec that holds an id
pub const SLOTS: [Slot; 7] = [
    Slot {
        path: "Image.data.dependencies.results.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Image.data.dependencies.children.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Image.data.dependencies.filesystems.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Harness.workload.data.dependencies.results.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Harness.workload.data.dependencies.children.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Harness.workload.data.dependencies.filesystems.tools.*",
        kind: SlotKind::ToolId,
    },
    Slot {
        path: "Agentic.backing.Container.tool_instance",
        kind: SlotKind::ToolInstanceId,
    },
];

/// What a slot holds
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotValue {
    /// A symbolic reference to resolve
    Ref(Ref),
    /// A literal id (accepted, but it ties the manifest to one Thorium instance)
    Uuid(Uuid),
}

/// A slot of a spec that was found holding a value
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundSlot {
    /// The path to the slot with its list indexes, such as `Image.data.dependencies.results.tools[0]`
    pub path: String,
    /// The kind of id the slot takes
    pub kind: SlotKind,
    /// What the slot holds
    pub value: SlotValue,
}

/// A slot that holds something that is neither an id nor a usable reference
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotProblem {
    /// The path to the slot with its list indexes
    pub path: String,
    /// Why the slot's value can't be used
    pub message: String,
}

/// Read what a single slot holds
///
/// # Arguments
///
/// * `value` - The JSON value in the slot
/// * `kind` - The kind of id the slot takes
fn read_slot(value: &Value, kind: SlotKind) -> Result<SlotValue, String> {
    // a slot holds a string, whether a UUID or a reference
    let Some(text) = value.as_str() else {
        return Err("expected a UUID or a ref: string".to_owned());
    };
    // a literal id is accepted as is
    if let Ok(uuid) = Uuid::parse_str(text) {
        return Ok(SlotValue::Uuid(uuid));
    }
    // anything else must be a reference of the right shape for this slot
    let reference = text.parse::<Ref>().map_err(|err| {
        if text.starts_with(REF_PREFIX) {
            err
        } else {
            format!("'{text}' is neither a UUID nor a ref:tool/NAME reference")
        }
    })?;
    match (&reference, kind) {
        (Ref::Tool { label: None, .. }, SlotKind::ToolId)
        | (Ref::Tool { label: Some(_), .. }, SlotKind::ToolInstanceId) => {
            Ok(SlotValue::Ref(reference))
        }
        (Ref::Tool { label: Some(_), .. }, SlotKind::ToolId) => Err(format!(
            "'{text}' pins a version, but this slot takes a tool; drop the @LABEL"
        )),
        (Ref::Tool { label: None, .. }, SlotKind::ToolInstanceId) => Err(format!(
            "'{text}' names no version, but this slot takes a tool instance; add @LABEL"
        )),
        _ => Err(format!("'{text}' is not a tool reference")),
    }
}

/// Visit every value a slot path reaches in a JSON document
///
/// # Arguments
///
/// * `value` - The document to walk
/// * `segments` - The remaining segments of the slot path
/// * `path` - The concrete path walked so far
/// * `visit` - Called with each reached value and its concrete path
fn walk(value: &mut Value, segments: &[&str], path: &str, visit: &mut dyn FnMut(&mut Value, &str)) {
    // an exhausted path means this value is the slot
    let Some((segment, rest)) = segments.split_first() else {
        visit(value, path);
        return;
    };
    // `*` walks every element of a list; any other segment walks into an object key
    if *segment == "*" {
        if let Value::Array(items) = value {
            for (index, item) in items.iter_mut().enumerate() {
                walk(item, rest, &format!("{path}[{index}]"), visit);
            }
        }
    } else if let Some(child) = value.get_mut(*segment) {
        let child_path = if path.is_empty() {
            (*segment).to_owned()
        } else {
            format!("{path}.{segment}")
        };
        walk(child, rest, &child_path, visit);
    }
}

/// Find everything the slots of a spec hold
///
/// Returns the slots holding a usable id or reference, and the slots holding something else.
///
/// # Arguments
///
/// * `spec` - The JSON tool spec to read
#[must_use]
pub fn find_slots(spec: &Value) -> (Vec<FoundSlot>, Vec<SlotProblem>) {
    // walk a copy so the same walker serves reading and rewriting
    let mut copy = spec.clone();
    let mut found = Vec::new();
    let mut problems = Vec::new();
    // read each slot of the table
    for slot in SLOTS {
        let segments: Vec<&str> = slot.path.split('.').collect();
        walk(
            &mut copy,
            &segments,
            "",
            &mut |value, path| match read_slot(value, slot.kind) {
                Ok(value) => found.push(FoundSlot {
                    path: path.to_owned(),
                    kind: slot.kind,
                    value,
                }),
                Err(message) => problems.push(SlotProblem {
                    path: path.to_owned(),
                    message,
                }),
            },
        );
    }
    (found, problems)
}

/// Replace every reference in a spec with the id the resolver gives it
///
/// Literal ids are left as they are. Slots holding anything else are returned as problems and
/// left untouched.
///
/// # Arguments
///
/// * `spec` - The JSON tool spec to rewrite in place
/// * `resolve` - Maps a reference and the kind of slot it stands in to an id
pub fn resolve_slots(
    spec: &mut Value,
    mut resolve: impl FnMut(&Ref, SlotKind) -> Result<Uuid, String>,
) -> Vec<SlotProblem> {
    let mut problems = Vec::new();
    // rewrite each slot of the table
    for slot in SLOTS {
        let segments: Vec<&str> = slot.path.split('.').collect();
        walk(spec, &segments, "", &mut |value, path| {
            match read_slot(value, slot.kind) {
                // a reference becomes the id the resolver finds for it
                Ok(SlotValue::Ref(reference)) => match resolve(&reference, slot.kind) {
                    Ok(id) => *value = Value::String(id.to_string()),
                    Err(message) => problems.push(SlotProblem {
                        path: path.to_owned(),
                        message,
                    }),
                },
                // a literal id needs no rewriting
                Ok(SlotValue::Uuid(_)) => {}
                Err(message) => problems.push(SlotProblem {
                    path: path.to_owned(),
                    message,
                }),
            }
        });
    }
    problems
}

/// Replace every id in a spec with the reference the mapper gives it (the inverse of
/// [`resolve_slots`], used when exporting)
///
/// Ids the mapper returns `None` for are left as they are.
///
/// # Arguments
///
/// * `spec` - The JSON tool spec to rewrite in place
/// * `to_ref` - Maps an id and the kind of slot it stands in to a reference
pub fn unresolve_slots(spec: &mut Value, mut to_ref: impl FnMut(Uuid, SlotKind) -> Option<Ref>) {
    // rewrite each slot of the table
    for slot in SLOTS {
        let segments: Vec<&str> = slot.path.split('.').collect();
        walk(spec, &segments, "", &mut |value, _path| {
            // only a literal id can be turned into a reference
            let Some(id) = value.as_str().and_then(|text| Uuid::parse_str(text).ok()) else {
                return;
            };
            if let Some(reference) = to_ref(id, slot.kind) {
                *value = Value::String(reference.to_string());
            }
        });
    }
}

/// The concrete paths of every string in a JSON document that is a UUID, with list indexes
/// replaced by `*`
///
/// # Arguments
///
/// * `value` - The document to scan
#[cfg(test)]
#[must_use]
pub fn uuid_paths(value: &Value) -> Vec<String> {
    // collect paths recursively
    let mut paths = Vec::new();
    collect_uuid_paths(value, "", &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

/// Collect the paths of the UUID strings below a value
///
/// # Arguments
///
/// * `value` - The value to scan
/// * `path` - The path of the value
/// * `paths` - The paths collected so far
#[cfg(test)]
fn collect_uuid_paths(value: &Value, path: &str, paths: &mut Vec<String>) {
    // descend into containers and record the strings that are UUIDs
    match value {
        Value::String(text) if Uuid::parse_str(text).is_ok() => paths.push(path.to_owned()),
        Value::Array(items) => {
            for item in items {
                collect_uuid_paths(item, &format!("{path}.*"), paths);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                collect_uuid_paths(child, &child_path, paths);
            }
        }
        _ => {}
    }
}
