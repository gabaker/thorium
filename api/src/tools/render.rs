//! Renders tools, tool choices, and tool results for each model API
//!
//! One agentic tool reaches models through several wire protocols ([`ModelApi`]) that disagree on
//! the tool envelope, the schema dialect, strict mode, tool choice, and the shape of results. The
//! rules for each live here once, so every harness (and thorpy for harnesses written in Python)
//! renders a run's `tools.json` entries the same way.
//!
//! Each rendered tool definition is one element of the API's tools list: an Anthropic tool, a
//! Bedrock Converse `toolSpec` wrapper, an `OpenAI` function tool, or a Gemini function
//! declaration. The request-level wrappers (`toolConfig.tools`, Gemini's
//! `functionDeclarations`) and request fields such as Bedrock's `anthropic_version` are the
//! harness's to add.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::models::entities::tools::MAX_SCHEMA_DEPTH;
use crate::models::{
    GeminiApiVersion, HarnessToolEntry, ModelApi, ModelCapabilities, ToolChoice, ToolLoading,
    ToolStrictness,
};

/// The most strict tools one Anthropic request may carry; tools past it are sent non-strict
pub const ANTHROPIC_MAX_STRICT_TOOLS: usize = 20;

/// The most optional parameters the strict tools of one Anthropic request may have together
pub const ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS: usize = 24;

/// The most union types the strict tools of one Anthropic request may have together
pub const ANTHROPIC_MAX_STRICT_UNION_TYPES: usize = 16;

/// What a renderer needs to know about the model it renders for
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RenderOptions {
    /// What the model can do (`HarnessModelEntry.capabilities`)
    #[serde(default)]
    pub capabilities: ModelCapabilities,
    /// The Gemini API version of the endpoint (decides between `parameters` and
    /// `parametersJsonSchema`; unset for other providers)
    #[serde(default)]
    pub gemini_api_version: Option<GeminiApiVersion>,
    /// The provider's model id (`HarnessModelEntry.model_id`), which decides the model family
    /// behind Bedrock `InvokeModel`
    #[serde(default)]
    pub model_id: String,
    /// Whether the conversation already holds tool calls or results, which Bedrock Converse
    /// needs a tool config for even when tools are off
    #[serde(default)]
    pub history_has_tool_calls: bool,
}

/// One tool rendered for a model API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RenderedTool {
    /// The tool definition in the API's format
    pub definition: Value,
    /// Whether the definition was rendered in strict mode
    pub strict: bool,
    /// Messages for the harness to log, such as a `strict: On` tool sent non-strict
    pub warnings: Vec<String>,
}

/// A tool choice rendered for a model API
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RenderedToolChoice {
    /// The request fields to set, by name (empty = leave the API's defaults); a `null` value
    /// means the field must be left out of the request
    pub fields: Map<String, Value>,
    /// Whether the harness must run the model's tool calls one at a time itself
    pub serialize_calls: bool,
    /// Messages for the harness to log, such as a forced choice sent as `Auto`
    pub warnings: Vec<String>,
}

/// The outcome of one tool call before it is encoded for a model API
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallResult {
    /// The id the model gave the call
    pub call_id: String,
    /// The function name the model called
    pub function_name: String,
    /// What the tool returned
    pub output: ToolCallOutput,
    /// Whether the call failed (refused, timed out, or the tool reported an error)
    #[serde(default)]
    pub is_error: bool,
}

/// What a tool returned, by its `result_format`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolCallOutput {
    /// A JSON value
    Json(Value),
    /// Plain text
    Text(String),
    /// The paths of the result files in the harness
    Files(Vec<String>),
}

/// Why a tool couldn't be rendered for a model API
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The model API can't take tools for this model
    Unsupported {
        /// The model API
        api: ModelApi,
        /// Why it can't
        reason: String,
    },
}

impl std::fmt::Display for RenderError {
    /// Write this error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // name the API and the reason
        match self {
            RenderError::Unsupported { api, reason } => {
                write!(f, "tools can't be rendered for {api:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for RenderError {}

/// Why a model's arguments for a tool call were refused; the harness returns the message to the
/// model as an error result
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentError {
    /// The function name the model called
    pub function_name: String,
    /// What is wrong with the arguments, naming the tool
    pub message: String,
}

impl std::fmt::Display for ArgumentError {
    /// Write this error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the message already names the tool
        f.write_str(&self.message)
    }
}

impl std::error::Error for ArgumentError {}

/// Keywords whose value is a single subschema
const SINGLE_SCHEMA_KEYWORDS: [&str; 11] = [
    "items",
    "additionalItems",
    "additionalProperties",
    "unevaluatedProperties",
    "unevaluatedItems",
    "contains",
    "propertyNames",
    "not",
    "if",
    "then",
    "else",
];

/// Keywords whose value is an array of subschemas
const ARRAY_SCHEMA_KEYWORDS: [&str; 5] = ["allOf", "anyOf", "oneOf", "prefixItems", "items"];

/// Keywords whose value maps names to subschemas
const MAP_SCHEMA_KEYWORDS: [&str; 6] = [
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];

/// The numeric and length bounds strict Anthropic and Bedrock schemas can't carry
const BOUND_KEYWORDS: [&str; 11] = [
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
];

/// Check whether a model API takes the Anthropic Messages body
///
/// # Arguments
///
/// * `api` - The model API
fn is_anthropic_body(api: ModelApi) -> bool {
    // Vertex rawPredict and Bedrock InvokeModel carry the Anthropic body for Claude
    matches!(
        api,
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke
    )
}

/// Check whether a model API has a strict tool schema mode
///
/// # Arguments
///
/// * `api` - The model API
fn has_strict_variant(api: ModelApi) -> bool {
    // only the Gemini APIs have no strict tool schemas
    !matches!(
        api,
        ModelApi::GeminiGenerateContent | ModelApi::GeminiInteractions
    )
}

/// Check whether a model API has a request field that turns parallel tool calls off
///
/// # Arguments
///
/// * `api` - The model API
fn has_parallel_switch(api: ModelApi) -> bool {
    // the Anthropic body and the OpenAI APIs have one
    is_anthropic_body(api) || matches!(api, ModelApi::OpenAiChat | ModelApi::OpenAiResponses)
}

/// Visit a schema and its subschemas, parents before children, down to the schema depth limit
///
/// # Arguments
///
/// * `schema` - The subschema to visit
/// * `depth` - How deep the subschema is below the root
/// * `visit` - What to do with each subschema
fn walk_schema(
    schema: &mut Map<String, Value>,
    depth: usize,
    visit: &mut dyn FnMut(&mut Map<String, Value>),
) {
    // visit this subschema first, so the children it changes are walked as changed
    visit(schema);
    // a schema nested past the limit isn't walked further
    if depth >= MAX_SCHEMA_DEPTH {
        return;
    }
    // walk each child subschema
    for keyword in SINGLE_SCHEMA_KEYWORDS {
        if let Some(Value::Object(child)) = schema.get_mut(keyword) {
            walk_schema(child, depth + 1, visit);
        }
    }
    for keyword in ARRAY_SCHEMA_KEYWORDS {
        if let Some(Value::Array(items)) = schema.get_mut(keyword) {
            for item in items {
                if let Value::Object(child) = item {
                    walk_schema(child, depth + 1, visit);
                }
            }
        }
    }
    for keyword in MAP_SCHEMA_KEYWORDS {
        if let Some(Value::Object(map)) = schema.get_mut(keyword) {
            for value in map.values_mut() {
                if let Value::Object(child) = value {
                    walk_schema(child, depth + 1, visit);
                }
            }
        }
    }
}

/// Visit every subschema of a schema, parents before children
///
/// # Arguments
///
/// * `schema` - The schema to walk
/// * `visit` - What to do with each subschema
fn for_each_schema(
    schema: &mut Map<String, Value>,
    visit: &mut dyn FnMut(&mut Map<String, Value>),
) {
    // start at the root
    walk_schema(schema, 0, visit);
}

/// Check whether a subschema describes an object
///
/// # Arguments
///
/// * `schema` - The subschema to check
fn is_object_schema(schema: &Map<String, Value>) -> bool {
    // an object type or a properties object makes an object schema
    match schema.get("type") {
        Some(Value::String(kind)) if kind == "object" => true,
        Some(Value::Array(kinds)) if kinds.iter().any(|kind| kind == "object") => true,
        _ => schema.get("properties").is_some_and(Value::is_object),
    }
}

/// Get the property names a subschema requires
///
/// # Arguments
///
/// * `schema` - The subschema
fn required_names(schema: &Map<String, Value>) -> Vec<String> {
    // the required list holds property names
    schema
        .get("required")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

/// Make a property schema also accept `null`
///
/// # Arguments
///
/// * `property` - The property's schema
fn make_nullable(property: &mut Value) {
    // only schema objects can be changed
    let Value::Object(schema) = property else {
        return;
    };
    // add null to the type, or allow null beside a schema with no type
    match schema.get("type").cloned() {
        Some(Value::String(kind)) => {
            if kind != "null" {
                schema.insert("type".to_owned(), json!([kind, "null"]));
            }
        }
        Some(Value::Array(mut kinds)) => {
            if !kinds.contains(&json!("null")) {
                kinds.push(json!("null"));
            }
            schema.insert("type".to_owned(), Value::Array(kinds));
        }
        _ => {
            let inner = std::mem::take(schema);
            *property = json!({"anyOf": [Value::Object(inner), {"type": "null"}]});
            return;
        }
    }
    // an enum must list null too to accept it
    if let Some(Value::Array(values)) = schema.get_mut("enum")
        && !values.contains(&Value::Null)
    {
        values.push(Value::Null);
    }
}

/// Close every object of a schema to properties it doesn't list
///
/// # Arguments
///
/// * `schema` - The schema to change
fn close_objects(schema: &mut Map<String, Value>) {
    // every object schema gets `additionalProperties: false`
    for_each_schema(schema, &mut |sub| {
        if is_object_schema(sub) {
            sub.insert("additionalProperties".to_owned(), Value::Bool(false));
        }
    });
}

/// Drop every numeric and length bound from a schema
///
/// # Arguments
///
/// * `schema` - The schema to change
fn drop_bounds(schema: &mut Map<String, Value>) {
    // remove each bound keyword from every subschema
    for_each_schema(schema, &mut |sub| {
        for keyword in BOUND_KEYWORDS {
            sub.remove(keyword);
        }
    });
}

/// Require every property of every object, making the optional ones accept `null`
///
/// # Arguments
///
/// * `schema` - The schema to change
fn require_all(schema: &mut Map<String, Value>) {
    // change each object schema with properties
    for_each_schema(schema, &mut |sub| {
        let required = required_names(sub);
        let Some(Value::Object(properties)) = sub.get_mut("properties") else {
            return;
        };
        // an optional property must accept null once it's required
        let mut names = Vec::with_capacity(properties.len());
        for (name, property) in properties.iter_mut() {
            if !required.contains(name) {
                make_nullable(property);
            }
            names.push(Value::String(name.clone()));
        }
        sub.insert("required".to_owned(), Value::Array(names));
    });
}

/// Apply a model API's strict variant to a schema
///
/// # Arguments
///
/// * `api` - The model API
/// * `schema` - The schema to change
fn apply_strict(api: ModelApi, schema: &mut Map<String, Value>) {
    // every strict variant closes objects
    close_objects(schema);
    // the OpenAI APIs require every property, Anthropic and Bedrock drop bounds, and Vertex does
    // both
    match api {
        ModelApi::OpenAiChat | ModelApi::OpenAiResponses => require_all(schema),
        ModelApi::VertexRawPredict => {
            drop_bounds(schema);
            require_all(schema);
        }
        _ => drop_bounds(schema),
    }
}

/// Copy a subschema with every internal `$ref` replaced by the schema it points at
///
/// A reference that leads back into itself, goes past the depth limit, or doesn't resolve is
/// replaced by an open schema with a warning.
///
/// # Arguments
///
/// * `sub` - The subschema to copy
/// * `root` - The whole schema references resolve against
/// * `stack` - The references being inlined around this subschema
/// * `depth` - How deep the copy is
/// * `warnings` - Where to add warnings
fn inline_refs(
    sub: &Map<String, Value>,
    root: &Value,
    stack: &mut Vec<String>,
    depth: usize,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    // start from the reference's target, if this subschema is a reference
    let mut out = Map::new();
    if let Some(Value::String(reference)) = sub.get("$ref") {
        let target = reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
            .and_then(Value::as_object);
        match target {
            Some(target) if !stack.contains(reference) && depth < MAX_SCHEMA_DEPTH => {
                stack.push(reference.clone());
                out = inline_refs(target, root, stack, depth + 1, warnings);
                stack.pop();
            }
            _ => warnings.push(format!(
                "the `$ref` {reference} can't be inlined for Gemini, so it is sent as an open schema"
            )),
        }
    }
    // copy every other keyword, inlining the subschemas it holds
    for (key, value) in sub {
        if key == "$ref" {
            continue;
        }
        let key_str = key.as_str();
        let copied = match value {
            Value::Object(child) if SINGLE_SCHEMA_KEYWORDS.contains(&key_str) => {
                Value::Object(inline_refs(child, root, stack, depth + 1, warnings))
            }
            Value::Array(items) if ARRAY_SCHEMA_KEYWORDS.contains(&key_str) => Value::Array(
                items
                    .iter()
                    .map(|item| match item {
                        Value::Object(child) => {
                            Value::Object(inline_refs(child, root, stack, depth + 1, warnings))
                        }
                        other => other.clone(),
                    })
                    .collect(),
            ),
            Value::Object(map) if MAP_SCHEMA_KEYWORDS.contains(&key_str) => Value::Object(
                map.iter()
                    .map(|(name, item)| {
                        let item = match item {
                            Value::Object(child) => {
                                Value::Object(inline_refs(child, root, stack, depth + 1, warnings))
                            }
                            other => other.clone(),
                        };
                        (name.clone(), item)
                    })
                    .collect(),
            ),
            other => other.clone(),
        };
        out.insert(key.clone(), copied);
    }
    out
}

/// Merge one `allOf` part into the subschema holding it
///
/// # Arguments
///
/// * `sub` - The subschema the part is merged into
/// * `part` - The part to merge
fn merge_part(sub: &mut Map<String, Value>, part: Map<String, Value>) {
    // properties and required lists combine, and other keywords are kept where unset
    for (key, value) in part {
        match (key.as_str(), value) {
            ("properties", Value::Object(properties)) => {
                let merged = sub
                    .entry("properties")
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(merged) = merged {
                    for (name, property) in properties {
                        merged.entry(name).or_insert(property);
                    }
                }
            }
            ("required", Value::Array(names)) => {
                let merged = sub
                    .entry("required")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(merged) = merged {
                    for name in names {
                        if !merged.contains(&name) {
                            merged.push(name);
                        }
                    }
                }
            }
            (_, value) => {
                sub.entry(key).or_insert(value);
            }
        }
    }
}

/// Apply Gemini's `parameters` rewrites to one subschema
///
/// # Arguments
///
/// * `sub` - The subschema to rewrite
fn gemini_rewrite(sub: &mut Map<String, Value>) {
    // Gemini has no additionalProperties
    sub.remove("additionalProperties");
    // allOf parts are merged into the subschema
    if let Some(Value::Array(parts)) = sub.remove("allOf") {
        for part in parts {
            if let Value::Object(part) = part {
                merge_part(sub, part);
            }
        }
    }
    // oneOf becomes anyOf
    if let Some(Value::Array(options)) = sub.remove("oneOf") {
        match sub.get_mut("anyOf") {
            Some(Value::Array(any)) => any.extend(options),
            _ => {
                sub.insert("anyOf".to_owned(), Value::Array(options));
            }
        }
    }
    // const becomes a one-value enum
    if let Some(value) = sub.remove("const") {
        sub.entry("enum").or_insert_with(|| json!([value]));
    }
    // a type array becomes one type, or a union of types, marked nullable when it held null
    if let Some(Value::Array(kinds)) = sub.get("type").cloned() {
        sub.remove("type");
        let nullable = kinds.contains(&json!("null"));
        let mut others: Vec<Value> = kinds.into_iter().filter(|kind| kind != "null").collect();
        if others.len() == 1 {
            sub.insert("type".to_owned(), others.remove(0));
        } else if !others.is_empty() {
            let options = others.into_iter().map(|kind| json!({"type": kind}));
            match sub.get_mut("anyOf") {
                Some(Value::Array(any)) => any.extend(options),
                _ => {
                    sub.insert("anyOf".to_owned(), Value::Array(options.collect()));
                }
            }
        }
        if nullable {
            sub.insert("nullable".to_owned(), Value::Bool(true));
        }
    }
    // enum values are strings, with null turned into nullable
    if let Some(Value::Array(values)) = sub.get("enum").cloned() {
        let mut nullable = false;
        let strings = values
            .into_iter()
            .filter_map(|value| match value {
                Value::Null => {
                    nullable = true;
                    None
                }
                Value::String(text) => Some(Value::String(text)),
                other => Some(Value::String(other.to_string())),
            })
            .collect();
        sub.insert("enum".to_owned(), Value::Array(strings));
        sub.insert("type".to_owned(), json!("string"));
        if nullable {
            sub.insert("nullable".to_owned(), Value::Bool(true));
        }
    }
}

/// Rewrite a JSON schema into Gemini's `parameters` subset
///
/// # Arguments
///
/// * `schema` - The JSON schema to rewrite
/// * `warnings` - Where to add warnings
fn gemini_schema(schema: &Map<String, Value>, warnings: &mut Vec<String>) -> Map<String, Value> {
    // inline every reference against the original schema, then drop the definitions
    let root = Value::Object(schema.clone());
    let mut rewritten = inline_refs(schema, &root, &mut Vec::new(), 0, warnings);
    rewritten.remove("$defs");
    rewritten.remove("definitions");
    // apply the other rewrites to every subschema
    for_each_schema(&mut rewritten, &mut gemini_rewrite);
    rewritten
}

/// Build a Gemini function declaration
///
/// # Arguments
///
/// * `entry` - The tool's `tools.json` entry
/// * `schema` - The tool's input schema with a properties object
/// * `opts` - What the renderer knows about the model
/// * `warnings` - Where to add warnings
fn gemini_declaration(
    entry: &HarnessToolEntry,
    schema: Map<String, Value>,
    opts: &RenderOptions,
    warnings: &mut Vec<String>,
) -> Map<String, Value> {
    // v1 takes the OpenAPI subset and v1beta takes JSON schema unchanged
    let v1 = opts.gemini_api_version.unwrap_or_default() == GeminiApiVersion::V1;
    let mut parameters = if v1 {
        gemini_schema(&schema, warnings)
    } else {
        schema
    };
    // the first input example shows the model a call
    if let Some(example) = entry.input_examples.first() {
        parameters.insert("example".to_owned(), example.clone());
    }
    // build the declaration with its output schema as the response
    let mut declaration = Map::new();
    declaration.insert("name".to_owned(), json!(entry.function_name));
    declaration.insert("description".to_owned(), json!(entry.model_description));
    let (parameters_key, response_key) = if v1 {
        ("parameters", "response")
    } else {
        ("parametersJsonSchema", "responseJsonSchema")
    };
    declaration.insert(parameters_key.to_owned(), Value::Object(parameters));
    if let Some(output) = &entry.output_schema {
        let response = if v1 {
            gemini_schema(output, warnings)
        } else {
            output.clone()
        };
        declaration.insert(response_key.to_owned(), Value::Object(response));
    }
    declaration
}

/// Decide whether a tool is rendered strict, and the warning for a `strict: On` tool that isn't
///
/// # Arguments
///
/// * `entry` - The tool's `tools.json` entry
/// * `api` - The model API
/// * `opts` - What the renderer knows about the model
fn strictness(
    entry: &HarnessToolEntry,
    api: ModelApi,
    opts: &RenderOptions,
) -> (bool, Option<String>) {
    // a tool that is never strict stays that way
    if entry.strict == ToolStrictness::Off {
        return (false, None);
    }
    // the API, the model, and the schema must all allow strict mode
    let reason = if !has_strict_variant(api) {
        Some(format!("{api:?} has no strict tool schemas"))
    } else if !opts.capabilities.strict_tools {
        Some("the model doesn't accept strict tool schemas".to_owned())
    } else if !entry.portability.strict_compatible {
        Some(format!(
            "its input_schema isn't strict-compatible: {}",
            entry.portability.strict_issues.join("; ")
        ))
    } else {
        None
    };
    // only a tool set to On warns when it can't be strict
    match reason {
        None => (true, None),
        Some(reason) if entry.strict == ToolStrictness::On => (
            false,
            Some(format!(
                "tool `{}` is set to strict On but is sent non-strict to {api:?}: {reason}",
                entry.function_name
            )),
        ),
        Some(_) => (false, None),
    }
}

/// Build a tool's definition for a model API
///
/// # Arguments
///
/// * `entry` - The tool's `tools.json` entry
/// * `api` - The model API
/// * `strict` - Whether to render the strict variant
/// * `opts` - What the renderer knows about the model
/// * `warnings` - Where to add warnings
fn definition(
    entry: &HarnessToolEntry,
    api: ModelApi,
    strict: bool,
    opts: &RenderOptions,
    warnings: &mut Vec<String>,
) -> Value {
    // every API gets a schema with a properties object
    let mut schema = entry.input_schema.clone();
    if !schema.get("properties").is_some_and(Value::is_object) {
        schema.insert("properties".to_owned(), Value::Object(Map::new()));
    }
    // the Anthropic body refuses `$schema`
    if is_anthropic_body(api) {
        schema.remove("$schema");
    }
    // apply the API's strict variant
    if strict {
        apply_strict(api, &mut schema);
    }
    // wrap the schema in the API's envelope
    let deferred = entry.loading == ToolLoading::Deferred;
    let mut tool = Map::new();
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => {
            tool.insert("name".to_owned(), json!(entry.function_name));
            tool.insert("description".to_owned(), json!(entry.model_description));
            tool.insert("input_schema".to_owned(), Value::Object(schema));
            if strict {
                tool.insert("strict".to_owned(), Value::Bool(true));
            }
            if !entry.input_examples.is_empty() {
                tool.insert("input_examples".to_owned(), json!(entry.input_examples));
            }
            if deferred {
                tool.insert("defer_loading".to_owned(), Value::Bool(true));
            }
        }
        ModelApi::BedrockConverse => {
            let mut spec = Map::new();
            spec.insert("name".to_owned(), json!(entry.function_name));
            spec.insert("description".to_owned(), json!(entry.model_description));
            spec.insert("inputSchema".to_owned(), json!({"json": schema}));
            if strict {
                spec.insert("strict".to_owned(), Value::Bool(true));
            }
            tool.insert("toolSpec".to_owned(), Value::Object(spec));
        }
        ModelApi::OpenAiChat => {
            let mut function = Map::new();
            function.insert("name".to_owned(), json!(entry.function_name));
            function.insert("description".to_owned(), json!(entry.model_description));
            function.insert("parameters".to_owned(), Value::Object(schema));
            if strict {
                function.insert("strict".to_owned(), Value::Bool(true));
            }
            tool.insert("type".to_owned(), json!("function"));
            tool.insert("function".to_owned(), Value::Object(function));
        }
        ModelApi::OpenAiResponses => {
            tool.insert("type".to_owned(), json!("function"));
            tool.insert("name".to_owned(), json!(entry.function_name));
            tool.insert("description".to_owned(), json!(entry.model_description));
            tool.insert("parameters".to_owned(), Value::Object(schema));
            // Responses tries strict by default, so non-strict is sent explicitly
            tool.insert("strict".to_owned(), Value::Bool(strict));
            if deferred {
                tool.insert("defer_loading".to_owned(), Value::Bool(true));
            }
        }
        ModelApi::GeminiGenerateContent => {
            tool = gemini_declaration(entry, schema, opts, warnings);
        }
        ModelApi::GeminiInteractions => {
            tool.insert("type".to_owned(), json!("function"));
            tool.insert("name".to_owned(), json!(entry.function_name));
            tool.insert("description".to_owned(), json!(entry.model_description));
            tool.insert("parameters".to_owned(), Value::Object(schema));
        }
    }
    Value::Object(tool)
}

/// Render one tool for a model API
///
/// # Arguments
///
/// * `entry` - The tool's `tools.json` entry
/// * `api` - The protocol the model is called with
/// * `opts` - What the renderer needs to know about the model
pub fn render_tool(
    entry: &HarnessToolEntry,
    api: ModelApi,
    opts: &RenderOptions,
) -> Result<RenderedTool, RenderError> {
    // Bedrock InvokeModel takes the Anthropic body only for Claude models
    if api == ModelApi::BedrockInvoke && !opts.model_id.to_ascii_lowercase().contains("claude") {
        return Err(RenderError::Unsupported {
            api,
            reason: format!(
                "only Claude models take tools through it, not `{}`",
                opts.model_id
            ),
        });
    }
    // decide strictness and build the definition
    let (strict, warning) = strictness(entry, api, opts);
    let mut warnings: Vec<String> = warning.into_iter().collect();
    let definition = definition(entry, api, strict, opts, &mut warnings);
    Ok(RenderedTool {
        definition,
        strict,
        warnings,
    })
}

/// Count the optional properties of every object in a schema
///
/// # Arguments
///
/// * `schema` - The schema to count
fn optional_parameters(schema: &Map<String, Value>) -> usize {
    // count the properties each object schema doesn't require
    let mut schema = schema.clone();
    let mut count = 0;
    for_each_schema(&mut schema, &mut |sub| {
        let required = required_names(sub);
        if let Some(Value::Object(properties)) = sub.get("properties") {
            count += properties
                .keys()
                .filter(|name| !required.contains(name))
                .count();
        }
    });
    count
}

/// Count the union types in a schema
///
/// # Arguments
///
/// * `schema` - The schema to count
fn union_types(schema: &Value) -> usize {
    // count each anyOf, oneOf, and type array
    let Some(schema) = schema.as_object() else {
        return 0;
    };
    let mut schema = schema.clone();
    let mut count = 0;
    for_each_schema(&mut schema, &mut |sub| {
        if sub.get("anyOf").is_some_and(Value::is_array)
            || sub.get("oneOf").is_some_and(Value::is_array)
            || sub.get("type").is_some_and(Value::is_array)
        {
            count += 1;
        }
    });
    count
}

/// Render every tool of one request, applying the API's per-request limits
///
/// # Arguments
///
/// * `entries` - The tools' `tools.json` entries, in `tools.json` order
/// * `api` - The protocol the model is called with
/// * `opts` - What the renderer needs to know about the model
pub fn render_tools(
    entries: &[&HarnessToolEntry],
    api: ModelApi,
    opts: &RenderOptions,
) -> Result<Vec<RenderedTool>, RenderError> {
    // render each tool on its own first
    let mut rendered = entries
        .iter()
        .map(|entry| render_tool(entry, api, opts))
        .collect::<Result<Vec<RenderedTool>, RenderError>>()?;
    // only the Anthropic body has per-request strict limits
    if !is_anthropic_body(api) {
        return Ok(rendered);
    }
    // keep strict tools in tools.json order while they fit every limit
    let (mut tools, mut optional, mut unions) = (0, 0, 0);
    for (entry, tool) in entries.iter().zip(rendered.iter_mut()) {
        if !tool.strict {
            continue;
        }
        // count what this tool adds to the request
        let tool_optional = optional_parameters(&entry.input_schema);
        let tool_unions = union_types(&tool.definition["input_schema"]);
        let over = if tools + 1 > ANTHROPIC_MAX_STRICT_TOOLS {
            Some(format!("{ANTHROPIC_MAX_STRICT_TOOLS} strict tools"))
        } else if optional + tool_optional > ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS {
            Some(format!(
                "{ANTHROPIC_MAX_STRICT_OPTIONAL_PARAMETERS} optional parameters across strict tools"
            ))
        } else if unions + tool_unions > ANTHROPIC_MAX_STRICT_UNION_TYPES {
            Some(format!(
                "{ANTHROPIC_MAX_STRICT_UNION_TYPES} union types across strict tools"
            ))
        } else {
            None
        };
        // a tool past a limit is sent non-strict with a warning
        match over {
            Some(limit) => {
                let mut warnings = std::mem::take(&mut tool.warnings);
                warnings.push(format!(
                    "tool `{}` is sent non-strict: it would pass Anthropic's per-request limit of {limit}",
                    entry.function_name
                ));
                let definition = definition(entry, api, false, opts, &mut warnings);
                *tool = RenderedTool {
                    definition,
                    strict: false,
                    warnings,
                };
            }
            None => {
                tools += 1;
                optional += tool_optional;
                unions += tool_unions;
            }
        }
    }
    Ok(rendered)
}

/// Render an agent's tool choice and parallel-call setting for a model API
///
/// # Arguments
///
/// * `choice` - The agent's `tool_choice`
/// * `parallel` - The agent's `parallel_tool_calls`
/// * `api` - The protocol the model is called with
/// * `opts` - What the renderer needs to know about the model
#[must_use]
pub fn render_tool_choice(
    choice: &ToolChoice,
    parallel: Option<bool>,
    api: ModelApi,
    opts: &RenderOptions,
) -> RenderedToolChoice {
    // a forced choice is sent as Auto to a model that can't be forced
    let mut warnings = Vec::new();
    let forced = matches!(choice, ToolChoice::Required | ToolChoice::Function(_));
    let choice = if forced && !opts.capabilities.forced_tool_choice {
        warnings.push(format!(
            "the agent's tool choice {choice:?} is sent as Auto because the model doesn't accept a forced tool choice"
        ));
        &ToolChoice::Auto
    } else {
        choice
    };
    // parallel calls are turned off by the request when the API and model can, and by the
    // harness otherwise
    let serial = parallel == Some(false);
    let switch = serial && has_parallel_switch(api) && opts.capabilities.parallel_tool_calls;
    let serialize_calls = serial && !switch;
    // set the API's fields for the choice
    let mut fields = Map::new();
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => {
            let mut tool_choice = match choice {
                ToolChoice::Auto => json!({"type": "auto"}),
                ToolChoice::Required => json!({"type": "any"}),
                ToolChoice::None => json!({"type": "none"}),
                ToolChoice::Function(name) => json!({"type": "tool", "name": name}),
            };
            if switch && *choice != ToolChoice::None {
                tool_choice["disable_parallel_tool_use"] = Value::Bool(true);
            }
            fields.insert("tool_choice".to_owned(), tool_choice);
        }
        ModelApi::BedrockConverse => match choice {
            ToolChoice::Auto => {
                fields.insert("toolChoice".to_owned(), json!({"auto": {}}));
            }
            ToolChoice::Required => {
                fields.insert("toolChoice".to_owned(), json!({"any": {}}));
            }
            ToolChoice::Function(name) => {
                fields.insert("toolChoice".to_owned(), json!({"tool": {"name": name}}));
            }
            ToolChoice::None if opts.history_has_tool_calls => {
                // Converse needs a tool config once the history holds tool blocks
                fields.insert("toolChoice".to_owned(), json!({"auto": {}}));
                warnings.push(
                    "tools are off but the conversation holds tool calls, so the tool config stays and every tool call must be answered with an error result saying tools are off"
                        .to_owned(),
                );
            }
            ToolChoice::None => {
                fields.insert("toolConfig".to_owned(), Value::Null);
            }
        },
        ModelApi::OpenAiChat | ModelApi::OpenAiResponses => {
            let tool_choice = match (choice, api) {
                (ToolChoice::Auto, _) => json!("auto"),
                (ToolChoice::Required, _) => json!("required"),
                (ToolChoice::None, _) => json!("none"),
                (ToolChoice::Function(name), ModelApi::OpenAiChat) => {
                    json!({"type": "function", "function": {"name": name}})
                }
                (ToolChoice::Function(name), _) => json!({"type": "function", "name": name}),
            };
            fields.insert("tool_choice".to_owned(), tool_choice);
            if switch {
                fields.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
            }
        }
        ModelApi::GeminiGenerateContent => {
            let config = match choice {
                ToolChoice::Auto => json!({"mode": "AUTO"}),
                ToolChoice::Required => json!({"mode": "ANY"}),
                ToolChoice::None => json!({"mode": "NONE"}),
                ToolChoice::Function(name) => {
                    json!({"mode": "ANY", "allowedFunctionNames": [name]})
                }
            };
            fields.insert(
                "toolConfig".to_owned(),
                json!({"functionCallingConfig": config}),
            );
        }
        ModelApi::GeminiInteractions => {
            let tool_choice = match choice {
                ToolChoice::Auto => json!("auto"),
                ToolChoice::Required => json!("any"),
                ToolChoice::None => json!("none"),
                ToolChoice::Function(name) => {
                    json!({"allowed_tools": {"mode": "any", "tools": [name]}})
                }
            };
            fields.insert("tool_choice".to_owned(), tool_choice);
        }
    }
    RenderedToolChoice {
        fields,
        serialize_calls,
        warnings,
    }
}

/// Render a structured-output schema the way a model API takes it
///
/// # Arguments
///
/// * `schema` - The JSON schema the model's answer must follow
/// * `api` - The protocol the model is called with
#[must_use]
pub fn render_structured_output(schema: &Map<String, Value>, api: ModelApi) -> Value {
    // Bedrock Converse takes the schema as a JSON string and the other APIs as an object
    match api {
        ModelApi::BedrockConverse => Value::String(Value::Object(schema.clone()).to_string()),
        _ => Value::Object(schema.clone()),
    }
}

/// Cut text to at most a number of bytes on a character boundary
///
/// # Arguments
///
/// * `text` - The text to cut
/// * `max` - The most bytes to keep (0 = unlimited)
fn truncate(mut text: String, max: u64) -> (String, bool) {
    // text within the limit is kept whole
    let max = usize::try_from(max).unwrap_or(usize::MAX);
    if max == 0 || text.len() <= max {
        return (text, false);
    }
    // cut at the last character boundary within the limit
    let end = text.floor_char_boundary(max);
    text.truncate(end);
    (text, true)
}

/// Encode a tool call's result as the message or content block a model API expects
///
/// # Arguments
///
/// * `result` - The tool call's outcome
/// * `entry` - The tool's `tools.json` entry (for its `output_schema` and `max_result_bytes`)
/// * `api` - The protocol the model is called with
#[must_use]
pub fn encode_result(result: &ToolCallResult, entry: &HarnessToolEntry, api: ModelApi) -> Value {
    // a JSON success of a tool with an output schema must follow it
    let mismatch = match (&result.output, result.is_error) {
        (ToolCallOutput::Json(value), false) => entry.check_output(value).err(),
        _ => None,
    };
    let is_error = result.is_error || mismatch.is_some();
    // build the text every API gets, and the JSON value for APIs that take one
    let (text, json) = match (mismatch, &result.output) {
        (Some(message), _) => (message, None),
        (None, ToolCallOutput::Json(value)) => (value.to_string(), Some(value.clone())),
        (None, ToolCallOutput::Text(text)) => (text.clone(), None),
        (None, ToolCallOutput::Files(paths)) => (paths.join("\n"), None),
    };
    // a failure is marked in the text for APIs with no error flag
    let text = if is_error && matches!(api, ModelApi::OpenAiChat | ModelApi::OpenAiResponses) {
        format!("Error: {text}")
    } else {
        text
    };
    // cut the text to the entry's limit; a cut or failed result has no JSON value
    let (text, cut) = truncate(text, entry.max_result_bytes);
    let json = json.filter(|_| !cut && !is_error);
    // wrap the result in the API's shape
    let call_id = &result.call_id;
    let name = &result.function_name;
    match api {
        ModelApi::AnthropicMessages | ModelApi::VertexRawPredict | ModelApi::BedrockInvoke => {
            let mut block = json!({"type": "tool_result", "tool_use_id": call_id, "content": text});
            if is_error {
                block["is_error"] = Value::Bool(true);
            }
            block
        }
        ModelApi::BedrockConverse => {
            let mut content = Vec::with_capacity(2);
            if let Some(json) = json {
                content.push(json!({"json": json}));
            }
            content.push(json!({"text": text}));
            let status = if is_error { "error" } else { "success" };
            json!({"toolResult": {"toolUseId": call_id, "content": content, "status": status}})
        }
        ModelApi::OpenAiChat => json!({"role": "tool", "tool_call_id": call_id, "content": text}),
        ModelApi::OpenAiResponses => {
            json!({"type": "function_call_output", "call_id": call_id, "output": text})
        }
        ModelApi::GeminiGenerateContent => {
            let response = if is_error {
                json!({"error": text})
            } else {
                json!({"output": json.unwrap_or(Value::String(text))})
            };
            json!({"functionResponse": {"id": call_id, "name": name, "response": response}})
        }
        ModelApi::GeminiInteractions => {
            let mut block = json!({
                "type": "function_result",
                "call_id": call_id,
                "name": name,
                "result": json.unwrap_or(Value::String(text)),
            });
            if is_error {
                block["is_error"] = Value::Bool(true);
            }
            block
        }
    }
}

/// Decode and check the arguments of a model's tool call
///
/// # Arguments
///
/// * `raw` - The arguments as the API sent them (an object, or a JSON string)
/// * `entry` - The tool's `tools.json` entry
/// * `strict_applied` - Whether the tool was rendered in strict mode for this request
pub fn decode_arguments(
    raw: &Value,
    entry: &HarnessToolEntry,
    strict_applied: bool,
) -> Result<Value, ArgumentError> {
    // build errors that name the tool
    let refuse = |message: String| ArgumentError {
        function_name: entry.function_name.clone(),
        message,
    };
    // the OpenAI APIs send a JSON string, the others an object, and no arguments are empty
    let arguments = match raw {
        Value::String(text) => serde_json::from_str::<Value>(text).map_err(|error| {
            refuse(format!(
                "{} was called with arguments that aren't valid JSON: {error}",
                entry.function_name
            ))
        })?,
        Value::Null => Value::Object(Map::new()),
        other => other.clone(),
    };
    // arguments are always an object
    if !arguments.is_object() {
        return Err(refuse(format!(
            "{} was called with arguments that aren't a JSON object",
            entry.function_name
        )));
    }
    // arguments the API didn't enforce a schema on are checked against it
    if !strict_applied {
        entry.check_arguments(&arguments).map_err(refuse)?;
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests;
