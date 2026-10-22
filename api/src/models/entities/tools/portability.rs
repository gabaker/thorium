//! Walks author-written JSON schemas for the agentic tool portability rules and report
//!
//! Every check here works on the parsed schema only: nothing is fetched, and the walk is
//! iterative and stops below [`MAX_SCHEMA_DEPTH`] so a deep schema can't exhaust the stack.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use super::{ToolIcon, ToolPortabilityReport};

/// The deepest a subschema may be nested below the root of a tool schema
pub const MAX_SCHEMA_DEPTH: usize = 32;

/// The most input examples a tool may have
pub const MAX_INPUT_EXAMPLES: usize = 5;

/// The most bytes one serialized input example may take up
pub const MAX_INPUT_EXAMPLE_BYTES: usize = 4 * 1024;

/// The most bytes all of a tool's serialized input examples may take up together
pub const MAX_INPUT_EXAMPLES_BYTES: usize = 16 * 1024;

/// The most icons a tool may have
pub const MAX_TOOL_ICONS: usize = 4;

/// The most bytes an icon's `src` may take up
pub const MAX_ICON_SRC_BYTES: usize = 32 * 1024;

/// The most sizes one icon may list
pub const MAX_ICON_SIZES: usize = 8;

/// The most bytes one icon size may take up
const MAX_ICON_SIZE_BYTES: usize = 32;

/// The most bytes an icon's media type may take up
const MAX_ICON_MIME_TYPE_BYTES: usize = 127;

/// The most distinct regular expressions (`pattern` values and `patternProperties` keys) a
/// schema may hold
pub const MAX_SCHEMA_PATTERNS: usize = 64;

/// The most bytes one compiled regular expression may take up, large enough for length-bounded
/// Unicode classes like `^.{1,256}$`; the limit on distinct patterns bounds a schema's total
const MAX_PATTERN_COMPILED_BYTES: usize = 1024 * 1024;

/// The most bytes the lazy DFA of one regular expression may cache while matching
const MAX_PATTERN_DFA_BYTES: usize = 256 * 1024;

/// The media types a `data:` icon may have
const DATA_ICON_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// The longest a property name may be
const MAX_PROPERTY_NAME_LEN: usize = 64;

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

/// Keywords that reference another schema
const REF_KEYWORDS: [&str; 3] = ["$ref", "$dynamicRef", "$recursiveRef"];

/// Keywords that make a schema incompatible with strict mode wherever they appear
const STRICT_KEYWORDS: [&str; 8] = [
    "patternProperties",
    "if",
    "then",
    "else",
    "not",
    "dependentSchemas",
    "$dynamicRef",
    "$recursiveRef",
];

/// Keywords that name a subschema so a reference can target it by name
const ANCHOR_KEYWORDS: [&str; 2] = ["$anchor", "$dynamicAnchor"];

/// A kind of Gemini rewrite: its label, the keyword it applies to, and whether a value needs it
type RewriteKind = (&'static str, &'static str, fn(&Value) -> bool);

/// A subschema found while walking a schema
pub(super) struct Subschema<'a> {
    /// The JSON pointer of this subschema in URI fragment form (`#` is the root)
    pub pointer: String,
    /// The subschema itself
    pub schema: &'a Map<String, Value>,
    /// The index of the subschema holding this one in [`Walk::subschemas`] (unset for the root)
    pub parent: Option<usize>,
}

/// Every object subschema of a schema, and the first pointer nested too deep if there was one
pub(super) struct Walk<'a> {
    /// The subschemas, root first
    pub subschemas: Vec<Subschema<'a>>,
    /// The pointer of the first subschema nested deeper than [`MAX_SCHEMA_DEPTH`]
    pub too_deep: Option<String>,
}

/// Escape one JSON pointer token
///
/// # Arguments
///
/// * `token` - The token to escape
fn escape(token: &str) -> String {
    // `~` and `/` are the only characters a pointer token escapes
    token.replace('~', "~0").replace('/', "~1")
}

/// Walk every object subschema of a schema without recursion
///
/// Subschemas nested deeper than [`MAX_SCHEMA_DEPTH`] are left out and the first one is recorded.
///
/// # Arguments
///
/// * `root` - The schema to walk
pub(super) fn walk(root: &Map<String, Value>) -> Walk<'_> {
    // start with the root at depth 0
    let mut subschemas = Vec::new();
    let mut too_deep = None;
    let mut queue = VecDeque::from([("#".to_owned(), 0_usize, root, None)]);
    while let Some((pointer, depth, schema, parent)) = queue.pop_front() {
        // this subschema's index once it is pushed, which its children point back at
        let index = subschemas.len();
        // collect each child schema with its pointer
        let mut children: Vec<(String, &Map<String, Value>)> = Vec::new();
        for keyword in SINGLE_SCHEMA_KEYWORDS {
            if let Some(Value::Object(child)) = schema.get(keyword) {
                children.push((format!("{pointer}/{}", escape(keyword)), child));
            }
        }
        for keyword in ARRAY_SCHEMA_KEYWORDS {
            if let Some(Value::Array(items)) = schema.get(keyword) {
                for (index, item) in items.iter().enumerate() {
                    if let Value::Object(child) = item {
                        children.push((format!("{pointer}/{}/{index}", escape(keyword)), child));
                    }
                }
            }
        }
        for keyword in MAP_SCHEMA_KEYWORDS {
            if let Some(Value::Object(map)) = schema.get(keyword) {
                for (name, value) in map {
                    if let Value::Object(child) = value {
                        children.push((
                            format!("{pointer}/{}/{}", escape(keyword), escape(name)),
                            child,
                        ));
                    }
                }
            }
        }
        // queue the children that are within the depth limit and record the first that isn't
        for (child_pointer, child) in children {
            if depth + 1 > MAX_SCHEMA_DEPTH {
                too_deep.get_or_insert(child_pointer);
            } else {
                queue.push_back((child_pointer, depth + 1, child, Some(index)));
            }
        }
        subschemas.push(Subschema {
            pointer,
            schema,
            parent,
        });
    }
    Walk {
        subschemas,
        too_deep,
    }
}

/// Make sure a schema can't make the validator fetch anything, nest too deeply, or reach a
/// location the portability rules don't walk
///
/// # Arguments
///
/// * `field` - The name of the schema's field, for errors
/// * `walked` - The walked schema
pub(super) fn check_safety(field: &str, walked: &Walk<'_>) -> Result<(), String> {
    // a schema nested too deeply is refused
    if let Some(pointer) = &walked.too_deep {
        return Err(format!(
            "{field} is nested deeper than {MAX_SCHEMA_DEPTH} levels at {pointer}"
        ));
    }
    // map each walked pointer and named anchor so references can be resolved
    let indexes = pointer_indexes(walked);
    let anchors = anchors(walked);
    for sub in &walked.subschemas {
        // an embedded `$id` would change what the `#` references below it resolve against
        if sub.parent.is_some() && sub.schema.contains_key("$id") {
            return Err(format!(
                "{field} $id at {}/$id is only allowed at the root",
                sub.pointer
            ));
        }
        for keyword in REF_KEYWORDS {
            if let Some(reference) = sub.schema.get(keyword) {
                // a reference must be a string starting with `#`
                let Some(internal) = reference.as_str().filter(|value| value.starts_with('#'))
                else {
                    return Err(format!(
                        "{field} {keyword} {reference} at {} isn't an internal `#` reference",
                        sub.pointer
                    ));
                };
                // the reference must land on a subschema that was walked, so every rule saw it
                if !resolve(internal, &anchors)
                    .is_some_and(|target| indexes.contains_key(target.as_str()))
                {
                    return Err(format!(
                        "{field} {keyword} {reference} at {} doesn't point at a subschema of {field}",
                        sub.pointer
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Count the distinct regular expressions in a schema and refuse a schema with too many
///
/// Each distinct expression is compiled separately, so their number bounds the compile cost.
///
/// # Arguments
///
/// * `field` - The name of the schema's field, for errors
/// * `walked` - The walked schema
pub(super) fn check_patterns(field: &str, walked: &Walk<'_>) -> Result<(), String> {
    // collect every pattern value and pattern property key
    let mut patterns = HashSet::new();
    for sub in &walked.subschemas {
        if let Some(Value::String(pattern)) = sub.schema.get("pattern") {
            patterns.insert(pattern.as_str());
        }
        if let Some(Value::Object(map)) = sub.schema.get("patternProperties") {
            patterns.extend(map.keys().map(String::as_str));
        }
    }
    // too many distinct expressions are refused
    if patterns.len() > MAX_SCHEMA_PATTERNS {
        return Err(format!(
            "{field} has {} distinct patterns, over the limit of {MAX_SCHEMA_PATTERNS}",
            patterns.len()
        ));
    }
    Ok(())
}

/// Check whether a property name is portable (`^[A-Za-z_][A-Za-z0-9_]{0,63}$`)
///
/// # Arguments
///
/// * `name` - The property name to check
fn is_portable_property(name: &str) -> bool {
    // the first character is a letter or underscore and the rest are letters, digits, or underscores
    let mut chars = name.chars();
    name.len() <= MAX_PROPERTY_NAME_LEN
        && chars
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Check whether a schema declares the object type
///
/// # Arguments
///
/// * `schema` - The schema to check
fn is_object_schema(schema: &Map<String, Value>) -> bool {
    // the type is `object` or a list holding it
    match schema.get("type") {
        Some(Value::String(kind)) => kind == "object",
        Some(Value::Array(kinds)) => kinds.iter().any(|kind| kind == "object"),
        _ => false,
    }
}

/// Make sure every property name is portable and every object schema lists its properties
///
/// # Arguments
///
/// * `walked` - The walked input schema
pub(super) fn check_properties(walked: &Walk<'_>) -> Result<(), String> {
    // check every subschema
    for sub in &walked.subschemas {
        // every property name must be portable
        if let Some(Value::Object(properties)) = sub.schema.get("properties") {
            for name in properties.keys() {
                if !is_portable_property(name) {
                    return Err(format!(
                        "input_schema property name at {}/properties/{} must match ^[A-Za-z_][A-Za-z0-9_]{{0,63}}$",
                        sub.pointer,
                        escape(name)
                    ));
                }
            }
        }
        // an object schema must have a properties object, even an empty one
        if is_object_schema(sub.schema)
            && !sub.schema.get("properties").is_some_and(Value::is_object)
        {
            return Err(format!(
                "input_schema object schema at {} needs a properties object (it may be empty)",
                sub.pointer
            ));
        }
    }
    Ok(())
}

/// Decode the percent-escapes in a URI fragment
///
/// A malformed escape is kept as written, so it never matches a walked pointer.
///
/// # Arguments
///
/// * `fragment` - The fragment to decode
fn percent_decode(fragment: &str) -> String {
    // copy each byte, replacing every well-formed `%XX` escape with the byte it encodes
    let raw = fragment.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        // decode an escape when both hex digits are there
        let escaped = (raw[index] == b'%')
            .then(|| fragment.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(byte) = escaped {
            decoded.push(byte);
            index += 3;
        } else {
            decoded.push(raw[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Resolve an internal reference to the pointer of the schema it targets
///
/// # Arguments
///
/// * `reference` - The reference to resolve
/// * `anchors` - The pointer of each named anchor
fn resolve(reference: &str, anchors: &BTreeMap<&str, &str>) -> Option<String> {
    // a pointer fragment resolves to itself and a named fragment to its anchor
    let fragment = percent_decode(reference.strip_prefix('#')?);
    if fragment.is_empty() || fragment.starts_with('/') {
        Some(format!("#{fragment}"))
    } else {
        anchors
            .get(fragment.as_str())
            .map(|pointer| (*pointer).to_owned())
    }
}

/// Map each walked subschema's pointer to its index in [`Walk::subschemas`]
///
/// # Arguments
///
/// * `walked` - The walked schema
fn pointer_indexes<'a>(walked: &'a Walk<'_>) -> HashMap<&'a str, usize> {
    // index the subschemas by pointer
    walked
        .subschemas
        .iter()
        .enumerate()
        .map(|(index, sub)| (sub.pointer.as_str(), index))
        .collect()
}

/// Map each named anchor to the pointer of the subschema declaring it
///
/// # Arguments
///
/// * `walked` - The walked schema
fn anchors<'a>(walked: &'a Walk<'_>) -> BTreeMap<&'a str, &'a str> {
    // collect every anchor keyword's name with its subschema's pointer
    walked
        .subschemas
        .iter()
        .flat_map(|sub| {
            ANCHOR_KEYWORDS.iter().filter_map(|keyword| {
                sub.schema
                    .get(*keyword)
                    .and_then(Value::as_str)
                    .map(|anchor| (anchor, sub.pointer.as_str()))
            })
        })
        .collect()
}

/// Label every node of a directed graph with its strongly connected component
///
/// This is Tarjan's algorithm run with an explicit stack, so a long chain can't exhaust the
/// call stack.
///
/// # Arguments
///
/// * `edges` - The nodes each node has an edge to
fn components(edges: &[Vec<usize>]) -> Vec<usize> {
    // the order each node was first visited, its lowest reachable visit order, and its component
    let mut order = vec![usize::MAX; edges.len()];
    let mut low = vec![0; edges.len()];
    let mut component = vec![usize::MAX; edges.len()];
    let mut on_stack = vec![false; edges.len()];
    let mut stack = Vec::new();
    let mut visited = 0;
    let mut found = 0;
    for start in 0..edges.len() {
        // skip nodes an earlier search already reached
        if order[start] != usize::MAX {
            continue;
        }
        // each frame is a node and the index of the next edge of it to follow
        let mut frames = vec![(start, 0_usize)];
        order[start] = visited;
        low[start] = visited;
        visited += 1;
        stack.push(start);
        on_stack[start] = true;
        while let Some(frame) = frames.last_mut() {
            let (node, next) = *frame;
            if let Some(&target) = edges[node].get(next) {
                // follow the next edge, descending into nodes not yet visited
                frame.1 += 1;
                if order[target] == usize::MAX {
                    order[target] = visited;
                    low[target] = visited;
                    visited += 1;
                    stack.push(target);
                    on_stack[target] = true;
                    frames.push((target, 0));
                } else if on_stack[target] {
                    low[node] = low[node].min(order[target]);
                }
                continue;
            }
            // every edge is followed, so close this node's component if it is the root of one
            frames.pop();
            if low[node] == order[node] {
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component[member] = found;
                    if member == node {
                        break;
                    }
                }
                found += 1;
            }
            // pass the lowest visit order up to the parent frame
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
        }
    }
    component
}

/// Find every reference that leads back into a schema containing it
///
/// The subschemas form a graph with an edge from each subschema to each of its children and from
/// each `$ref` to its target. A reference is recursive exactly when its target can reach it back,
/// that is when the reference and its target share a strongly connected component.
///
/// # Arguments
///
/// * `walked` - The walked schema
fn recursive_refs(walked: &Walk<'_>) -> Vec<String> {
    // map each walked pointer and named anchor to resolve references
    let indexes = pointer_indexes(walked);
    let anchors = anchors(walked);
    // link each subschema to its children and each reference to its target
    let mut edges = vec![Vec::new(); walked.subschemas.len()];
    let mut sites = Vec::new();
    for (index, sub) in walked.subschemas.iter().enumerate() {
        if let Some(parent) = sub.parent {
            edges[parent].push(index);
        }
        let target = sub
            .schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| resolve(reference, &anchors));
        if let Some(target) = target
            && let Some(&target_index) = indexes.get(target.as_str())
        {
            edges[index].push(target_index);
            sites.push((index, target_index, target));
        }
    }
    // a reference in the same component as its target is recursive
    let component = components(&edges);
    sites
        .into_iter()
        .filter(|(site, target_index, _)| component[*site] == component[*target_index])
        .map(|(site, _, target)| {
            format!(
                "recursive `$ref` at {}/$ref to {target}",
                walked.subschemas[site].pointer
            )
        })
        .collect()
}

/// List why a schema can't be sent in strict mode
///
/// # Arguments
///
/// * `walked` - The walked input schema
pub(super) fn strict_issues(walked: &Walk<'_>) -> Vec<String> {
    // check each subschema's keywords
    let mut issues = Vec::new();
    for sub in &walked.subschemas {
        // only a closed object is strict
        if let Some(additional) = sub.schema.get("additionalProperties")
            && additional != &Value::Bool(false)
        {
            issues.push(format!(
                "`additionalProperties` at {}/additionalProperties isn't false",
                sub.pointer
            ));
        }
        // these keywords are never strict
        for keyword in STRICT_KEYWORDS {
            if sub.schema.contains_key(keyword) {
                issues.push(format!(
                    "`{keyword}` at {}/{}",
                    sub.pointer,
                    escape(keyword)
                ));
            }
        }
    }
    // strict mode can't express recursion
    issues.extend(recursive_refs(walked));
    issues
}

/// List the lossy rewrites a Gemini `parameters` schema needs, one entry per kind of rewrite
///
/// # Arguments
///
/// * `walked` - The walked input schema
pub(super) fn gemini_rewrites(walked: &Walk<'_>) -> Vec<String> {
    // each kind of rewrite, the keyword it applies to, and whether a value triggers it
    let kinds: [RewriteKind; 6] = [
        (
            "drop `additionalProperties`",
            "additionalProperties",
            |_| true,
        ),
        ("inline `$ref`", "$ref", |_| true),
        ("`oneOf` to `anyOf`", "oneOf", |_| true),
        ("merge `allOf`", "allOf", |_| true),
        ("`const` to `enum`", "const", |_| true),
        ("type array to `nullable`", "type", Value::is_array),
    ];
    // list the pointers of each kind that occurs
    kinds
        .iter()
        .filter_map(|(label, keyword, applies)| {
            let pointers = walked
                .subschemas
                .iter()
                .filter(|sub| sub.schema.get(*keyword).is_some_and(applies))
                .map(|sub| format!("{}/{}", sub.pointer, escape(keyword)))
                .collect::<Vec<String>>();
            if pointers.is_empty() {
                None
            } else {
                Some(format!("{label} at {}", pointers.join(", ")))
            }
        })
        .collect()
}

/// Build the portability report of an input schema
///
/// # Arguments
///
/// * `input_schema` - The input schema to report on
#[must_use]
pub(super) fn report(input_schema: &Map<String, Value>) -> ToolPortabilityReport {
    // walk the schema once for both lists
    let walked = walk(input_schema);
    let strict_issues = strict_issues(&walked);
    let gemini_rewrites = gemini_rewrites(&walked);
    ToolPortabilityReport {
        strict_compatible: strict_issues.is_empty(),
        strict_issues,
        gemini_compatible: gemini_rewrites.is_empty(),
        gemini_rewrites,
    }
}

/// Compile a schema with the validator settings used for tool schemas
///
/// The validator never fetches a reference and runs `pattern` with a linear-time regex engine
/// whose compiled size is capped, so no single expression is slow to compile. A pattern over
/// the cap is refused with an error naming the limit.
///
/// # Arguments
///
/// * `field` - The name of the schema's field, for errors
/// * `schema` - The schema to compile
pub(super) fn compile(
    field: &str,
    schema: &Map<String, Value>,
) -> Result<jsonschema::Validator, String> {
    // build an offline validator with the size-capped linear-time regex engine
    jsonschema::options()
        .offline()
        .with_pattern_options(
            jsonschema::PatternOptions::regex()
                .size_limit(MAX_PATTERN_COMPILED_BYTES)
                .dfa_size_limit(MAX_PATTERN_DFA_BYTES),
        )
        .build(&Value::Object(schema.clone()))
        .map_err(|err| {
            // a pattern refused for its compiled size names the limit
            if let Some(pattern) = oversized_pattern(&err) {
                return format!(
                    "{field} has the pattern {pattern:?}, which compiles to more than the \
                     {MAX_PATTERN_COMPILED_BYTES} byte limit for one regular expression"
                );
            }
            format!(
                "{field} isn't a schema the validator accepts at #{}: {err}",
                err.instance_path()
            )
        })
}

/// Get the pattern a schema compile refused for its compiled size, if that's why it failed
///
/// The validator reports every refused pattern the same way, so the pattern is compiled alone
/// under the same limit to tell a pattern that's too large from one that isn't valid.
///
/// # Arguments
///
/// * `err` - The error the schema compile failed with
fn oversized_pattern(err: &jsonschema::ValidationError<'_>) -> Option<String> {
    // only a refused regular expression can be over the limit
    let jsonschema::error::ValidationErrorKind::Format { format } = err.kind() else {
        return None;
    };
    if format != "regex" {
        return None;
    }
    // the refused pattern is the error's instance, both for `pattern` and `patternProperties`
    let pattern = err.instance().as_str()?;
    // compile it alone under the same limit, which fails fast when it's too large
    match regex::RegexBuilder::new(pattern)
        .size_limit(MAX_PATTERN_COMPILED_BYTES)
        .build()
    {
        Err(regex::Error::CompiledTooBig(_)) => Some(pattern.to_owned()),
        _ => None,
    }
}

/// Make sure a tool's input examples are few, small, and valid against its input schema
///
/// # Arguments
///
/// * `examples` - The input examples to check
/// * `validator` - The compiled input schema
pub(super) fn check_examples(
    examples: &[Value],
    validator: &jsonschema::Validator,
) -> Result<(), String> {
    // there can only be a few examples
    if examples.len() > MAX_INPUT_EXAMPLES {
        return Err(format!(
            "input_examples has {} examples, over the limit of {MAX_INPUT_EXAMPLES}",
            examples.len()
        ));
    }
    // each example must be small and the total must be too
    let mut total = 0;
    for (index, example) in examples.iter().enumerate() {
        // measure this example
        let bytes = serde_json::to_vec(example).map_or(0, |raw| raw.len());
        if bytes > MAX_INPUT_EXAMPLE_BYTES {
            return Err(format!(
                "input_examples[{index}] is {bytes} bytes, over the {MAX_INPUT_EXAMPLE_BYTES} byte limit"
            ));
        }
        total += bytes;
    }
    if total > MAX_INPUT_EXAMPLES_BYTES {
        return Err(format!(
            "input_examples total {total} bytes, over the {MAX_INPUT_EXAMPLES_BYTES} byte limit"
        ));
    }
    // each example must match the input schema
    for (index, example) in examples.iter().enumerate() {
        if let Err(err) = validator.validate(example) {
            return Err(format!(
                "input_examples[{index}] doesn't match input_schema at #{}: {err}",
                err.instance_path()
            ));
        }
    }
    Ok(())
}

/// Make sure a tool's icons are few, small, and portable
///
/// # Arguments
///
/// * `icons` - The icons to check
pub(super) fn check_icons(icons: &[ToolIcon]) -> Result<(), String> {
    // there can only be a few icons
    if icons.len() > MAX_TOOL_ICONS {
        return Err(format!(
            "icons has {} icons, over the limit of {MAX_TOOL_ICONS}",
            icons.len()
        ));
    }
    for (index, icon) in icons.iter().enumerate() {
        // every url must be small, whatever its scheme
        if icon.src.len() > MAX_ICON_SRC_BYTES {
            return Err(format!(
                "icons[{index}].src is {} bytes, over the {MAX_ICON_SRC_BYTES} byte limit",
                icon.src.len()
            ));
        }
        // the sizes must be few and short
        if icon.sizes.len() > MAX_ICON_SIZES
            || icon
                .sizes
                .iter()
                .any(|size| size.len() > MAX_ICON_SIZE_BYTES)
        {
            return Err(format!(
                "icons[{index}].sizes may list at most {MAX_ICON_SIZES} sizes of at most {MAX_ICON_SIZE_BYTES} bytes each"
            ));
        }
        // the media type must be short
        if icon
            .mime_type
            .as_ref()
            .is_some_and(|mime_type| mime_type.len() > MAX_ICON_MIME_TYPE_BYTES)
        {
            return Err(format!(
                "icons[{index}].mime_type is over the {MAX_ICON_MIME_TYPE_BYTES} byte limit"
            ));
        }
        // the url must be https or a raster data URI
        if icon.src.starts_with("https://") {
            continue;
        }
        let Some(data) = icon.src.strip_prefix("data:") else {
            return Err(format!(
                "icons[{index}].src must be an https: url or a data: URI"
            ));
        };
        // the media type runs up to the first parameter or the data
        let media_type = data.split([';', ',']).next().unwrap_or_default();
        if !DATA_ICON_TYPES.contains(&media_type) {
            return Err(format!(
                "icons[{index}].src data: URI type `{media_type}` must be one of {}",
                DATA_ICON_TYPES.join(", ")
            ));
        }
        // a media type set beside a data URI must agree with it
        if let Some(mime_type) = &icon.mime_type
            && !mime_type.eq_ignore_ascii_case(media_type)
        {
            return Err(format!(
                "icons[{index}].mime_type `{mime_type}` doesn't match its data: URI type `{media_type}`"
            ));
        }
    }
    Ok(())
}
