//! Content digests for Thorium's versioned definitions
//!
//! A digest is `"v1:" + hex(sha256(jcs(digest_value(input))))`: the typed value after
//! deserialization and defaulting is turned into a default-pruned JSON value
//! ([`DigestValue`]), tagged with its type (`{"t": "<type>", ...}`), written as RFC 8785
//! canonical JSON, and hashed with SHA-256.

use serde::ser::Error as _;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::hash::BuildHasher;
use uuid::Uuid;

use super::models::ModelMatcher;
use super::tools::{ToolMechanism, ToolSpec};
use super::toolsets::{ToolSetInclusion, ToolSetSelector};
use super::workflows::WorkflowLimits;

#[cfg(test)]
mod tests;

/// Turns a typed value into the default-pruned JSON value Thorium hashes
///
/// Derive it with `#[derive(thorium_derive::DigestValue)]`, which leaves out every struct field
/// whose digest value equals the value serde fills in when the field is absent.
pub trait DigestValue {
    /// Build this value's default-pruned digest value
    fn digest_value(&self) -> Value;
}

/// Implement [`DigestValue`] for scalar types through their JSON form
macro_rules! digest_value_from {
    ($($ty:ty),*) => {
        $(
            impl DigestValue for $ty {
                /// Build this scalar's digest value
                fn digest_value(&self) -> Value {
                    // scalars digest as their JSON form
                    Value::from(*self)
                }
            }
        )*
    };
}

digest_value_from!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, bool);

impl DigestValue for String {
    /// Build this string's digest value
    fn digest_value(&self) -> Value {
        // strings digest as themselves
        Value::String(self.clone())
    }
}

impl DigestValue for Uuid {
    /// Build this UUID's digest value in its hyphenated serde form
    fn digest_value(&self) -> Value {
        // UUIDs digest as their hyphenated string
        Value::String(self.to_string())
    }
}

impl<T: DigestValue> DigestValue for Option<T> {
    /// Build this option's digest value: `null` for `None`, else the inner value's
    fn digest_value(&self) -> Value {
        // digest the inner value if there is one
        self.as_ref().map_or(Value::Null, DigestValue::digest_value)
    }
}

impl<T: DigestValue + ?Sized> DigestValue for Box<T> {
    /// Build the boxed value's digest value
    fn digest_value(&self) -> Value {
        // digest the boxed value
        (**self).digest_value()
    }
}

impl<T: DigestValue> DigestValue for Vec<T> {
    /// Build an array in element order, keeping every element and pruning only its contents
    fn digest_value(&self) -> Value {
        // digest every element in order
        Value::Array(self.iter().map(DigestValue::digest_value).collect())
    }
}

impl<T: DigestValue> DigestValue for BTreeSet<T> {
    /// Build an array of this set's element digests sorted by their canonical JSON
    ///
    /// Sorting by content rather than by `T`'s `Ord` keeps a hash stable when an element enum's
    /// variants are reordered or one is inserted, since a derived `Ord` follows declaration order.
    fn digest_value(&self) -> Value {
        // pair every element's digest with its canonical bytes
        let mut keyed = self
            .iter()
            .map(|item| {
                let value = item.digest_value();
                // digest values hold no floats that canonicalization could refuse, so the
                // plain JSON text is only a fallback that keeps the order total
                let key = ThorDigest::canonical_bytes(&value)
                    .unwrap_or_else(|_| value.to_string().into_bytes());
                (key, value)
            })
            .collect::<Vec<(Vec<u8>, Value)>>();
        // sort by the canonical bytes and drop elements that digest the same
        keyed.sort_by(|left, right| left.0.cmp(&right.0));
        keyed.dedup_by(|left, right| left.0 == right.0);
        Value::Array(keyed.into_iter().map(|(_, value)| value).collect())
    }
}

impl<T: DigestValue> DigestValue for BTreeMap<String, T> {
    /// Build an object keeping every entry and pruning only each value's contents
    fn digest_value(&self) -> Value {
        // digest every value under its key
        Value::Object(
            self.iter()
                .map(|(key, value)| (key.clone(), value.digest_value()))
                .collect(),
        )
    }
}

impl<T: DigestValue, S: BuildHasher> DigestValue for HashMap<String, T, S> {
    /// Build an object keeping every entry and pruning only each value's contents
    ///
    /// Canonicalization sorts the keys, so iteration order never matters.
    fn digest_value(&self) -> Value {
        // digest every value under its key
        Value::Object(
            self.iter()
                .map(|(key, value)| (key.clone(), value.digest_value()))
                .collect(),
        )
    }
}

impl DigestValue for Value {
    /// Opaque user JSON is never pruned inside, only canonicalized
    fn digest_value(&self) -> Value {
        // keep opaque JSON as it is
        self.clone()
    }
}

impl DigestValue for Map<String, Value> {
    /// An opaque user JSON object is never pruned inside, only canonicalized
    fn digest_value(&self) -> Value {
        // keep the opaque object as it is
        Value::Object(self.clone())
    }
}

/// The type tag that separates the digests of different kinds of input
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestTag {
    /// A Tool Instance's `spec_hash`
    ToolSpec,
    /// An Agent's `config_hash`
    AgentConfig,
    /// A `ToolSet` Instance's digest
    ToolSetInstance,
    /// An instanced agent's toolset digest
    AgentToolset,
    /// A Workflow Instance's digest
    WorkflowInstance,
}

impl DigestTag {
    /// Get the `t` value of this tag
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DigestTag::ToolSpec => "tool_spec",
            DigestTag::AgentConfig => "agent_config",
            DigestTag::ToolSetInstance => "toolset_instance",
            DigestTag::AgentToolset => "agent_toolset",
            DigestTag::WorkflowInstance => "workflow_instance",
        }
    }
}

/// The input of a tool instance's `spec_hash`
#[derive(Debug, Default, thorium_derive::DigestValue)]
struct ToolSpecDigest {
    /// The name of the tool's kind
    kind: String,
    /// The pruned digest value of the spec, with a pinned instance replaced by its `spec_hash`
    spec: Value,
    /// The sorted `spec_hash`es of the native tool instances a harness instance provides
    provides: Vec<String>,
}

/// One member of a toolset instance as the instance's digest sees it
#[derive(Debug, Default, thorium_derive::DigestValue)]
struct ToolSetMemberDigest {
    /// The member's `spec_hash`
    spec_hash: String,
    /// The name of how the member was included
    inclusion: String,
}

/// The input of a toolset instance's digest
#[derive(Debug, Default, thorium_derive::DigestValue)]
struct ToolSetInstanceDigest {
    /// Whether the read only filter was on
    read_only: bool,
    /// The toolset's tag selector
    selector: Option<ToolSetSelector>,
    /// The members' spec hashes and inclusions, sorted
    members: Vec<ToolSetMemberDigest>,
}

/// The input of an instanced agent's toolset digest
#[derive(Debug, Default, thorium_derive::DigestValue)]
struct AgentToolsetDigest {
    /// The spec hashes of the agent's effective tools, sorted
    tools: Vec<String>,
}

/// One agent of a workflow instance as the workflow digest sees it
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, thorium_derive::DigestValue)]
struct WorkflowAgentDigest {
    /// The agent's config hash
    config_hash: String,
    /// The spec hash of the agent's harness instance
    harness: String,
    /// The spec hashes of the agent's tools, sorted
    tools: Vec<String>,
    /// The digests of the agent's toolset instances, sorted
    tool_sets: Vec<String>,
    /// The agent's tool bindings as tool spec hashes and mechanism names, sorted
    bindings: Vec<WorkflowBindingDigest>,
}

/// One tool binding of an agent as the workflow digest sees it
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, thorium_derive::DigestValue)]
struct WorkflowBindingDigest {
    /// The spec hash of the bound tool
    spec_hash: String,
    /// The name of the mechanism it's bound to
    mechanism: String,
}

/// One delegation in a workflow instance as the workflow digest sees it
#[derive(Debug, Default, PartialEq, Eq, PartialOrd, Ord, thorium_derive::DigestValue)]
struct WorkflowDelegationDigest {
    /// The config hash of the delegating agent
    source: String,
    /// The config hash of the agent delegated to
    target: String,
}

/// The input of a workflow instance's digest
#[derive(Debug, Default, thorium_derive::DigestValue)]
struct WorkflowInstanceDigest {
    /// Every agent by its content, sorted, so identical agents count once each
    agents: Vec<WorkflowAgentDigest>,
    /// Every delegation by its agents' config hashes, sorted
    delegations: Vec<WorkflowDelegationDigest>,
    /// The entry agent's config hash
    entry_agent: String,
    /// The spec hashes of the tools that are only dependencies, sorted
    dependencies: Vec<String>,
    /// The workflow's run-wide limits
    limits: WorkflowLimits,
    /// The workflow's default models, in priority order
    default_models: Vec<ModelMatcher>,
}

/// One agent of a workflow instance, described by the hashes of what it uses
#[derive(Debug, Clone, Default)]
pub struct WorkflowAgentNode<'a> {
    /// The agent's config hash
    pub config_hash: &'a str,
    /// The spec hash of the agent's harness instance, if it has one
    pub harness: Option<&'a str>,
    /// The spec hashes of the agent's tools, in any order
    pub tools: Vec<&'a str>,
    /// The digests of the agent's toolset instances, in any order
    pub tool_sets: Vec<&'a str>,
    /// The spec hash of each tool the agent's harness calls and the mechanism it calls it with,
    /// in any order
    pub bindings: Vec<(&'a str, ToolMechanism)>,
}

/// Everything a workflow instance's digest covers, described by hashes
#[derive(Debug, Clone, Copy)]
pub struct WorkflowDigestInput<'a> {
    /// Each agent, described by the hashes of what it uses
    pub agents: &'a [WorkflowAgentNode<'a>],
    /// Each delegation as the config hashes of its source and target
    pub delegations: &'a [(&'a str, &'a str)],
    /// The entry agent's config hash, if the entry agent is in the instance
    pub entry_agent: Option<&'a str>,
    /// The spec hashes of the tools that are only dependencies
    pub dependencies: &'a [&'a str],
    /// The workflow's run-wide limits
    pub limits: &'a WorkflowLimits,
    /// The workflow's default models, in priority order
    pub default_models: &'a [ModelMatcher],
}

/// Copy and sort a list of hashes
///
/// # Arguments
///
/// * `hashes` - The hashes to sort
fn sorted_hashes(hashes: &[&str]) -> Vec<String> {
    // own and sort the hashes
    let mut sorted = hashes
        .iter()
        .map(|hash| (*hash).to_owned())
        .collect::<Vec<String>>();
    sorted.sort();
    sorted
}

/// Describe and sort an agent's tool bindings by spec hash and mechanism name
///
/// # Arguments
///
/// * `bindings` - Each bound tool's spec hash and mechanism
fn sorted_bindings(bindings: &[(&str, ToolMechanism)]) -> Vec<WorkflowBindingDigest> {
    // name each mechanism and sort the pairs
    let mut sorted = bindings
        .iter()
        .map(|(spec_hash, mechanism)| WorkflowBindingDigest {
            spec_hash: (*spec_hash).to_owned(),
            mechanism: mechanism.as_str().to_owned(),
        })
        .collect::<Vec<WorkflowBindingDigest>>();
    sorted.sort();
    sorted
}

/// Computes and compares Thorium's content digests
pub struct ThorDigest;

impl ThorDigest {
    /// The prefix of the current digest scheme
    pub const SCHEME: &'static str = "v1:";

    /// Build the tagged digest value of an input: its pruned object with a `t` key
    ///
    /// # Arguments
    ///
    /// * `tag` - The input's type tag
    /// * `input` - The input to tag
    pub fn tagged_value<T: DigestValue + ?Sized>(
        tag: DigestTag,
        input: &T,
    ) -> Result<Value, serde_json::Error> {
        // digest inputs are structs, so their values are objects
        let Value::Object(mut map) = input.digest_value() else {
            return Err(serde_json::Error::custom("a digest input must be a struct"));
        };
        // the tag key can't be shadowed by a field
        if map.contains_key("t") {
            return Err(serde_json::Error::custom(
                "a digest input can't have a field named t",
            ));
        }
        // add the type tag
        map.insert("t".to_owned(), Value::String(tag.as_str().to_owned()));
        Ok(Value::Object(map))
    }

    /// Write a value as RFC 8785 canonical JSON
    ///
    /// # Arguments
    ///
    /// * `value` - The value to canonicalize
    pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
        // write the value with RFC 8785
        serde_json_canonicalizer::to_vec(value)
    }

    /// Hash a value's canonical JSON under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to add
    /// * `value` - The value to hash
    fn hash_with_scheme(scheme: &str, value: &Value) -> Result<String, serde_json::Error> {
        // canonicalize and hash the value
        let hash = Sha256::digest(Self::canonical_bytes(value)?);
        // write the prefix and the lowercase hex digest
        let mut digest = String::with_capacity(scheme.len() + hash.len() * 2);
        digest.push_str(scheme);
        for byte in hash {
            // writing to a string can't fail
            let _ = write!(digest, "{byte:02x}");
        }
        Ok(digest)
    }

    /// Hash an already tagged value under the current scheme
    ///
    /// # Arguments
    ///
    /// * `value` - The value to hash
    pub fn of_canonical(value: &Value) -> Result<String, serde_json::Error> {
        // hash under the current scheme
        Self::hash_with_scheme(Self::SCHEME, value)
    }

    /// Compute the digest of an input under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    /// * `tag` - The input's type tag
    /// * `input` - The input to digest
    pub(crate) fn compute_with_scheme<T: DigestValue + ?Sized>(
        scheme: &str,
        tag: DigestTag,
        input: &T,
    ) -> Result<String, serde_json::Error> {
        // tag the input and hash it
        let tagged = Self::tagged_value(tag, input)?;
        Self::hash_with_scheme(scheme, &tagged)
    }

    /// Compute the digest of an input under the current scheme
    ///
    /// # Arguments
    ///
    /// * `tag` - The input's type tag
    /// * `input` - The input to digest
    pub fn compute<T: DigestValue + ?Sized>(
        tag: DigestTag,
        input: &T,
    ) -> Result<String, serde_json::Error> {
        // digest under the current scheme
        Self::compute_with_scheme(Self::SCHEME, tag, input)
    }

    /// Build the tagged digest value of a tool spec
    ///
    /// A container backing contributes the `spec_hash` of the Image instance it pins instead of
    /// the instance's ID, so equal definitions hash equally whichever instance they pin.
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to build a digest value for
    /// * `pinned_spec_hash` - The `spec_hash` of the instance a container backing pins
    pub fn tool_spec_value(
        spec: &ToolSpec,
        pinned_spec_hash: Option<&str>,
    ) -> Result<Value, serde_json::Error> {
        // a tool that provides no native tools hashes as if the field didn't exist
        Self::provided_tool_spec_value(spec, pinned_spec_hash, &[])
    }

    /// Build the tagged digest value of a tool spec and the native tools its instance provides
    ///
    /// The native tools are described by their sorted `spec_hash`es, which are pruned when there
    /// are none.
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to build a digest value for
    /// * `pinned_spec_hash` - The `spec_hash` of the instance a container backing pins
    /// * `provides` - The `spec_hash`es of the native tool instances a harness instance provides
    pub fn provided_tool_spec_value(
        spec: &ToolSpec,
        pinned_spec_hash: Option<&str>,
        provides: &[&str],
    ) -> Result<Value, serde_json::Error> {
        // get the pruned digest value of the spec
        let mut value = spec.digest_value();
        // swap a pinned instance's ID for its spec hash
        match (spec.pinned_instance(), pinned_spec_hash) {
            (Some(_), Some(pinned)) => {
                // find the container backing, which is never pruned since it isn't the default
                let backing = value
                    .pointer_mut("/Agentic/backing/Container")
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| serde_json::Error::custom("a container backing is missing"))?;
                // put the pinned instance's spec hash where its id was
                backing.insert("tool_instance".to_owned(), Value::String(pinned.to_owned()));
            }
            (Some(_), None) => {
                return Err(serde_json::Error::custom(
                    "a container backing needs its pinned instance's spec hash",
                ));
            }
            (None, Some(_)) => {
                return Err(serde_json::Error::custom(
                    "only a container backing pins an instance",
                ));
            }
            (None, None) => (),
        }
        // build the tagged input from the kind name and the spec
        let input = ToolSpecDigest {
            kind: spec.kind().to_string(),
            spec: value,
            provides: sorted_hashes(provides),
        };
        Self::tagged_value(DigestTag::ToolSpec, &input)
    }

    /// Compute a tool spec's `spec_hash` under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    /// * `spec` - The spec to hash
    /// * `pinned_spec_hash` - The `spec_hash` of the instance a container backing pins
    pub(crate) fn tool_spec_with_scheme(
        scheme: &str,
        spec: &ToolSpec,
        pinned_spec_hash: Option<&str>,
    ) -> Result<String, serde_json::Error> {
        // build the tagged value and hash it
        let tagged = Self::tool_spec_value(spec, pinned_spec_hash)?;
        Self::hash_with_scheme(scheme, &tagged)
    }

    /// Compute a tool spec's `spec_hash` under the current scheme
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to hash
    /// * `pinned_spec_hash` - The `spec_hash` of the instance a container backing pins
    pub fn tool_spec(
        spec: &ToolSpec,
        pinned_spec_hash: Option<&str>,
    ) -> Result<String, serde_json::Error> {
        // hash under the current scheme
        Self::tool_spec_with_scheme(Self::SCHEME, spec, pinned_spec_hash)
    }

    /// Compute the `spec_hash` of a tool spec and the native tools its instance provides under
    /// the current scheme
    ///
    /// # Arguments
    ///
    /// * `spec` - The spec to hash
    /// * `pinned_spec_hash` - The `spec_hash` of the instance a container backing pins
    /// * `provides` - The `spec_hash`es of the native tool instances a harness instance provides
    pub fn provided_tool_spec(
        spec: &ToolSpec,
        pinned_spec_hash: Option<&str>,
        provides: &[&str],
    ) -> Result<String, serde_json::Error> {
        // build the tagged value and hash it under the current scheme
        let tagged = Self::provided_tool_spec_value(spec, pinned_spec_hash, provides)?;
        Self::hash_with_scheme(Self::SCHEME, &tagged)
    }

    /// Build the tagged digest value of a toolset instance
    ///
    /// Members are described by their spec hash and inclusion name and sorted, so neither their
    /// ids nor their order changes the value.
    ///
    /// # Arguments
    ///
    /// * `read_only` - Whether the read only filter was on
    /// * `selector` - The toolset's tag selector
    /// * `members` - Each member's spec hash and inclusion
    pub fn toolset_instance_value(
        read_only: bool,
        selector: Option<&ToolSetSelector>,
        members: &[(&str, ToolSetInclusion)],
    ) -> Result<Value, serde_json::Error> {
        // sort the members by their hash and then their inclusion's name
        let mut sorted = members
            .iter()
            .map(|(spec_hash, inclusion)| ToolSetMemberDigest {
                spec_hash: (*spec_hash).to_owned(),
                inclusion: inclusion.as_str().to_owned(),
            })
            .collect::<Vec<ToolSetMemberDigest>>();
        sorted.sort_by(|left, right| {
            (&left.spec_hash, &left.inclusion).cmp(&(&right.spec_hash, &right.inclusion))
        });
        // tag the settings and the sorted members as a toolset instance
        let input = ToolSetInstanceDigest {
            read_only,
            selector: selector.cloned(),
            members: sorted,
        };
        Self::tagged_value(DigestTag::ToolSetInstance, &input)
    }

    /// Compute a toolset instance's digest under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    /// * `read_only` - Whether the read only filter was on
    /// * `selector` - The toolset's tag selector
    /// * `members` - Each member's spec hash and inclusion
    pub(crate) fn toolset_instance_with_scheme(
        scheme: &str,
        read_only: bool,
        selector: Option<&ToolSetSelector>,
        members: &[(&str, ToolSetInclusion)],
    ) -> Result<String, serde_json::Error> {
        // build the tagged value and hash it
        let tagged = Self::toolset_instance_value(read_only, selector, members)?;
        Self::hash_with_scheme(scheme, &tagged)
    }

    /// Compute a toolset instance's digest under the current scheme
    ///
    /// Members are hashed by their spec hash and inclusion, sorted, so neither their ids nor
    /// their order changes the digest.
    ///
    /// # Arguments
    ///
    /// * `read_only` - Whether the read only filter was on
    /// * `selector` - The toolset's tag selector
    /// * `members` - Each member's spec hash and inclusion
    pub fn toolset_instance(
        read_only: bool,
        selector: Option<&ToolSetSelector>,
        members: &[(&str, ToolSetInclusion)],
    ) -> Result<String, serde_json::Error> {
        // hash under the current scheme
        Self::toolset_instance_with_scheme(Self::SCHEME, read_only, selector, members)
    }

    /// Build the tagged digest value of an instanced agent's toolset
    ///
    /// # Arguments
    ///
    /// * `spec_hashes` - The spec hash of each of the agent's tools
    pub fn agent_toolset_value(spec_hashes: &[&str]) -> Result<Value, serde_json::Error> {
        // tag the sorted spec hashes as an agent toolset
        let input = AgentToolsetDigest {
            tools: sorted_hashes(spec_hashes),
        };
        Self::tagged_value(DigestTag::AgentToolset, &input)
    }

    /// Compute an instanced agent's toolset digest under the current scheme
    ///
    /// The digest covers the spec hashes of the agent's effective tools, whatever order they
    /// are listed in.
    ///
    /// # Arguments
    ///
    /// * `spec_hashes` - The spec hash of each of the agent's tools
    pub fn agent_toolset(spec_hashes: &[&str]) -> Result<String, serde_json::Error> {
        // build the tagged value and hash it under the current scheme
        let tagged = Self::agent_toolset_value(spec_hashes)?;
        Self::hash_with_scheme(Self::SCHEME, &tagged)
    }

    /// Build the tagged digest value of a workflow instance
    ///
    /// Agents are described by their content and sorted as a multiset, so neither their ids nor
    /// their order changes the value while two identical agents still differ from one.
    ///
    /// # Arguments
    ///
    /// * `input` - The hashes and settings the workflow digest covers
    pub fn workflow_instance_value(
        input: &WorkflowDigestInput,
    ) -> Result<Value, serde_json::Error> {
        // describe each agent with its lists sorted, then sort the agents
        let mut nodes = input
            .agents
            .iter()
            .map(|agent| WorkflowAgentDigest {
                config_hash: agent.config_hash.to_owned(),
                harness: agent.harness.unwrap_or_default().to_owned(),
                tools: sorted_hashes(&agent.tools),
                tool_sets: sorted_hashes(&agent.tool_sets),
                bindings: sorted_bindings(&agent.bindings),
            })
            .collect::<Vec<WorkflowAgentDigest>>();
        nodes.sort();
        // sort the delegations by their agents' config hashes
        let mut pairs = input
            .delegations
            .iter()
            .map(|(source, target)| WorkflowDelegationDigest {
                source: (*source).to_owned(),
                target: (*target).to_owned(),
            })
            .collect::<Vec<WorkflowDelegationDigest>>();
        pairs.sort();
        // tag everything as a workflow instance
        let digest = WorkflowInstanceDigest {
            agents: nodes,
            delegations: pairs,
            entry_agent: input.entry_agent.unwrap_or_default().to_owned(),
            dependencies: sorted_hashes(input.dependencies),
            limits: input.limits.clone(),
            default_models: input.default_models.to_vec(),
        };
        Self::tagged_value(DigestTag::WorkflowInstance, &digest)
    }

    /// Compute a workflow instance's digest under a scheme prefix
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme prefix to use
    /// * `input` - The hashes and settings the workflow digest covers
    pub(crate) fn workflow_instance_with_scheme(
        scheme: &str,
        input: &WorkflowDigestInput,
    ) -> Result<String, serde_json::Error> {
        // build the tagged value and hash it
        let tagged = Self::workflow_instance_value(input)?;
        Self::hash_with_scheme(scheme, &tagged)
    }

    /// Compute a workflow instance's digest under the current scheme
    ///
    /// # Arguments
    ///
    /// * `input` - The hashes and settings the workflow digest covers
    pub fn workflow_instance(input: &WorkflowDigestInput) -> Result<String, serde_json::Error> {
        // hash under the current scheme
        Self::workflow_instance_with_scheme(Self::SCHEME, input)
    }

    /// Get the scheme prefix of a digest (for example `v1:`)
    ///
    /// # Arguments
    ///
    /// * `digest` - The digest to read
    #[must_use]
    pub fn scheme(digest: &str) -> Option<&str> {
        // the scheme is everything up to and including the first colon
        digest.find(':').map(|end| &digest[..=end])
    }

    /// Check whether a stored digest and a candidate describe the same definition under a scheme
    ///
    /// # Arguments
    ///
    /// * `scheme` - The active scheme prefix
    /// * `stored` - The stored digest
    /// * `recompute` - Recomputes the stored side from its stored value under the active scheme
    /// * `candidate` - The candidate digest computed under the active scheme
    pub(crate) fn same_definition_with_scheme<F, E>(
        scheme: &str,
        stored: &str,
        recompute: F,
        candidate: &str,
    ) -> Result<bool, E>
    where
        F: FnOnce() -> Result<String, E>,
    {
        // a stored digest under the active scheme compares directly
        if Self::scheme(stored) == Some(scheme) {
            return Ok(stored == candidate);
        }
        // an older stored digest is recomputed and never rewritten
        Ok(recompute()? == candidate)
    }

    /// Check whether a stored digest and a candidate describe the same definition
    ///
    /// A stored digest under an older scheme is recomputed from its stored value before comparing.
    ///
    /// # Arguments
    ///
    /// * `stored` - The stored digest
    /// * `recompute` - Recomputes the stored side from its stored value under the current scheme
    /// * `candidate` - The candidate digest computed under the current scheme
    pub fn same_definition<F, E>(stored: &str, recompute: F, candidate: &str) -> Result<bool, E>
    where
        F: FnOnce() -> Result<String, E>,
    {
        // compare under the current scheme
        Self::same_definition_with_scheme(Self::SCHEME, stored, recompute, candidate)
    }
}
