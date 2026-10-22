//! Unit tests for content digests

use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{DigestTag, DigestValue, ThorDigest};
use crate::models::{
    AgentConfig, ModelApi, ModelCapabilities, ModelMatcher, ModelProviderKind, ModelRequirements,
    ToolChoice, ToolSetInclusion, ToolSetSelector, WorkflowLimits,
};
use crate::models::{
    AgenticTool, AgenticToolBacking, ArgTarget, ContainerToolBacking, HarnessArgs,
    HarnessCapabilities, HarnessInputs, HarnessInterface, HarnessOutputs, HarnessSpec,
    HarnessToolAccess, HarnessToolPolicy, McpConfigFormat, McpConfigTarget, OrphanedRunPolicy,
    ToolCapabilities, ToolEffect, ToolIcon, ToolInputMapping, ToolLoading, ToolOutputDelivery,
    ToolResultFormat, ToolSpec, ToolStrictness,
};
use crate::models::{
    ArgStrategy, AutoTag, AutoTagLogic, BurstableResources, CacheDependencySettings, ChildFilters,
    ChildrenDependencySettings, Cleanup, ConfigMap, Dependencies, DependencyPassStrategy,
    EphemeralDependencySettings, FileNamingStrategy, FileSystemDependencySettings, FilesHandler,
    GenericCacheDependencySettings, HostPath, HostPathTypes, ImageArgs, ImageLifetime, Kvm,
    KwargDependency, NFS, OutputCollection, OutputDisplayType, OutputHandler,
    RepoDependencySettings, Resources, ResultDependencySettings, SampleDependencySettings, Secret,
    SecurityContext, SpawnLimits, TagDependencySettings, Volume, VolumeTypes,
};
use crate::models::{
    BareMetalEnvironment, ExecutionSettings, JobDataSettings, K8sEnvironment, KvmEnvironment,
    SchedulingSettings, WindowsEnvironment, WorkloadEnvironment, WorkloadSpec,
};

/// The serde default for [`WithDefaultPath::level`]
fn default_level() -> u64 {
    3
}

/// A nested test struct
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct Inner {
    /// A name
    #[serde(default)]
    name: String,
    /// A count
    #[serde(default)]
    count: u64,
}

/// A test struct holding every container shape a digest prunes
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct Outer {
    /// A nested struct
    #[serde(default)]
    inner: Inner,
    /// An ordered list of nested structs
    #[serde(default)]
    list: Vec<Inner>,
    /// A set of names
    #[serde(default)]
    set: BTreeSet<String>,
    /// A sorted map of nested structs
    #[serde(default)]
    map: BTreeMap<String, Inner>,
    /// A map of environment variables
    #[serde(default)]
    env: HashMap<String, Option<String>>,
    /// Opaque user JSON
    #[serde(default)]
    opaque: Value,
    /// An opaque user JSON object
    #[serde(default)]
    object: Map<String, Value>,
    /// An optional nested struct
    #[serde(default)]
    maybe: Option<Inner>,
}

/// A test struct whose serde default function differs from its `Default`
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct WithDefaultPath {
    /// A level serde defaults to 3 while `Default` gives 0
    #[serde(default = "default_level")]
    level: u64,
}

/// A test struct before a field was added
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct SpecV1 {
    /// A name
    name: String,
}

/// The same test struct after a defaulted field was added
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct SpecV2 {
    /// A name
    name: String,
    /// A field added later with a default
    #[serde(default)]
    retries: u64,
}

/// The same test struct after a defaulted field was added, with a `Default` that differs from
/// the value serde fills in for the added field
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct SpecV2CustomDefault {
    /// A name
    name: String,
    /// A field added later that serde fills in with `u64::default()` when absent
    #[serde(default)]
    retries: u64,
}

impl Default for SpecV2CustomDefault {
    /// Build a default whose `retries` differs from the value serde fills in
    fn default() -> Self {
        SpecV2CustomDefault {
            name: String::new(),
            retries: 3,
        }
    }
}

/// A test enum whose variants are declared in one order
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    thorium_derive::DigestValue,
)]
enum Forward {
    /// Declared first
    Native,
    /// Declared second
    Mcp,
}

/// The test enum with its variants declared in the opposite order
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    thorium_derive::DigestValue,
)]
enum Reversed {
    /// Declared first
    Mcp,
    /// Declared second
    Native,
}

/// A test struct with camel case keys
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
#[serde(rename_all = "camelCase")]
struct CamelSpec {
    /// A multi word field
    first_field: u64,
    /// A field with its own rename
    #[serde(rename = "other")]
    second_field: String,
}

/// A test enum with every variant shape
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
#[serde(rename_all = "snake_case")]
enum Shape {
    /// A unit variant
    #[default]
    Empty,
    /// A newtype variant
    Single(Inner),
    /// A tuple variant
    Pair(u64, String),
    /// A struct variant
    Named {
        /// A width
        width: u64,
        /// A label
        label: String,
    },
}

/// A test struct holding a test enum
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct ShapeHolder {
    /// The shape
    shape: Shape,
}

/// A digest input holding an opaque JSON Schema document
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct SchemaSpec {
    /// A user JSON Schema
    #[serde(default)]
    input_schema: Map<String, Value>,
    /// Output collection settings
    #[serde(default)]
    output: OutputCollection,
}

/// A digest input with a list of member hashes
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct MembersInput {
    /// Member hashes
    members: Vec<String>,
}

/// A digest input embedding every existing workload type for known-answer vectors
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize, thorium_derive::DigestValue)]
struct KnownAnswerInput {
    /// Image args
    args: ImageArgs,
    /// Resources
    resources: Resources,
    /// Spawn limits
    spawn_limit: SpawnLimits,
    /// Lifetime
    lifetime: Option<ImageLifetime>,
    /// Volumes
    volumes: Vec<Volume>,
    /// Environment variables
    env: HashMap<String, Option<String>>,
    /// Security context
    security_context: Option<SecurityContext>,
    /// Dependencies
    dependencies: Dependencies,
    /// Output collection
    output_collection: OutputCollection,
    /// Clean up settings
    clean_up: Option<Cleanup>,
    /// Kvm settings
    kvm: Option<Kvm>,
    /// The display type
    display_type: OutputDisplayType,
}

/// Canonicalize a value to a UTF-8 string
///
/// # Arguments
///
/// * `value` - The value to canonicalize
fn jcs(value: &Value) -> String {
    // canonicalize through the same wrapper digests use
    String::from_utf8(ThorDigest::canonical_bytes(value).unwrap()).unwrap()
}

/// Build a fully populated known-answer input
fn known_answer_input() -> KnownAnswerInput {
    // build env with a declared but unset variable
    let mut env = HashMap::new();
    env.insert("FOO".to_owned(), None);
    env.insert("BAR".to_owned(), Some("baz".to_owned()));
    // build an auto tag map
    let mut auto_tag = HashMap::new();
    auto_tag.insert(
        "score".to_owned(),
        AutoTag::default().logic(AutoTagLogic::Greater(json!(0.5))),
    );
    // build the dependencies with a few changed settings
    let mut dependencies = Dependencies::default();
    dependencies.samples.strategy = DependencyPassStrategy::Names;
    dependencies.samples.naming = FileNamingStrategy::MostRecent;
    dependencies.results.kwarg = KwargDependency::List("--results".to_owned());
    dependencies.results.images = vec!["other".to_owned()];
    dependencies.tags.enabled = true;
    KnownAnswerInput {
        args: ImageArgs {
            entrypoint: Some(vec!["/bin/tool".to_owned()]),
            command: Some(vec!["--fast".to_owned(), "--json".to_owned()]),
            reaction: None,
            repo: None,
            commit: None,
            output: ArgStrategy::Kwarg("--out".to_owned()),
            output_files: ArgStrategy::Append,
        },
        resources: Resources {
            cpu: 2000,
            memory: 4096,
            ephemeral_storage: 0,
            worker_slots: 1,
            nvidia_gpu: 0,
            amd_gpu: 0,
            burstable: BurstableResources::default(),
        },
        spawn_limit: SpawnLimits::Basic(10),
        lifetime: Some(ImageLifetime::jobs(5)),
        volumes: vec![Volume::new("conf", "/etc/tool", VolumeTypes::ConfigMap).read_only()],
        env,
        security_context: Some(SecurityContext::default().user(1000)),
        dependencies,
        output_collection: OutputCollection {
            auto_tag,
            ..OutputCollection::default()
        },
        clean_up: None,
        kvm: None,
        display_type: OutputDisplayType::Html,
    }
}

/// The canonical JSON of the known-answer input with the `tool_spec` tag
const KNOWN_ANSWER_JCS: &str = concat!(
    r#"{"args":{"command":["--fast","--json"],"entrypoint":["/bin/tool"],"output":{"Kwarg":"--out"},"output_files":"Append"},"#,
    r#""dependencies":{"results":{"images":["other"],"kwarg":{"List":"--results"}},"samples":{"naming":"MostRecent","strategy":"Names"},"tags":{"enabled":true}},"#,
    r#""display_type":"HTML","env":{"BAR":"baz","FOO":null},"lifetime":{"amount":5,"counter":"jobs"},"#,
    r#""output_collection":{"auto_tag":{"score":{"logic":{"Greater":0.5}}}},"resources":{"cpu":2000,"memory":4096},"#,
    r#""security_context":{"user":1000},"spawn_limit":{"Basic":10},"t":"tool_spec","#,
    r#""volumes":[{"archetype":"ConfigMap","mount_path":"/etc/tool","name":"conf","read_only":true}]}"#,
);

/// The `v1:` digest of [`KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const KNOWN_ANSWER_DIGEST: &str =
    "v1:b994ed1b62b46da635eb7ec71cab86db01bd5516892ccee49c35369df4c2beb4";

/// The RFC 8785 key sorting vector produces the RFC's bytes
#[test]
fn rfc8785_key_sorting() {
    // build the RFC 8785 section 3.2.3 input
    let value = json!({
        "\u{20ac}": "Euro Sign",
        "\r": "Carriage Return",
        "\u{fb33}": "Hebrew Letter Dalet With Dagesh",
        "1": "One",
        "\u{1f600}": "Emoji: Grinning Face",
        "\u{0080}": "Control",
        "\u{00f6}": "Latin Small Letter O With Diaeresis"
    });
    // keys sort by their UTF-16 code units
    let expected = "{\"\\r\":\"Carriage Return\",\"1\":\"One\",\"\u{0080}\":\"Control\",\
        \"\u{00f6}\":\"Latin Small Letter O With Diaeresis\",\"\u{20ac}\":\"Euro Sign\",\
        \"\u{1f600}\":\"Emoji: Grinning Face\",\"\u{fb33}\":\"Hebrew Letter Dalet With Dagesh\"}";
    assert_eq!(jcs(&value), expected);
}

/// The RFC 8785 section 3.2.2 vector (numbers, string escaping, literals)
#[test]
fn rfc8785_sample() {
    // parse the RFC's first number from its text, which has more digits than an f64 holds
    let first: f64 = "333333333.33333329".parse().unwrap();
    // build the RFC 8785 section 3.2.2 input
    let value = json!({
        "numbers": [first, 1e30, 4.50, 2e-3, 0.000_000_000_000_000_000_000_000_001],
        "string": "\u{20ac}$\u{000F}\u{000a}A'\u{0042}\u{0022}\u{005c}\\\"/",
        "literals": [null, true, false]
    });
    // check the RFC's canonical bytes
    let expected = "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\
        \"string\":\"\u{20ac}$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}";
    assert_eq!(jcs(&value), expected);
}

/// The RFC 8785 appendix B number vectors produce ECMAScript number forms
#[test]
fn rfc8785_numbers() {
    // the IEEE-754 bit patterns and their canonical forms from RFC 8785 appendix B
    let cases: [(u64, &str); 23] = [
        (0x0000_0000_0000_0000, "0"),
        (0x8000_0000_0000_0000, "0"),
        (0x0000_0000_0000_0001, "5e-324"),
        (0x8000_0000_0000_0001, "-5e-324"),
        (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
        (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
        (0x4340_0000_0000_0000, "9007199254740992"),
        (0xc340_0000_0000_0000, "-9007199254740992"),
        (0x4430_0000_0000_0000, "295147905179352830000"),
        (0x44b5_2d02_c7e1_4af5, "9.999999999999997e+22"),
        (0x44b5_2d02_c7e1_4af6, "1e+23"),
        (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
        (0x444b_1ae4_d6e2_ef4e, "999999999999999700000"),
        (0x444b_1ae4_d6e2_ef4f, "999999999999999900000"),
        (0x444b_1ae4_d6e2_ef50, "1e+21"),
        (0x3eb0_c6f7_a0b5_ed8c, "9.999999999999997e-7"),
        (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
        (0x41b3_de43_5555_5553, "333333333.3333332"),
        (0x41b3_de43_5555_5554, "333333333.33333325"),
        (0x41b3_de43_5555_5555, "333333333.3333333"),
        (0x41b3_de43_5555_5556, "333333333.3333334"),
        (0x41b3_de43_5555_5557, "333333333.33333343"),
        (0xbecb_f647_612f_3696, "-0.0000033333333333333333"),
    ];
    for (bits, expected) in cases {
        // canonicalize each number on its own
        let value = json!(f64::from_bits(bits));
        assert_eq!(jcs(&value), expected, "bits {bits:#018x}");
    }
}

/// Key insertion order never changes the canonical bytes
#[test]
fn insertion_order() {
    // build the same object with two insertion orders
    let mut forward = Map::new();
    let mut backward = Map::new();
    let keys = ["zeta", "alpha", "Beta", "gamma", "\u{e9}", "a"];
    for (i, key) in keys.iter().enumerate() {
        forward.insert(
            (*key).to_owned(),
            json!({"index": i, "nested": {"b": 1, "a": 2}}),
        );
    }
    for (i, key) in keys.iter().enumerate().rev() {
        backward.insert(
            (*key).to_owned(),
            json!({"nested": {"a": 2, "b": 1}, "index": i}),
        );
    }
    // both orders give identical bytes and digests
    let forward = Value::Object(forward);
    let backward = Value::Object(backward);
    assert_eq!(jcs(&forward), jcs(&backward));
    assert_eq!(
        ThorDigest::of_canonical(&forward).unwrap(),
        ThorDigest::of_canonical(&backward).unwrap()
    );
}

/// Nested maps, sets, and arrays prune only contents, never entries or elements
#[test]
fn pruning_shapes() {
    // build a value with defaulted contents everywhere
    let mut value = Outer {
        list: vec![
            Inner::default(),
            Inner {
                name: "b".into(),
                count: 0,
            },
        ],
        set: ["z", "a", "m"].iter().map(|s| (*s).to_owned()).collect(),
        ..Outer::default()
    };
    value.map.insert("first".into(), Inner::default());
    value.map.insert(
        "second".into(),
        Inner {
            name: String::new(),
            count: 2,
        },
    );
    value.env.insert("FOO".into(), None);
    value.opaque = json!({"type": "object", "default": 0, "items": []});
    value.object.insert("empty".into(), json!({}));
    value.maybe = Some(Inner::default());
    // sets sort, arrays keep order and elements, maps keep entries, opaque JSON is untouched
    let expected = json!({
        "list": [{}, {"name": "b"}],
        "set": ["a", "m", "z"],
        "map": {"first": {}, "second": {"count": 2}},
        "env": {"FOO": null},
        "opaque": {"type": "object", "default": 0, "items": []},
        "object": {"empty": {}},
        "maybe": {}
    });
    assert_eq!(value.digest_value(), expected);
}

/// An `env` entry with no value hashes differently from an empty map
#[test]
fn env_entry_kept() {
    // build an empty env and one with a declared variable
    let empty = Outer::default();
    let mut declared = Outer::default();
    declared.env.insert("FOO".into(), None);
    // the declared variable changes the digest
    assert_ne!(
        ThorDigest::compute(DigestTag::ToolSpec, &empty).unwrap(),
        ThorDigest::compute(DigestTag::ToolSpec, &declared).unwrap()
    );
}

/// A field with `#[serde(default = "path")]` prunes against `path()`
#[test]
fn default_path() {
    // the serde default is pruned and the `Default` value is kept
    assert_eq!(WithDefaultPath { level: 3 }.digest_value(), json!({}));
    assert_eq!(
        WithDefaultPath { level: 0 }.digest_value(),
        json!({"level": 0})
    );
    // an absent field deserializes to the pruned value
    let absent: WithDefaultPath = serde_json::from_str("{}").unwrap();
    assert_eq!(absent.digest_value(), json!({}));
}

/// Enum variants of every shape follow serde's names and prune their payloads
#[test]
fn enum_shapes() {
    // unit variants are their names
    assert_eq!(Shape::Empty.digest_value(), json!("empty"));
    // newtype variants carry their pruned payload
    assert_eq!(
        Shape::Single(Inner::default()).digest_value(),
        json!({"single": {}})
    );
    // tuple variants carry an array
    assert_eq!(
        Shape::Pair(0, "x".into()).digest_value(),
        json!({"pair": [0, "x"]})
    );
    // struct variants prune their fields against their types' defaults
    assert_eq!(
        Shape::Named {
            width: 2,
            label: String::new()
        }
        .digest_value(),
        json!({"named": {"width": 2}})
    );
    // a field at its enum default is left out
    assert_eq!(ShapeHolder::default().digest_value(), json!({}));
}

/// Assert that a type's default digest value is empty
///
/// # Arguments
///
/// * `name` - The type's name for failure messages
fn assert_default_prunes<T: Default + DigestValue>(name: &str) {
    assert_eq!(T::default().digest_value(), json!({}), "{name}");
}

/// Every hashed struct's default prunes every field
#[test]
fn defaults_prune() {
    // existing workload types
    assert_default_prunes::<BurstableResources>("BurstableResources");
    assert_default_prunes::<ImageLifetime>("ImageLifetime");
    assert_default_prunes::<ImageArgs>("ImageArgs");
    assert_default_prunes::<SecurityContext>("SecurityContext");
    assert_default_prunes::<Dependencies>("Dependencies");
    assert_default_prunes::<SampleDependencySettings>("SampleDependencySettings");
    assert_default_prunes::<EphemeralDependencySettings>("EphemeralDependencySettings");
    assert_default_prunes::<ResultDependencySettings>("ResultDependencySettings");
    assert_default_prunes::<RepoDependencySettings>("RepoDependencySettings");
    assert_default_prunes::<TagDependencySettings>("TagDependencySettings");
    assert_default_prunes::<ChildrenDependencySettings>("ChildrenDependencySettings");
    assert_default_prunes::<ChildFilters>("ChildFilters");
    assert_default_prunes::<FileSystemDependencySettings>("FileSystemDependencySettings");
    assert_default_prunes::<CacheDependencySettings>("CacheDependencySettings");
    assert_default_prunes::<GenericCacheDependencySettings>("GenericCacheDependencySettings");
    assert_default_prunes::<Cleanup>("Cleanup");
    assert_default_prunes::<Kvm>("Kvm");
    assert_default_prunes::<Volume>("Volume");
    assert_default_prunes::<HostPath>("HostPath");
    assert_default_prunes::<ConfigMap>("ConfigMap");
    assert_default_prunes::<Secret>("Secret");
    assert_default_prunes::<NFS>("NFS");
    assert_default_prunes::<OutputCollection>("OutputCollection");
    assert_default_prunes::<FilesHandler>("FilesHandler");
    assert_default_prunes::<AutoTag>("AutoTag");
    // workload types
    assert_default_prunes::<WorkloadSpec>("WorkloadSpec");
    assert_default_prunes::<SchedulingSettings>("SchedulingSettings");
    assert_default_prunes::<K8sEnvironment>("K8sEnvironment");
    assert_default_prunes::<BareMetalEnvironment>("BareMetalEnvironment");
    assert_default_prunes::<WindowsEnvironment>("WindowsEnvironment");
    assert_default_prunes::<KvmEnvironment>("KvmEnvironment");
    assert_default_prunes::<ExecutionSettings>("ExecutionSettings");
    assert_default_prunes::<JobDataSettings>("JobDataSettings");
    // tool types
    assert_default_prunes::<AgenticTool>("AgenticTool");
    assert_default_prunes::<ToolCapabilities>("ToolCapabilities");
    assert_default_prunes::<ContainerToolBacking>("ContainerToolBacking");
    assert_default_prunes::<ToolInputMapping>("ToolInputMapping");
    assert_default_prunes::<ToolOutputDelivery>("ToolOutputDelivery");
    assert_default_prunes::<ToolIcon>("ToolIcon");
    // agent and model types
    assert_default_prunes::<ModelMatcher>("ModelMatcher");
    assert_default_prunes::<ModelRequirements>("ModelRequirements");
    assert_default_prunes::<ModelCapabilities>("ModelCapabilities");
    // workflow and toolset types
    assert_default_prunes::<WorkflowLimits>("WorkflowLimits");
    assert_default_prunes::<ToolSetSelector>("ToolSetSelector");
    // the agent config digest input's default is only its tag
    assert_eq!(
        ThorDigest::tagged_value(DigestTag::AgentConfig, &AgentConfig::default()).unwrap(),
        json!({"t": "agent_config"})
    );
    // the tool spec digest input's default is only its tag
    assert_eq!(
        ThorDigest::tagged_value(DigestTag::ToolSpec, &super::ToolSpecDigest::default()).unwrap(),
        json!({"t": "tool_spec"})
    );
    // test structs
    assert_default_prunes::<Inner>("Inner");
    assert_default_prunes::<Outer>("Outer");
    assert_default_prunes::<SpecV2>("SpecV2");
    assert_default_prunes::<CamelSpec>("CamelSpec");
    assert_default_prunes::<ShapeHolder>("ShapeHolder");
    assert_default_prunes::<SchemaSpec>("SchemaSpec");
    assert_default_prunes::<KnownAnswerInput>("KnownAnswerInput");
    // `Resources::default()` keeps `worker_slots` at 0 while serde fills in 1
    assert_eq!(
        Resources::default().digest_value(),
        json!({"worker_slots": 0})
    );
    // a digest input's default is only its tag
    for tag in [
        DigestTag::ToolSpec,
        DigestTag::AgentConfig,
        DigestTag::ToolSetInstance,
        DigestTag::AgentToolset,
        DigestTag::WorkflowInstance,
    ] {
        assert_eq!(
            ThorDigest::tagged_value(tag, &MembersInput::default()).unwrap(),
            json!({"t": tag.as_str()})
        );
    }
}

/// Adding a defaulted field leaves stored hashes unchanged
#[test]
fn added_field() {
    // hash a value stored before the field existed
    let stored = SpecV1 {
        name: "tool".into(),
    };
    let stored_json = serde_json::to_string(&stored).unwrap();
    let before = ThorDigest::compute(DigestTag::ToolSpec, &stored).unwrap();
    // load the same stored JSON with the new field
    let loaded: SpecV2 = serde_json::from_str(&stored_json).unwrap();
    let after = ThorDigest::compute(DigestTag::ToolSpec, &loaded).unwrap();
    assert_eq!(before, after);
    // an explicit default hashes like an omitted one
    let explicit: SpecV2 = serde_json::from_str(r#"{"name": "tool", "retries": 0}"#).unwrap();
    assert_eq!(
        ThorDigest::compute(DigestTag::ToolSpec, &explicit).unwrap(),
        after
    );
    // a set field changes the hash
    let set = SpecV2 {
        name: "tool".into(),
        retries: 2,
    };
    assert_ne!(
        ThorDigest::compute(DigestTag::ToolSpec, &set).unwrap(),
        after
    );
}

/// Adding a field with a plain `#[serde(default)]` leaves stored hashes unchanged even when the
/// struct's `Default` sets that field to something else
#[test]
fn added_field_with_custom_default() {
    // hash a value stored before the field existed
    let stored = SpecV1 {
        name: "tool".into(),
    };
    let stored_json = serde_json::to_string(&stored).unwrap();
    let before = ThorDigest::compute(DigestTag::ToolSpec, &stored).unwrap();
    // serde fills in 0 for the absent field, not the struct default's 3
    let loaded: SpecV2CustomDefault = serde_json::from_str(&stored_json).unwrap();
    assert_eq!(loaded.retries, 0);
    assert_eq!(
        ThorDigest::compute(DigestTag::ToolSpec, &loaded).unwrap(),
        before
    );
    // the struct default's value is a real setting and changes the hash
    let set = SpecV2CustomDefault {
        name: "tool".into(),
        ..SpecV2CustomDefault::default()
    };
    assert_eq!(set.digest_value(), json!({"name": "tool", "retries": 3}));
}

/// A set digests in the order of its elements' canonical JSON, whatever their `Ord` is
#[test]
fn set_order_is_canonical() {
    // the same members whose `Ord` puts them in opposite orders
    let forward: BTreeSet<Forward> = [Forward::Native, Forward::Mcp].into_iter().collect();
    let reversed: BTreeSet<Reversed> = [Reversed::Native, Reversed::Mcp].into_iter().collect();
    // both digest as the members sorted by their canonical JSON
    assert_eq!(forward.digest_value(), json!(["Mcp", "Native"]));
    assert_eq!(forward.digest_value(), reversed.digest_value());
}

/// A stored spec omitting defaults hashes like the fully written one
#[test]
fn omitted_defaults() {
    // write a spec out fully and minimally
    let full = serde_json::to_string(&known_answer_input()).unwrap();
    let minimal = KNOWN_ANSWER_JCS.replace(r#""t":"tool_spec","#, "");
    // both load to the same digest
    let full: KnownAnswerInput = serde_json::from_str(&full).unwrap();
    let minimal: KnownAnswerInput = serde_json::from_str(&minimal).unwrap();
    assert_eq!(
        ThorDigest::compute(DigestTag::ToolSpec, &full).unwrap(),
        ThorDigest::compute(DigestTag::ToolSpec, &minimal).unwrap()
    );
    // a fully written default dependencies block hashes like an absent one
    let written = serde_json::to_string(&Dependencies::default()).unwrap();
    let written: Dependencies = serde_json::from_str(&written).unwrap();
    let absent: Dependencies = serde_json::from_str("{}").unwrap();
    assert_eq!(written.digest_value(), absent.digest_value());
}

/// Number forms inside opaque JSON and auto tags hash the same
#[test]
fn opaque_numbers() {
    // build schemas that differ only in number form
    let parse = |raw: &str| -> SchemaSpec {
        serde_json::from_str(&format!(r#"{{"input_schema": {raw}}}"#)).unwrap()
    };
    let int = parse(r#"{"maximum": 1, "minimum": 0}"#);
    let float = parse(r#"{"maximum": 1.0, "minimum": -0}"#);
    assert_eq!(
        ThorDigest::compute(DigestTag::ToolSpec, &int).unwrap(),
        ThorDigest::compute(DigestTag::ToolSpec, &float).unwrap()
    );
    // a decimal minimum canonicalizes stably
    let decimal = parse(r#"{"minimum": 0.5}"#);
    let tagged = ThorDigest::tagged_value(DigestTag::ToolSpec, &decimal).unwrap();
    assert_eq!(
        jcs(&tagged),
        r#"{"input_schema":{"minimum":0.5},"t":"tool_spec"}"#
    );
    assert_eq!(
        ThorDigest::compute(DigestTag::ToolSpec, &decimal).unwrap(),
        ThorDigest::compute(DigestTag::ToolSpec, &parse(r#"{"minimum": 5e-1}"#)).unwrap()
    );
    // auto tag thresholds canonicalize stably and integer forms match
    let with_logic = |logic: AutoTagLogic| {
        let mut spec = SchemaSpec::default();
        spec.output
            .auto_tag
            .insert("score".into(), AutoTag::default().logic(logic));
        ThorDigest::compute(DigestTag::ToolSpec, &spec).unwrap()
    };
    let greater = with_logic(AutoTagLogic::Greater(json!(0.5)));
    assert_eq!(
        greater,
        with_logic(AutoTagLogic::Greater(serde_json::from_str("0.50").unwrap()))
    );
    assert_eq!(
        with_logic(AutoTagLogic::Equal(json!(1))),
        with_logic(AutoTagLogic::Equal(json!(1.0)))
    );
}

/// Type tags separate digests of equal payloads
#[test]
fn type_tags() {
    // equal toolset payloads under the two toolset tags differ
    let members = MembersInput {
        members: vec!["v1:aa".into(), "v1:bb".into()],
    };
    assert_ne!(
        ThorDigest::compute(DigestTag::ToolSetInstance, &members).unwrap(),
        ThorDigest::compute(DigestTag::AgentToolset, &members).unwrap()
    );
    // empty payloads under the tool spec and agent config tags differ
    let empty = MembersInput::default();
    assert_ne!(
        ThorDigest::compute(DigestTag::ToolSpec, &empty).unwrap(),
        ThorDigest::compute(DigestTag::AgentConfig, &empty).unwrap()
    );
    // every tag has its documented name
    let names: Vec<&str> = [
        DigestTag::ToolSpec,
        DigestTag::AgentConfig,
        DigestTag::ToolSetInstance,
        DigestTag::AgentToolset,
        DigestTag::WorkflowInstance,
    ]
    .iter()
    .map(|tag| tag.as_str())
    .collect();
    assert_eq!(
        names,
        [
            "tool_spec",
            "agent_config",
            "toolset_instance",
            "agent_toolset",
            "workflow_instance"
        ]
    );
}

/// A payload that already holds a `t` key or isn't an object is rejected
#[test]
fn invalid_payloads() {
    // a non object payload can't carry a tag
    assert!(ThorDigest::tagged_value(DigestTag::ToolSpec, &Shape::Empty).is_err());
    // a payload with its own `t` key would collide with the tag
    let mut object = Map::new();
    object.insert("t".into(), json!("agent_config"));
    assert!(ThorDigest::tagged_value(DigestTag::ToolSpec, &object).is_err());
}

/// A stored digest under an older prefix is recomputed before comparing
#[test]
fn older_prefix() {
    // a spec stored with a v1 digest
    let spec = SpecV2 {
        name: "tool".into(),
        retries: 1,
    };
    let stored = ThorDigest::compute(DigestTag::ToolSpec, &spec).unwrap();
    assert!(stored.starts_with("v1:"));
    assert_eq!(ThorDigest::scheme(&stored), Some("v1:"));
    // with a v2 scheme active the same spec gets a v2 digest
    let candidate = ThorDigest::compute_with_scheme("v2:", DigestTag::ToolSpec, &spec).unwrap();
    assert!(candidate.starts_with("v2:"));
    assert_ne!(stored, candidate);
    // the stored side is recomputed, so the same spec still matches (a no-op publish)
    let recompute = || ThorDigest::compute_with_scheme("v2:", DigestTag::ToolSpec, &spec);
    assert!(
        ThorDigest::same_definition_with_scheme("v2:", &stored, recompute, &candidate).unwrap()
    );
    // a different spec under v2 doesn't match
    let other = SpecV2 {
        name: "tool".into(),
        retries: 2,
    };
    let other = ThorDigest::compute_with_scheme("v2:", DigestTag::ToolSpec, &other).unwrap();
    assert!(!ThorDigest::same_definition_with_scheme("v2:", &stored, recompute, &other).unwrap());
    // the stored value itself is never rewritten
    assert!(stored.starts_with("v1:"));
    // under the current scheme stored digests compare directly without recomputing
    let current = ThorDigest::compute(DigestTag::ToolSpec, &spec).unwrap();
    let never = || -> Result<String, serde_json::Error> { panic!("recomputed a current digest") };
    assert!(ThorDigest::same_definition(&stored, never, &current).unwrap());
}

/// Assert that a type deserialized from `{}` prunes to `{}`
///
/// # Arguments
///
/// * `name` - The type's name for failure messages
fn assert_absent_prunes<T: serde::de::DeserializeOwned + DigestValue>(name: &str) {
    let value: T = serde_json::from_str("{}").unwrap_or_else(|err| panic!("{name}: {err}"));
    assert_eq!(value.digest_value(), json!({}), "{name}");
}

/// Every all-defaulted hashed struct deserialized from `{}` prunes to `{}`
#[test]
fn serde_defaults_prune() {
    // existing workload types whose fields all have serde defaults
    assert_absent_prunes::<ImageArgs>("ImageArgs");
    assert_absent_prunes::<SecurityContext>("SecurityContext");
    assert_absent_prunes::<Dependencies>("Dependencies");
    assert_absent_prunes::<EphemeralDependencySettings>("EphemeralDependencySettings");
    assert_absent_prunes::<ResultDependencySettings>("ResultDependencySettings");
    assert_absent_prunes::<CacheDependencySettings>("CacheDependencySettings");
    assert_absent_prunes::<OutputCollection>("OutputCollection");
    assert_absent_prunes::<FilesHandler>("FilesHandler");
    // test structs
    assert_absent_prunes::<Inner>("Inner");
    assert_absent_prunes::<Outer>("Outer");
    assert_absent_prunes::<WithDefaultPath>("WithDefaultPath");
    assert_absent_prunes::<SchemaSpec>("SchemaSpec");
}

/// The derive parses the serde attributes of real fields
#[test]
fn attribute_parsing() {
    // an empty `#[serde()]` followed by `default = "default_tags_strategy"` prunes `strategy`
    let mut tags = TagDependencySettings {
        strategy: DependencyPassStrategy::default(),
        ..TagDependencySettings::default()
    };
    assert_eq!(tags.digest_value(), json!({}));
    tags.strategy = DependencyPassStrategy::Names;
    assert_eq!(tags.digest_value(), json!({"strategy": "Names"}));
    // `default` plus `deserialize_with` prunes empty entrypoints and commands
    let args: ImageArgs = serde_json::from_str(r#"{"entrypoint": [], "command": []}"#).unwrap();
    assert_eq!(args.digest_value(), json!({}));
    // `skip_serializing_if` compiles and prunes as the default
    let volume = Volume::default();
    assert_eq!(volume.digest_value(), json!({}));
    let named = Volume {
        name: "conf".into(),
        ..Volume::default()
    };
    assert_eq!(named.digest_value(), json!({"name": "conf"}));
    // variant renames match serde
    assert_eq!(OutputDisplayType::Json.digest_value(), json!("JSON"));
    assert_eq!(OutputDisplayType::Html.digest_value(), json!("HTML"));
    assert_eq!(
        OutputDisplayType::Html.digest_value(),
        serde_json::to_value(OutputDisplayType::Html).unwrap()
    );
    // a container `rename_all` and a field `rename` give serde's keys
    let camel = CamelSpec {
        first_field: 1,
        second_field: "x".into(),
    };
    assert_eq!(camel.digest_value(), json!({"firstField": 1, "other": "x"}));
    assert_eq!(camel.digest_value(), serde_json::to_value(&camel).unwrap());
}

/// Enum, `Option`, and serde default function pruning on existing types
#[test]
fn existing_pruning() {
    // data variants carry their payload and unit variants are names
    assert_eq!(
        ArgStrategy::Kwarg("--out".into()).digest_value(),
        json!({"Kwarg": "--out"})
    );
    assert_eq!(DependencyPassStrategy::Names.digest_value(), json!("Names"));
    // a field at its enum default is left out
    let args = ImageArgs {
        output: ArgStrategy::None,
        ..ImageArgs::default()
    };
    assert_eq!(args.digest_value(), json!({}));
    // an option holding an empty string is kept and `None` is left out
    let args = ImageArgs {
        reaction: Some(String::new()),
        ..ImageArgs::default()
    };
    assert_eq!(args.digest_value(), json!({"reaction": ""}));
    let args = ImageArgs {
        reaction: None,
        ..ImageArgs::default()
    };
    assert_eq!(args.digest_value(), json!({}));
    // `worker_slots` prunes against `default_worker_slots` (1), so 0 is kept
    let one = Resources {
        cpu: 1,
        worker_slots: 1,
        ..Resources::default()
    };
    assert_eq!(one.digest_value(), json!({"cpu": 1}));
    let zero = Resources {
        cpu: 1,
        worker_slots: 0,
        ..Resources::default()
    };
    assert_eq!(zero.digest_value(), json!({"cpu": 1, "worker_slots": 0}));
}

/// List order matters, map insertion order doesn't, and default elements are kept
#[test]
fn list_and_map_order() {
    // swapping two command entries changes the hash
    let forward = ImageArgs {
        command: Some(vec!["a".into(), "b".into()]),
        ..ImageArgs::default()
    };
    let swapped = ImageArgs {
        command: Some(vec!["b".into(), "a".into()]),
        ..ImageArgs::default()
    };
    assert_ne!(forward.digest_value(), swapped.digest_value());
    // auto tags and kwarg maps built in different insertion orders hash the same
    let keys: Vec<String> = (0..32).map(|i| format!("key-{i}")).collect();
    let mut first = OutputCollection::default();
    let mut second = OutputCollection::default();
    let mut first_map = HashMap::new();
    let mut second_map = HashMap::with_capacity(128);
    for key in &keys {
        first
            .auto_tag
            .insert(key.clone(), AutoTag::default().key(key.clone()));
        first_map.insert(key.clone(), format!("--{key}"));
    }
    for key in keys.iter().rev() {
        second
            .auto_tag
            .insert(key.clone(), AutoTag::default().key(key.clone()));
        second_map.insert(key.clone(), format!("--{key}"));
    }
    assert_eq!(jcs(&first.digest_value()), jcs(&second.digest_value()));
    assert_eq!(
        jcs(&KwargDependency::Map(first_map).digest_value()),
        jcs(&KwargDependency::Map(second_map).digest_value())
    );
    // a list element equal to its default is kept
    let empty = EphemeralDependencySettings::default();
    let blank = EphemeralDependencySettings {
        names: vec![String::new()],
        ..EphemeralDependencySettings::default()
    };
    assert_ne!(empty.digest_value(), blank.digest_value());
    assert_eq!(blank.digest_value(), json!({"names": [""]}));
}

/// The frozen defaults of every hashed type
const FROZEN_DEFAULTS: &str = include_str!("defaults.json");

/// Build the default and absent-field serde values of a hashed type
///
/// # Arguments
///
/// * `minimal` - The required fields that let serde fill in every absent one, if the type is a struct
fn frozen<T>(minimal: Option<Value>) -> Value
where
    T: Default + Serialize + serde::de::DeserializeOwned,
{
    // serialize this type's default
    let mut entry = Map::new();
    entry.insert(
        "default".into(),
        serde_json::to_value(T::default()).unwrap(),
    );
    // serialize what serde fills in for absent fields
    if let Some(minimal) = minimal {
        let absent: T = serde_json::from_value(minimal).unwrap();
        entry.insert("absent".into(), serde_json::to_value(absent).unwrap());
    }
    Value::Object(entry)
}

/// Insert the current defaults of every hashed enum
///
/// # Arguments
///
/// * `all` - The map to insert each enum's defaults into
fn insert_enum_defaults(all: &mut Map<String, Value>) {
    // serialize each enum's default
    all.insert("ArgStrategy".into(), frozen::<ArgStrategy>(None));
    all.insert("AutoTagLogic".into(), frozen::<AutoTagLogic>(None));
    all.insert(
        "DependencyPassStrategy".into(),
        frozen::<DependencyPassStrategy>(None),
    );
    all.insert(
        "FileNamingStrategy".into(),
        frozen::<FileNamingStrategy>(None),
    );
    all.insert("HostPathTypes".into(), frozen::<HostPathTypes>(None));
    all.insert("KwargDependency".into(), frozen::<KwargDependency>(None));
    all.insert(
        "OutputDisplayType".into(),
        frozen::<OutputDisplayType>(None),
    );
    all.insert("OutputHandler".into(), frozen::<OutputHandler>(None));
    all.insert("SpawnLimits".into(), frozen::<SpawnLimits>(None));
    all.insert("VolumeTypes".into(), frozen::<VolumeTypes>(None));
    all.insert(
        "WorkloadEnvironment".into(),
        frozen::<WorkloadEnvironment>(None),
    );
    all.insert("ToolSpec".into(), frozen::<ToolSpec>(None));
    all.insert(
        "AgenticToolBacking".into(),
        frozen::<AgenticToolBacking>(None),
    );
    all.insert("ArgTarget".into(), frozen::<ArgTarget>(None));
    all.insert("ToolResultFormat".into(), frozen::<ToolResultFormat>(None));
    all.insert("ToolStrictness".into(), frozen::<ToolStrictness>(None));
    all.insert("ToolLoading".into(), frozen::<ToolLoading>(None));
    all.insert("ToolChoice".into(), frozen::<ToolChoice>(None));
    all.insert("McpConfigFormat".into(), frozen::<McpConfigFormat>(None));
    all.insert(
        "OrphanedRunPolicy".into(),
        frozen::<OrphanedRunPolicy>(None),
    );
}

/// Insert the current defaults of every hashed harness struct
///
/// # Arguments
///
/// * `all` - The map to insert each struct's defaults into
fn insert_harness_defaults(all: &mut Map<String, Value>) {
    // structs pass the required fields serde needs to fill in the rest
    all.insert(
        "HarnessSpec".into(),
        frozen::<HarnessSpec>(Some(json!({"workload": {"environment": "External"}}))),
    );
    all.insert(
        "HarnessInterface".into(),
        frozen::<HarnessInterface>(Some(json!({}))),
    );
    all.insert(
        "HarnessInputs".into(),
        frozen::<HarnessInputs>(Some(json!({}))),
    );
    all.insert(
        "HarnessOutputs".into(),
        frozen::<HarnessOutputs>(Some(json!({}))),
    );
    all.insert("HarnessArgs".into(), frozen::<HarnessArgs>(Some(json!({}))));
    all.insert(
        "HarnessToolAccess".into(),
        frozen::<HarnessToolAccess>(Some(json!({}))),
    );
    all.insert(
        "McpConfigTarget".into(),
        frozen::<McpConfigTarget>(Some(json!({"format": "Neutral", "path": ""}))),
    );
    all.insert(
        "HarnessCapabilities".into(),
        frozen::<HarnessCapabilities>(Some(json!({}))),
    );
    all.insert(
        "HarnessToolPolicy".into(),
        frozen::<HarnessToolPolicy>(Some(json!({}))),
    );
}

/// Insert the current defaults of every hashed agent and model struct
///
/// # Arguments
///
/// * `all` - The map to insert each struct's defaults into
fn insert_agent_defaults(all: &mut Map<String, Value>) {
    // structs pass the required fields serde needs to fill in the rest
    all.insert("AgentConfig".into(), frozen::<AgentConfig>(Some(json!({}))));
    all.insert(
        "ModelMatcher".into(),
        frozen::<ModelMatcher>(Some(json!({"model_id": ""}))),
    );
    all.insert(
        "ModelRequirements".into(),
        frozen::<ModelRequirements>(Some(json!({}))),
    );
    all.insert(
        "ModelCapabilities".into(),
        frozen::<ModelCapabilities>(Some(json!({}))),
    );
    // the workflow's run-wide limits are hashed into every workflow instance
    all.insert(
        "WorkflowLimits".into(),
        frozen::<WorkflowLimits>(Some(json!({}))),
    );
    // a toolset's selector is hashed into every toolset instance
    all.insert(
        "ToolSetSelector".into(),
        frozen::<ToolSetSelector>(Some(json!({"key": "", "value": ""}))),
    );
}

/// Insert the current defaults of every hashed tool struct
///
/// # Arguments
///
/// * `all` - The map to insert each struct's defaults into
fn insert_tool_defaults(all: &mut Map<String, Value>) {
    // structs pass the required fields serde needs to fill in the rest
    all.insert(
        "AgenticTool".into(),
        frozen::<AgenticTool>(Some(json!({
            "function_name": "",
            "model_description": "",
            "input_schema": {},
            "backing": {"ThoriumMcp": {"tool": ""}},
        }))),
    );
    all.insert(
        "ToolCapabilities".into(),
        frozen::<ToolCapabilities>(Some(json!({}))),
    );
    all.insert(
        "ContainerToolBacking".into(),
        frozen::<ContainerToolBacking>(Some(
            json!({"tool_instance": "00000000-0000-0000-0000-000000000000"}),
        )),
    );
    all.insert(
        "ToolInputMapping".into(),
        frozen::<ToolInputMapping>(Some(json!({}))),
    );
    all.insert(
        "ToolOutputDelivery".into(),
        frozen::<ToolOutputDelivery>(Some(json!({}))),
    );
    all.insert(
        "ToolIcon".into(),
        frozen::<ToolIcon>(Some(json!({"src": ""}))),
    );
}

/// Insert the current defaults of every hashed workload struct
///
/// # Arguments
///
/// * `all` - The map to insert each struct's defaults into
fn insert_workload_defaults(all: &mut Map<String, Value>) {
    // the scheduling settings every scheduled environment requires
    let scheduling = json!({"resources": {"cpu": 0, "memory": 0}, "spawn_limit": "Unlimited"});
    // structs pass the required fields serde needs to fill in the rest
    all.insert(
        "WorkloadSpec".into(),
        frozen::<WorkloadSpec>(Some(json!({"environment": "External"}))),
    );
    all.insert(
        "SchedulingSettings".into(),
        frozen::<SchedulingSettings>(Some(scheduling.clone())),
    );
    all.insert(
        "K8sEnvironment".into(),
        frozen::<K8sEnvironment>(Some(json!({"image": "", "scheduling": scheduling}))),
    );
    all.insert(
        "BareMetalEnvironment".into(),
        frozen::<BareMetalEnvironment>(Some(json!({"scheduling": scheduling}))),
    );
    all.insert(
        "WindowsEnvironment".into(),
        frozen::<WindowsEnvironment>(Some(json!({"image": "", "scheduling": scheduling}))),
    );
    all.insert(
        "KvmEnvironment".into(),
        frozen::<KvmEnvironment>(Some(json!({"scheduling": scheduling}))),
    );
    all.insert(
        "ExecutionSettings".into(),
        frozen::<ExecutionSettings>(Some(json!({}))),
    );
    all.insert(
        "JobDataSettings".into(),
        frozen::<JobDataSettings>(Some(json!({}))),
    );
}

/// Build the current defaults of every hashed type
fn current_defaults() -> Value {
    // required fields for structs that have them
    let empty = || Some(json!({}));
    let mut all = Map::new();
    // enums have no absent fields
    insert_enum_defaults(&mut all);
    // workload structs
    insert_workload_defaults(&mut all);
    // tool structs
    insert_tool_defaults(&mut all);
    // harness structs
    insert_harness_defaults(&mut all);
    // agent and model structs
    insert_agent_defaults(&mut all);
    // structs pass the required fields serde needs to fill in the rest
    all.insert(
        "AutoTag".into(),
        frozen::<AutoTag>(Some(json!({"logic": "Exists"}))),
    );
    all.insert(
        "BurstableResources".into(),
        frozen::<BurstableResources>(Some(json!({"cpu": 0, "memory": 0}))),
    );
    all.insert(
        "CacheDependencySettings".into(),
        frozen::<CacheDependencySettings>(empty()),
    );
    all.insert(
        "ChildFilters".into(),
        frozen::<ChildFilters>(Some(
            json!({"mime": [], "file_name": [], "file_extension": [], "submit_non_matches": false}),
        )),
    );
    all.insert(
        "ChildrenDependencySettings".into(),
        frozen::<ChildrenDependencySettings>(Some(json!({"enabled": false}))),
    );
    all.insert(
        "Cleanup".into(),
        frozen::<Cleanup>(Some(
            json!({"job_id": "None", "results": "None", "result_files_dir": "None", "script": ""}),
        )),
    );
    all.insert("ConfigMap".into(), frozen::<ConfigMap>(empty()));
    all.insert("Dependencies".into(), frozen::<Dependencies>(empty()));
    all.insert(
        "EphemeralDependencySettings".into(),
        frozen::<EphemeralDependencySettings>(empty()),
    );
    all.insert(
        "FileSystemDependencySettings".into(),
        frozen::<FileSystemDependencySettings>(Some(json!({"enabled": false}))),
    );
    all.insert("FilesHandler".into(), frozen::<FilesHandler>(empty()));
    all.insert(
        "GenericCacheDependencySettings".into(),
        frozen::<GenericCacheDependencySettings>(Some(json!({"strategy": "Disabled"}))),
    );
    all.insert(
        "HostPath".into(),
        frozen::<HostPath>(Some(json!({"path": ""}))),
    );
    all.insert("ImageArgs".into(), frozen::<ImageArgs>(empty()));
    all.insert(
        "ImageLifetime".into(),
        frozen::<ImageLifetime>(Some(json!({"counter": "", "amount": 0}))),
    );
    all.insert(
        "Kvm".into(),
        frozen::<Kvm>(Some(json!({"xml": "", "qcow2": ""}))),
    );
    all.insert(
        "NFS".into(),
        frozen::<NFS>(Some(json!({"path": "", "server": ""}))),
    );
    all.insert(
        "OutputCollection".into(),
        frozen::<OutputCollection>(empty()),
    );
    all.insert(
        "RepoDependencySettings".into(),
        frozen::<RepoDependencySettings>(Some(json!({"strategy": "Paths"}))),
    );
    all.insert(
        "ResultDependencySettings".into(),
        frozen::<ResultDependencySettings>(empty()),
    );
    all.insert(
        "Resources".into(),
        frozen::<Resources>(Some(json!({"cpu": 0, "memory": 0}))),
    );
    all.insert(
        "SampleDependencySettings".into(),
        frozen::<SampleDependencySettings>(Some(json!({"strategy": "Paths"}))),
    );
    all.insert("Secret".into(), frozen::<Secret>(empty()));
    all.insert("SecurityContext".into(), frozen::<SecurityContext>(empty()));
    all.insert(
        "TagDependencySettings".into(),
        frozen::<TagDependencySettings>(empty()),
    );
    all.insert(
        "Volume".into(),
        frozen::<Volume>(Some(
            json!({"name": "", "archetype": "HostPath", "mount_path": ""}),
        )),
    );
    Value::Object(all)
}

/// The defaults of every hashed type match the checked-in fixture
#[test]
fn frozen_defaults() {
    // load the checked-in fixture
    let fixture: Value = serde_json::from_str(FROZEN_DEFAULTS).unwrap();
    let current = current_defaults();
    // print the current defaults so a reviewed change can update the fixture
    assert_eq!(
        current,
        fixture,
        "a hashed default changed; current defaults:\n{}",
        serde_json::to_string_pretty(&current).unwrap()
    );
}

/// The known-answer vector over the existing workload types
///
/// This runs in both the default build and the `client`-only build
/// (`--no-default-features --features client`), so both produce identical digests.
#[test]
fn known_answer() {
    // build the tagged value of a populated input
    let input = known_answer_input();
    let tagged = ThorDigest::tagged_value(DigestTag::ToolSpec, &input).unwrap();
    // check the canonical bytes and the digest
    assert_eq!(jcs(&tagged), KNOWN_ANSWER_JCS);
    let digest = ThorDigest::compute(DigestTag::ToolSpec, &input).unwrap();
    assert_eq!(digest, KNOWN_ANSWER_DIGEST);
    // the digest is the v1 prefix and lowercase SHA-256 hex
    assert!(digest.starts_with("v1:"));
    assert_eq!(digest.len(), 67);
    assert!(
        digest[3..]
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    );
}

/// An agentic spec with a container backing that sets every agentic field
fn known_agentic_spec() -> ToolSpec {
    // map every kind of input onto the pinned tool's job
    let inputs = ToolInputMapping {
        args: BTreeMap::from([
            ("depth".to_owned(), ArgTarget::Kwarg("--depth".into())),
            ("path".to_owned(), ArgTarget::Positional),
            (
                "recursive".to_owned(),
                ArgTarget::Switch("--recursive".into()),
            ),
        ]),
        samples: Some("samples".into()),
    };
    // build the agentic tool
    let schema = json!({
        "type": "object",
        "properties": {
            "depth": {"type": "integer", "minimum": 0.5},
            "path": {"type": "string"},
            "recursive": {"type": "boolean"},
            "samples": {"type": "array", "items": {"type": "string"}},
        },
    });
    ToolSpec::Agentic(AgenticTool {
        function_name: "yara_scan".into(),
        title: Some("YARA scan".into()),
        model_description: "Scan samples with YARA rules".into(),
        input_schema: schema.as_object().unwrap().clone(),
        output_schema: Some(json!({"type": "object"}).as_object().unwrap().clone()),
        capabilities: ToolCapabilities {
            effects: BTreeSet::from([ToolEffect::WritesThorium, ToolEffect::Destructive]),
            idempotent: true,
            file_inputs: true,
            file_outputs: false,
            open_world: false,
        },
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: uuid::Uuid::nil(),
            inputs,
            outputs: ToolOutputDelivery {
                path: Some("/tmp/thorium/tool-results/yara".into()),
                max_result_bytes: 4096,
            },
        }),
        result_format: ToolResultFormat::Files,
        requires_approval: true,
        timeout: Some(600),
        input_examples: Vec::new(),
        strict: ToolStrictness::Auto,
        loading: ToolLoading::Eager,
        icons: Vec::new(),
    })
}

/// An Image spec in a K8s environment
fn known_image_spec() -> ToolSpec {
    // build a K8s workload with an image, env, and args
    let mut env = BTreeMap::new();
    env.insert("MODE".to_owned(), Some("fast".to_owned()));
    let mut workload = WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: "repo/yara:4.5".into(),
            scheduling: SchedulingSettings::default(),
            env,
            network_policies: BTreeSet::from(["egress".to_owned()]),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    };
    workload.execution.timeout = Some(300);
    workload.execution.args.command = Some(vec!["yara".into(), "-r".into()]);
    ToolSpec::Image(workload)
}

/// The canonical JSON of [`known_agentic_spec`]'s tagged digest value
const AGENTIC_KNOWN_ANSWER_JCS: &str = r#"{"kind":"Agentic","spec":{"Agentic":{"backing":{"Container":{"inputs":{"args":{"depth":{"Kwarg":"--depth"},"path":"Positional","recursive":{"Switch":"--recursive"}},"samples":"samples"},"outputs":{"max_result_bytes":4096,"path":"/tmp/thorium/tool-results/yara"},"tool_instance":"v1:pinned"}},"capabilities":{"effects":["Destructive","WritesThorium"],"file_inputs":true,"idempotent":true},"function_name":"yara_scan","input_schema":{"properties":{"depth":{"minimum":0.5,"type":"integer"},"path":{"type":"string"},"recursive":{"type":"boolean"},"samples":{"items":{"type":"string"},"type":"array"}},"type":"object"},"model_description":"Scan samples with YARA rules","output_schema":{"type":"object"},"requires_approval":true,"result_format":"Files","timeout":600,"title":"YARA scan"}},"t":"tool_spec"}"#;

/// The `v1:` digest of [`AGENTIC_KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const AGENTIC_KNOWN_ANSWER_DIGEST: &str =
    "v1:8ae560045d9633d848529adf837248556fb452e3e557ab1668d7344bb1af80ce";

/// The canonical JSON of [`known_image_spec`]'s tagged digest value
const IMAGE_KNOWN_ANSWER_JCS: &str = r#"{"kind":"Image","spec":{"Image":{"environment":{"K8s":{"env":{"MODE":"fast"},"image":"repo/yara:4.5","network_policies":["egress"]}},"execution":{"args":{"command":["yara","-r"]},"timeout":300}}},"t":"tool_spec"}"#;

/// The `v1:` digest of [`IMAGE_KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const IMAGE_KNOWN_ANSWER_DIGEST: &str =
    "v1:cd10735a2f568ab5d27a5b9624b5c53fd12f2951d766f260098541e18bd08614";

/// The known-answer vectors for a tool spec of each kind
///
/// These run in the client-only build too, so every client computes the same hashes as the API.
#[test]
fn tool_spec_known_answers() {
    // the agentic spec's pinned instance contributes its hash
    let agentic = ThorDigest::tool_spec_value(&known_agentic_spec(), Some("v1:pinned")).unwrap();
    assert_eq!(jcs(&agentic), AGENTIC_KNOWN_ANSWER_JCS);
    assert_eq!(
        ThorDigest::tool_spec(&known_agentic_spec(), Some("v1:pinned")).unwrap(),
        AGENTIC_KNOWN_ANSWER_DIGEST
    );
    // the image spec hashes its workload
    let image = ThorDigest::tool_spec_value(&known_image_spec(), None).unwrap();
    assert_eq!(jcs(&image), IMAGE_KNOWN_ANSWER_JCS);
    assert_eq!(
        ThorDigest::tool_spec(&known_image_spec(), None).unwrap(),
        IMAGE_KNOWN_ANSWER_DIGEST
    );
}

/// An agent config with most of its fields set, for the known-answer vector
fn known_agent_config() -> AgentConfig {
    AgentConfig {
        system_prompt: Some("You triage malware".into()),
        models: vec![
            ModelMatcher {
                model_id: "claude-opus".into(),
                provider: Some(ModelProviderKind::Anthropic),
                api: Some(ModelApi::AnthropicMessages),
                api_versions: BTreeSet::from(["2023-06-01".to_owned()]),
            },
            ModelMatcher {
                model_id: "gpt-5".into(),
                ..ModelMatcher::default()
            },
        ],
        model_requirements: ModelRequirements {
            min_context_window: Some(100_000),
            capabilities: ModelCapabilities {
                tool_calling: true,
                ..ModelCapabilities::default()
            },
            ..ModelRequirements::default()
        },
        max_turns: Some(40),
        max_swarm_size: Some(0),
        tool_tags: BTreeMap::from([("toolset".to_owned(), BTreeSet::from(["ghidra".to_owned()]))]),
        function_name: Some("triage_lead".into()),
        tool_choice: ToolChoice::Function("yara_scan".into()),
        parallel_tool_calls: Some(false),
        ..AgentConfig::default()
    }
}

/// The canonical JSON of [`known_agent_config`]'s tagged digest value
const AGENT_KNOWN_ANSWER_JCS: &str = r#"{"function_name":"triage_lead","max_swarm_size":0,"max_turns":40,"model_requirements":{"capabilities":{"tool_calling":true},"min_context_window":100000},"models":[{"api":"AnthropicMessages","api_versions":["2023-06-01"],"model_id":"claude-opus","provider":"Anthropic"},{"model_id":"gpt-5"}],"parallel_tool_calls":false,"system_prompt":"You triage malware","t":"agent_config","tool_choice":{"Function":"yara_scan"},"tool_tags":{"toolset":["ghidra"]}}"#;

/// The `v1:` digest of [`AGENT_KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const AGENT_KNOWN_ANSWER_DIGEST: &str =
    "v1:3a5b3b8d066fd84dbd69722b3eb047456c8784c612cbe6fc1ef312b79d87042f";

/// The known-answer vector for an agent config, whose digest value is tagged `agent_config`
///
/// This runs in the client-only build too, so every client computes the same `config_hash`.
#[test]
fn agent_config_known_answer() {
    // the tagged value carries the agent config tag
    let config = known_agent_config();
    let tagged = ThorDigest::tagged_value(DigestTag::AgentConfig, &config).unwrap();
    assert_eq!(tagged["t"], json!("agent_config"));
    // check the canonical bytes and the digest
    assert_eq!(jcs(&tagged), AGENT_KNOWN_ANSWER_JCS);
    assert_eq!(config.config_hash().unwrap(), AGENT_KNOWN_ANSWER_DIGEST);
}

/// The canonical JSON of the tagged digest value of the workflow [`workflow_known_answer`] builds
const WORKFLOW_KNOWN_ANSWER_JCS: &str = r#"{"agents":[{"config_hash":"v1:helper"},{"config_hash":"v1:lead","harness":"v1:harness","tool_sets":["v1:kit"],"tools":["v1:tool-a","v1:tool-b"]}],"default_models":[{"model_id":"gpt-5"}],"delegations":[{"source":"v1:helper","target":"v1:helper"},{"source":"v1:lead","target":"v1:helper"}],"dependencies":["v1:image-a","v1:image-b"],"entry_agent":"v1:lead","limits":{"max_alive_sub_agents":4},"t":"workflow_instance"}"#;

/// The `v1:` digest of [`WORKFLOW_KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const WORKFLOW_KNOWN_ANSWER_DIGEST: &str =
    "v1:d218d9b29e80d5d629c96fb89e598f099a4794abcb750e372e6921b00a8ace90";

/// The known-answer vector for a workflow instance digest, whose value is tagged
/// `workflow_instance`
///
/// The agents, tools, and delegations are listed out of order, and the vector holds them sorted.
#[test]
fn workflow_known_answer() {
    // an entry agent with a harness, two tools, and a toolset that delegates to a bare agent
    let lead = super::WorkflowAgentNode {
        config_hash: "v1:lead",
        harness: Some("v1:harness"),
        tools: vec!["v1:tool-b", "v1:tool-a"],
        tool_sets: vec!["v1:kit"],
        bindings: Vec::new(),
    };
    let helper = super::WorkflowAgentNode {
        config_hash: "v1:helper",
        ..super::WorkflowAgentNode::default()
    };
    let limits = WorkflowLimits {
        max_alive_sub_agents: Some(4),
    };
    let models = [ModelMatcher {
        model_id: "gpt-5".into(),
        ..ModelMatcher::default()
    }];
    let input = super::WorkflowDigestInput {
        agents: &[lead, helper],
        delegations: &[("v1:lead", "v1:helper"), ("v1:helper", "v1:helper")],
        entry_agent: Some("v1:lead"),
        dependencies: &["v1:image-b", "v1:image-a"],
        limits: &limits,
        default_models: &models,
    };
    // the tagged value carries the workflow instance tag
    let tagged = ThorDigest::workflow_instance_value(&input).unwrap();
    assert_eq!(tagged["t"], json!("workflow_instance"));
    // check the canonical bytes and the digest
    assert_eq!(jcs(&tagged), WORKFLOW_KNOWN_ANSWER_JCS);
    assert_eq!(
        ThorDigest::workflow_instance(&input).unwrap(),
        WORKFLOW_KNOWN_ANSWER_DIGEST
    );
}

/// The workflow digest inputs prune to nothing but their tags
#[test]
fn workflow_inputs_prune() {
    // the nested agent and delegation inputs prune to empty objects
    assert_default_prunes::<super::WorkflowAgentDigest>("WorkflowAgentDigest");
    assert_default_prunes::<super::WorkflowDelegationDigest>("WorkflowDelegationDigest");
    // the workflow and agent toolset inputs keep only their tags
    assert_eq!(
        ThorDigest::tagged_value(
            DigestTag::WorkflowInstance,
            &super::WorkflowInstanceDigest::default()
        )
        .unwrap(),
        json!({"t": "workflow_instance"})
    );
    assert_eq!(
        ThorDigest::tagged_value(
            DigestTag::AgentToolset,
            &super::AgentToolsetDigest::default()
        )
        .unwrap(),
        json!({"t": "agent_toolset"})
    );
    // an empty workflow hashes as its tag alone
    let limits = WorkflowLimits::default();
    let empty = super::WorkflowDigestInput {
        agents: &[],
        delegations: &[],
        entry_agent: None,
        dependencies: &[],
        limits: &limits,
        default_models: &[],
    };
    assert_eq!(
        ThorDigest::workflow_instance_value(&empty).unwrap(),
        json!({"t": "workflow_instance"})
    );
}

/// The canonical JSON of the known toolset instance's tagged digest value
const TOOLSET_KNOWN_ANSWER_JCS: &str = r#"{"members":[{"inclusion":"ExplicitLatest","spec_hash":"v1:tool-a"},{"inclusion":"ExplicitPinned","spec_hash":"v1:tool-a"},{"inclusion":"Selector","spec_hash":"v1:tool-b"}],"read_only":true,"selector":{"key":"toolset","value":"ghidra"},"t":"toolset_instance"}"#;

/// The `v1:` digest of [`TOOLSET_KNOWN_ANSWER_JCS`] (checked with `printf %s "$JCS" | sha256sum`)
const TOOLSET_KNOWN_ANSWER_DIGEST: &str =
    "v1:0dc4f76b6295d3c55ae646fd6147487d6e31bccbb30a3681c5a887bbb052b16f";

/// The canonical JSON of the known agent toolset's tagged digest value
const AGENT_TOOLSET_KNOWN_ANSWER_JCS: &str =
    r#"{"t":"agent_toolset","tools":["v1:tool-a","v1:tool-b"]}"#;

/// The `v1:` digest of [`AGENT_TOOLSET_KNOWN_ANSWER_JCS`]
const AGENT_TOOLSET_KNOWN_ANSWER_DIGEST: &str =
    "v1:e60693af160aba39be14e1481141f08b64402bc8c95c9096e8542ae1ded626ae";

/// The known-answer vector for a toolset instance digest, whose value is tagged
/// `toolset_instance` and carries each member's inclusion as its name
///
/// The members are listed out of order, and the vector holds them sorted by spec hash and then
/// inclusion name.
#[test]
fn toolset_known_answer() {
    // a read only toolset with a selector and three members
    let selector = ToolSetSelector::new("toolset", "ghidra");
    let members = [
        ("v1:tool-b", ToolSetInclusion::Selector),
        ("v1:tool-a", ToolSetInclusion::ExplicitPinned),
        ("v1:tool-a", ToolSetInclusion::ExplicitLatest),
    ];
    let tagged = ThorDigest::toolset_instance_value(true, Some(&selector), &members).unwrap();
    assert_eq!(tagged["t"], json!("toolset_instance"));
    assert_eq!(tagged["members"][0]["inclusion"], json!("ExplicitLatest"));
    // check the canonical bytes and the digest
    assert_eq!(jcs(&tagged), TOOLSET_KNOWN_ANSWER_JCS);
    assert_eq!(
        ThorDigest::toolset_instance(true, Some(&selector), &members).unwrap(),
        TOOLSET_KNOWN_ANSWER_DIGEST
    );
}

/// The known-answer vector for an agent's toolset digest, whose value is tagged `agent_toolset`
#[test]
fn agent_toolset_known_answer() {
    // two tools listed out of order
    let tagged = ThorDigest::agent_toolset_value(&["v1:tool-b", "v1:tool-a"]).unwrap();
    assert_eq!(tagged["t"], json!("agent_toolset"));
    assert_eq!(jcs(&tagged), AGENT_TOOLSET_KNOWN_ANSWER_JCS);
    assert_eq!(
        ThorDigest::agent_toolset(&["v1:tool-a", "v1:tool-b"]).unwrap(),
        AGENT_TOOLSET_KNOWN_ANSWER_DIGEST
    );
}

/// The toolset digest inputs prune to nothing but their tags, and an older scheme hashes the
/// same value under its own prefix
#[test]
fn toolset_inputs_prune() {
    // the member input prunes to an empty object
    assert_default_prunes::<super::ToolSetMemberDigest>("ToolSetMemberDigest");
    // an empty toolset hashes as its tag alone
    assert_eq!(
        ThorDigest::toolset_instance_value(false, None, &[]).unwrap(),
        json!({"t": "toolset_instance"})
    );
    // the same toolset under another scheme carries that scheme's prefix
    let selector = ToolSetSelector::new("toolset", "ghidra");
    let members = [("v1:tool-a", ToolSetInclusion::Selector)];
    let current = ThorDigest::toolset_instance(true, Some(&selector), &members).unwrap();
    let older =
        ThorDigest::toolset_instance_with_scheme("v2:", true, Some(&selector), &members).unwrap();
    assert!(older.starts_with("v2:"));
    assert_ne!(current, older);
}

/// Digests never use the in-process hasher or the unseparated SHA-256 helper
#[test]
fn no_unstable_hashers() {
    // read both digest source files
    let sources = [
        include_str!("../digests.rs"),
        include_str!("../../../../../thorium-derive/src/digest.rs"),
    ];
    // neither names the forbidden hashers
    for source in sources {
        assert!(!source.contains(concat!("Gx", "Hasher")));
        assert!(!source.contains(concat!("sha256", "_iter")));
    }
}
