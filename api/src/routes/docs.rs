use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use axum::routing::{MethodRouter, get_service};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use tower_http::services::{ServeDir, ServeFile};
use utoipa::openapi::RefOr;
use utoipa::openapi::Required;
use utoipa::openapi::path::{ParameterBuilder, ParameterIn};
use utoipa::openapi::schema::{ArrayItems, KnownFormat, Schema, SchemaFormat, SchemaType, Type};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_swagger_ui::{Config, SwaggerUi};

use super::BasicApiDocs;
use super::associations::AssociationApiDocs;
use super::entities::EntityApiDocs;
use super::events::EventApiDocs;
use super::files::FileApiDocs;
use super::groups::GroupApiDocs;
use super::images::ImageApiDocs;
use super::jobs::JobApiDocs;
use super::network_policies::NetworkPolicyDocs;
use super::oauth::OAuthApiDocs;
use super::pipelines::PipelineApiDocs;
use super::reactions::ReactionApiDocs;
use super::repos::RepoApiDocs;
use super::search::SearchApiDocs;
use super::search::events::{ResultSearchEventApiDocs, TagSearchEventApiDocs};
use super::streams::StreamApiDocs;
use super::system::SystemApiDocs;
use super::trees::TreeApiDocs;
use super::users::UserApiDocs;

use crate::models::{ResultSearchEvent, SearchEvent, TagSearchEvent};
use crate::{Conf, utils::AppState};

/// The struct containing our `OpenAPI` security info
///
/// The API reads credentials from the `Authorization` header only. It accepts `Basic` with a base64
/// encoded `username:password`, and `Bearer` (or `Token`) with a base64 encoded Thorium token. Routes
/// that take no credentials list no security requirement.
pub struct OpenApiSecurity;

impl Modify for OpenApiSecurity {
    /// Add the `basic` and `bearer` security schemes to a document
    ///
    /// # Arguments
    ///
    /// * `openapi` - The document to add the security schemes to
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // get our components
        let components = openapi.components.as_mut().unwrap();
        // a username and password, sent as `Authorization: Basic base64(username:password)`
        components.add_security_scheme(
            "basic",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Basic)
                    .description(Some(
                        "A Thorium username and password. The Authorization header is `Basic` \
                         followed by the base64 encoding of `username:password`.",
                    ))
                    .build(),
            ),
        );
        // a Thorium token, sent as `Authorization: Bearer base64(token)`
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("base64 encoded Thorium token")
                    .description(Some(
                        "A Thorium token, such as the one `POST /api/users/auth` returns. The \
                         Authorization header is `Bearer` followed by the base64 encoding of the \
                         token, so paste the base64 encoded token as the value.",
                    ))
                    .build(),
            ),
        );
    }
}

/// The struct that adds example values to the required path, query, and header parameters of a document
///
/// Optional parameters get none, because Try it out sends every example it prefills. Every example is an obviously fake and safe value, such as `example-group`, so trying a route out
/// from the Swagger UI never names a real group, user or file. Each example matches its parameter's
/// schema: an enum parameter gets one of its variants, a uuid a uuid, and an array a single item.
pub struct OpenApiParameterExamples;

impl OpenApiParameterExamples {
    /// A fake string for a string parameter, chosen by the parameter's name and description
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the parameter
    /// * `description` - The description of the parameter
    fn string(name: &str, description: &str) -> String {
        // the same name means the same kind of thing across routes, so it gets the same fake value
        let value = match name {
            "group" => "example-group",
            "image" => "example-image",
            "pipeline" => "example-pipeline",
            "username" | "target" => "example-user",
            "tool" => "example-tool",
            "stage" => "example-stage",
            "tag" => "example-tag",
            "cmd" => "proceed",
            "commitish" => "main",
            "stream" => "example-stream",
            "path" | "result_file" => "example/output.txt",
            "scaler_or_name" | "worker" => "example-worker",
            "query" => "example",
            "sha256" => "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "repo_path" => "github.com/example-org/example-repo",
            // the repo routes split a path's first segment from the rest, or name a repo data sha256
            "data" if description.contains("sha256") => {
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            }
            "data" => "github.com",
            // the name of a thing depends on what the route names
            "name" if description.contains("OAuth provider") => "example-provider",
            "name" if description.contains("scoped token") => "example-token",
            "name" if description.contains("network policy") => "example-policy",
            "name" if description.contains("ephemeral file") => "example-file.txt",
            // a notification route names the user to notify by the same `id` that dismissal gives a uuid
            "id" if description.contains("user to notify") => "example-user",
            // any other name gets a fake built from it
            other => return format!("example-{}", other.replace('_', "-")),
        };
        value.to_owned()
    }

    /// A fake integer for an integer parameter, chosen by the parameter's name
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the parameter
    fn integer(name: &str) -> i64 {
        // timestamps and sizes need values that make sense for what they bound
        match name {
            "start" => 1_700_000_000,
            "end" => 1_700_086_400,
            "split" => 3600,
            "runtime" => 60,
            "limit" | "page_size" => 10,
            _ => 0,
        }
    }

    /// A fake date time for a date time parameter, chosen by the parameter's name
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the parameter
    fn date_time(name: &str) -> &'static str {
        // a range starts before it ends
        if name == "end" {
            "2024-01-31T00:00:00Z"
        } else {
            "2024-01-01T00:00:00Z"
        }
    }

    /// An example value that matches a schema, if there is a plain one to give
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the parameter
    /// * `description` - The description of the parameter
    /// * `schema` - The schema the example must match
    /// * `schemas` - The schemas the document holds, which a reference may name
    fn example(
        name: &str,
        description: &str,
        schema: &RefOr<Schema>,
        schemas: &BTreeMap<String, RefOr<Schema>>,
    ) -> Option<Value> {
        // follow a reference to the schema it names
        let schema = match schema {
            RefOr::Ref(reference) => {
                let target = reference.ref_location.rsplit('/').next()?;
                schemas.get(target)?
            }
            RefOr::T(_) => schema,
        };
        match schema {
            // a single item is the simplest array
            RefOr::T(Schema::Array(array)) => match &array.items {
                ArrayItems::RefOrSchema(items) => Self::example(name, description, items, schemas)
                    .map(|item| Value::from(vec![item])),
                ArrayItems::False => None,
            },
            RefOr::T(Schema::Object(object)) => {
                // an enum parameter takes one of its variants
                if let Some(variant) = object
                    .enum_values
                    .as_ref()
                    .and_then(|values| values.first())
                {
                    return Some(variant.clone());
                }
                // the first type that isn't null is the one the example must match
                let kind = match &object.schema_type {
                    SchemaType::Type(kind) => kind.clone(),
                    SchemaType::Array(kinds) => {
                        kinds.iter().find(|kind| **kind != Type::Null)?.clone()
                    }
                    SchemaType::AnyValue => return None,
                };
                match (kind, &object.format) {
                    (Type::String, Some(SchemaFormat::KnownFormat(KnownFormat::Uuid))) => {
                        Some(Value::from("3fa85f64-5717-4562-b3fc-2c963f66afa6"))
                    }
                    (Type::String, Some(SchemaFormat::KnownFormat(KnownFormat::DateTime))) => {
                        Some(Value::from(Self::date_time(name)))
                    }
                    (Type::String, _) => Some(Value::from(Self::string(name, description))),
                    (Type::Integer, _) => Some(Value::from(Self::integer(name))),
                    (Type::Boolean, _) => Some(Value::from(false)),
                    _ => None,
                }
            }
            // a schema that is not a plain type has no plain example
            _ => None,
        }
    }
}

impl Modify for OpenApiParameterExamples {
    /// Add an example to every required path, query, and header parameter that has a plain one
    ///
    /// # Arguments
    ///
    /// * `openapi` - The document whose parameters get examples
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // the schemas the parameters may reference
        let schemas = openapi
            .components
            .as_ref()
            .map_or_else(BTreeMap::new, |components| components.schemas.clone());
        for item in openapi.paths.paths.values_mut() {
            // every operation of every path
            let operations = [
                &mut item.get,
                &mut item.put,
                &mut item.post,
                &mut item.delete,
                &mut item.patch,
            ];
            for operation in operations.into_iter().flatten() {
                for parameter in operation.parameters.iter_mut().flatten() {
                    // only path, query, and header parameters get examples
                    if !matches!(
                        parameter.parameter_in,
                        ParameterIn::Path | ParameterIn::Query | ParameterIn::Header
                    ) {
                        continue;
                    }
                    // an optional parameter stays empty: Try it out sends every example it prefills, and
                    // an invented cursor, group or date range would change what the request asks for
                    if !matches!(parameter.required, Required::True) {
                        continue;
                    }
                    // build an example from the parameter's schema
                    let description = parameter.description.clone().unwrap_or_default();
                    let example = parameter.schema.as_ref().and_then(|schema| {
                        Self::example(&parameter.name, &description, schema, &schemas)
                    });
                    // set it on the parameter
                    if let Some(example) = example {
                        *parameter = ParameterBuilder::from(parameter.clone())
                            .example(Some(example))
                            .build();
                    }
                }
            }
        }
    }
}

/// Make a redirect that adds a trailing slash relative to the request
///
/// The directory services answer a directory url without its trailing slash with a redirect to the
/// absolute path they see, which is the path below the point they are nested at. Such a redirect
/// leaves the api prefix, and an ingress prefix, behind and lands on a page that does not exist. The
/// redirect only adds a slash, so naming the last segment with its slash reaches the same page from
/// any prefix.
///
/// # Arguments
///
/// * `response` - The response to fix
async fn relative_redirect(mut response: Response) -> Response {
    // only a redirect that names an absolute path needs to change
    if response.status().is_redirection() {
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .filter(|location| location.starts_with('/'));
        if let Some(location) = location {
            // the last segment, with the slash, is the same address relative to the request
            let last = location
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or_default();
            if let Ok(relative) = HeaderValue::from_str(&format!("{last}/")) {
                response.headers_mut().insert(header::LOCATION, relative);
            }
        }
    }
    response
}

/// Serve a directory of docs, answering a missing page with the not found image
///
/// # Arguments
///
/// * `dir` - The directory to serve
/// * `not_found` - The file to answer a missing page with
fn docs_service(dir: &Path, not_found: &Path) -> MethodRouter {
    // serve the directory with its redirects relative to the request
    get_service(ServeDir::new(dir).not_found_service(ServeFile::new(not_found)))
        .layer(axum::middleware::map_response(relative_redirect))
}

/// Serve our user docs
///
/// # Arguments
///
/// * `conf` - The Thorium config
fn user(conf: &Conf) -> MethodRouter {
    // serve the book from where the config says it is
    docs_service(
        conf.thorium.assets.user_docs.as_path(),
        &conf.thorium.assets.not_found,
    )
}

/// Serve our developer docs
///
/// # Arguments
///
/// * `conf` - The Thorium config
fn dev(conf: &Conf) -> MethodRouter {
    // serve the rustdoc output from where the config says it is
    docs_service(
        conf.thorium.assets.dev_docs.as_path(),
        &conf.thorium.assets.not_found,
    )
}

/// The path the router nests every api route under
///
/// The swagger UI fetches its documents from the browser's origin, so the urls it lists must carry this
/// prefix even though the documents' routes are registered without it.
pub(crate) const API_PREFIX: &str = "/api";

/// The name of the area an `OpenAPI` document describes, from the url it is served at
///
/// # Arguments
///
/// * `url` - The url the document is served at, such as `/pipelines/openapi.json`
fn area(url: &str) -> String {
    // the area is the url's path without the file name that ends most urls
    let path = url.trim_start_matches('/');
    let path = path.strip_suffix("openapi.json").unwrap_or(path);
    // the basic routes have no area in their url
    let path = path.trim_matches('/');
    if path.is_empty() {
        return "basic".to_owned();
    }
    // areas below another area, such as `search/events/results`, join their parts
    path.replace('/', "_")
}

/// Tidy an `OpenAPI` document before it is served
///
/// A route's doc comment becomes its description, so the Rust `# Arguments` list that ends most of
/// them is cut, since it names the handler's variables instead of the request's parts. Every
/// operation id is prefixed with the document's area so ids stay unique across the documents, every
/// operation is tagged with its area, and the title names the area.
///
/// # Arguments
///
/// * `url` - The url the document is served at
/// * `doc` - The document to tidy
fn tidy(url: &str, mut doc: utoipa::openapi::OpenApi) -> utoipa::openapi::OpenApi {
    // name the area this document describes
    let area = area(url);
    doc.info.title = format!("Thorium API: {area}");
    // tidy every operation of every path
    for item in doc.paths.paths.values_mut() {
        let operations = [
            &mut item.get,
            &mut item.put,
            &mut item.post,
            &mut item.delete,
            &mut item.patch,
        ];
        for operation in operations.into_iter().flatten() {
            // keep what the doc comment says before its Rust argument list
            if let Some(description) = operation.description.take() {
                let kept = description
                    .split("# Arguments")
                    .next()
                    .unwrap_or_default()
                    .trim();
                operation.description = (!kept.is_empty()).then(|| kept.to_owned());
            }
            // prefix the operation id with the area, because handler names repeat across areas
            if let Some(id) = operation.operation_id.take() {
                operation.operation_id = Some(format!("{area}_{id}"));
            }
            // group the operation under its area
            operation.tags = Some(vec![area.clone()]);
        }
    }
    doc
}

/// A text field of a multipart form
///
/// # Arguments
///
/// * `description` - What the field holds
fn form_text(description: &str) -> Value {
    // a plain string field
    serde_json::json!({"type": "string", "description": description})
}

/// A text field of a multipart form that is one of a set of names
///
/// # Arguments
///
/// * `description` - What the field holds
/// * `names` - The names the field may be
fn form_enum(description: &str, names: &[&str]) -> Value {
    // a string limited to the names, which Swagger UI renders as a menu
    serde_json::json!({"type": "string", "enum": names, "description": description})
}

/// A text field of a multipart form that may be sent more than once
///
/// # Arguments
///
/// * `description` - What each repeat of the field holds
fn form_texts(description: &str) -> Value {
    // a list of strings, one per repeat of the field
    serde_json::json!({"type": "array", "items": {"type": "string"}, "description": description})
}

/// A field of a multipart form that carries one file
///
/// # Arguments
///
/// * `description` - What the file holds
fn form_file(description: &str) -> Value {
    // a binary string is what Swagger UI renders as a file picker
    serde_json::json!({"type": "string", "format": "binary", "description": description})
}

/// A field of a multipart form that carries files and may be sent more than once
///
/// # Arguments
///
/// * `description` - What each file holds
fn form_files(description: &str) -> Value {
    // a list of binary strings, one file per repeat of the field
    serde_json::json!({"type": "array", "items": {"type": "string", "format": "binary"}, "description": description})
}

/// Build the schema of a multipart form from its named fields
///
/// # Arguments
///
/// * `fields` - The fields with a fixed name and the schema of each
/// * `required` - The names of the fields every request must send
/// * `dynamic` - What the fields whose names carry a key, such as `tags[<key>]`, hold
fn form_schema(fields: Vec<(&str, Value)>, required: &[&str], dynamic: &str) -> Value {
    // the fixed fields keep the order they were listed in
    let properties = fields
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect::<serde_json::Map<String, Value>>();
    // fields named with a key are described as one open set of strings
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": {"type": "string", "description": dynamic},
    })
}

/// The form of a result upload, which files and repos share
fn results_form() -> Value {
    // the tool, its result and how to render it, plus any result files
    form_schema(
        vec![
            ("groups", form_texts("The groups that can see this result")),
            ("tool", form_text("The tool that created this result")),
            (
                "tool_version",
                form_text("The version of the tool, a semantic version or any label"),
            ),
            ("cmd", form_text("The command that created this result")),
            ("result", form_text("The result itself")),
            (
                "display_type",
                form_enum(
                    "How to render the result",
                    &[
                        "JSON",
                        "String",
                        "Table",
                        "Image",
                        "Custom",
                        "Disassembly",
                        "HTML",
                        "Markdown",
                        "Hidden",
                        "XML",
                        "YAML",
                        "Decomposition",
                    ],
                ),
            ),
            (
                "files",
                form_files("Result files, each stored under the path in its file name"),
            ),
        ],
        &["tool", "result", "display_type"],
        "`extra` is extra data for the result as JSON, `entities_count[<kind>]` is how many entities of a kind the result holds, and `entities[<kind>]` is a JSON file of them with a content type set",
    )
}

/// The form of a file upload
fn upload_form() -> Value {
    // only the fields whose empty value parses are offered; the rest are described
    form_schema(
        vec![
            ("groups", form_texts("The groups to share the file with")),
            ("description", form_text("A description of the file")),
            (
                "data",
                form_file("The file itself, which must be the last field"),
            ),
        ],
        &["groups", "data"],
        "`tags[<key>]` is a tag value for a key, `trigger_depth` is how deep in a chain of triggered reactions the upload is, and `origin[origin_type]`, `origin[url]`, `origin[name]`, `origin[tool]`, `origin[parent]`, `origin[cmd]` and the other `origin[...]` fields say where the file came from (send only the ones you mean to set: an empty value does not parse)",
    )
}

/// The form that creates an entity
fn entity_create_form() -> Value {
    // the fields every kind shares; each kind's own settings are `metadata[...]` fields
    form_schema(
        vec![
            (
                "kind",
                form_text("The kind of entity to create, such as `Other`, `Vendor` or `Tool`"),
            ),
            ("name", form_text("The name of the entity")),
            ("description", form_text("A description of the entity")),
            ("groups[]", form_texts("The groups to create the entity in")),
            ("image", form_file("An image for the entity")),
        ],
        &["kind", "name", "groups[]"],
        "`tags[<key>][]` is a tag value for a key, and `metadata[...]` fields hold the kind's own settings",
    )
}

/// The form that updates an entity
fn entity_update_form() -> Value {
    // the fields every kind shares; each kind's own changes are `metadata[...]` fields
    form_schema(
        vec![
            ("description", form_text("A new description for the entity")),
            (
                "clear_description",
                form_enum("Whether to clear the description", &["false", "true"]),
            ),
            ("image", form_file("A new image for the entity")),
        ],
        &[],
        "`metadata[...]` fields change the kind's own settings",
    )
}

/// A form with a single field and nothing else
///
/// # Arguments
///
/// * `name` - The name of the field
/// * `field` - The schema of the field
fn single_field_form(name: &str, field: Value) -> Value {
    // the one field is required, and no other field is accepted
    form_schema(vec![(name, field)], &[name], "No other fields are accepted")
}

/// The multipart form a route reads, if it reads one
///
/// The routes read their forms field by field, so their handlers carry no type that describes them.
/// The names here are the ones the handlers accept.
///
/// # Arguments
///
/// * `method` - The lowercase method of the route
/// * `path` - The path of the route
fn multipart_form(method: &str, path: &str) -> Option<Value> {
    match (method, path) {
        ("post", "/api/files/") => Some(upload_form()),
        ("post", "/api/files/comment/{sha256}") => Some(form_schema(
            vec![
                ("comment", form_text("The comment")),
                ("files", form_files("Files to attach to the comment")),
            ],
            &["comment"],
            "No other fields are accepted",
        )),
        ("post", "/api/files/results/{sha256}" | "/api/repos/results/{repo_path}") => {
            Some(results_form())
        }
        ("post", "/api/repos/data/{repo_path}") => Some(form_schema(
            vec![
                ("groups", form_texts("The groups to share the data with")),
                (
                    "data",
                    form_file("The zipped repo, which must be the last field"),
                ),
            ],
            &["data"],
            "No other fields are accepted",
        )),
        ("patch", "/api/reactions/{group}/{id}/cache/files/") => Some(single_field_form(
            "files",
            form_files("The files to add, each stored under the path in its file name"),
        )),
        ("post", "/api/users/image") => Some(single_field_form(
            "image",
            form_file("The image to use as the user's icon"),
        )),
        ("post", "/api/entities/") => Some(entity_create_form()),
        ("patch", "/api/entities/{id}") => Some(entity_update_form()),
        // every other route reads no form
        _ => None,
    }
}

/// Whether a value of the wrong type can be sent for a query parameter with this schema
///
/// Plain strings (and lists of them) accept any text, so only parameters that must parse as a
/// number, a boolean, an id, a date or one of a set of names can be a bad request.
///
/// # Arguments
///
/// * `components` - The schemas the document holds, which a reference may name
/// * `schema` - The schema of the parameter
fn can_misparse(components: &Value, schema: &Value) -> bool {
    // follow a reference to the schema it names
    let schema = match schema["$ref"].as_str() {
        Some(reference) => &components["schemas"][reference.rsplit('/').next().unwrap_or_default()],
        None => schema,
    };
    // a set of names, a number, a boolean, an id or a date must parse
    if schema.get("enum").is_some() || schema.get("oneOf").is_some() {
        return true;
    }
    match schema["type"].as_str() {
        Some("array") => can_misparse(components, &schema["items"]),
        Some("string") => schema.get("format").is_some(),
        Some("integer" | "number" | "boolean") => true,
        _ => false,
    }
}

/// Fix what the handlers' types cannot say about one operation
///
/// Multipart bodies list their form fields, byte streams are binary strings, a body that must be
/// JSON answers 415 for any other content type, a query parameter answers 400 for a value of the
/// wrong type, and the tag filters are query objects sent as `tags[<key>]=<value>`.
///
/// # Arguments
///
/// * `method` - The lowercase method of the operation
/// * `path` - The path of the operation
/// * `components` - The schemas the document holds
/// * `operation` - The operation to fix, as JSON
fn polish_operation(method: &str, path: &str, components: &Value, operation: &mut Value) {
    // list the form fields of a multipart body
    if operation["requestBody"]["content"]["multipart/form-data"].is_object()
        && let Some(form) = multipart_form(method, path)
    {
        operation["requestBody"]["content"]["multipart/form-data"]["schema"] = form;
    }
    // describe a byte stream as a binary string instead of an array of numbers
    if let Some(responses) = operation["responses"].as_object_mut() {
        for response in responses.values_mut() {
            if response["content"]["application/octet-stream"]["schema"].is_object() {
                response["content"]["application/octet-stream"]["schema"] =
                    serde_json::json!({"type": "string", "format": "binary"});
            }
        }
    }
    // the status a route answers is not listed twice
    let refuses_writes = operation["responses"].get("410").is_some();
    // a JSON body sent with another content type is refused before the handler runs
    if operation["requestBody"]["content"]["application/json"].is_object()
        && !refuses_writes
        && operation["responses"].get("415").is_none()
    {
        operation["responses"]["415"] = serde_json::json!({
            "description": "The request's Content-Type is not application/json",
        });
    }
    // a query parameter with a value of the wrong type is a bad request
    let queried = operation["parameters"].as_array().is_some_and(|list| {
        list.iter()
            .any(|param| param["in"] == "query" && can_misparse(components, &param["schema"]))
    });
    if queried && !refuses_writes && operation["responses"].get("400").is_none() {
        operation["responses"]["400"] = serde_json::json!({
            "description": "The request is invalid: a query parameter has an invalid value",
        });
    }
    // a query object is sent as `name[key]=value`, which is the deep object style
    // (`get_mut` because indexing a missing member of a mutable value would add a null one)
    for parameter in operation
        .get_mut("parameters")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if parameter["in"] == "query" && parameter["schema"]["type"] == "object" {
            parameter["style"] = Value::from("deepObject");
            parameter["explode"] = Value::from(true);
            // a value is sent as plain text: a list would be sent as its JSON text, which matches nothing
            if parameter["schema"]["additionalProperties"]["type"] == "array" {
                parameter["schema"]["additionalProperties"] = serde_json::json!({"type": "string"});
                let note = "Each key takes one value here; to match several values of a key, send `name[key][]=a&name[key][]=b`.";
                let described = parameter["description"].as_str().unwrap_or_default();
                parameter["description"] =
                    Value::from(format!("{described} {note}").trim().to_owned());
            }
        }
    }
}

/// Fix what the handlers' types cannot say about the operations of a document
///
/// The fixed document is served as JSON, because the parts the fixes add, such as the fields of a
/// multipart form, are not all things the handlers' types can build.
///
/// # Arguments
///
/// * `doc` - The document to fix
fn polish(doc: &utoipa::openapi::OpenApi) -> Value {
    // edit the document as JSON, where every part of an operation can be reached
    let mut value = serde_json::to_value(doc).expect("a serializable document");
    // the schemas the operations' parameters may name
    let components = value["components"].clone();
    if let Some(paths) = value["paths"].as_object_mut() {
        for (path, item) in paths {
            for (method, operation) in item.as_object_mut().into_iter().flatten() {
                // only the methods hold operations
                if ["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                    polish_operation(method, path, &components, operation);
                }
            }
        }
    }
    value
}

/// Every `OpenAPI` document the swagger UI serves, with the url it is served at
fn served_docs() -> Vec<(String, utoipa::openapi::OpenApi)> {
    // the documents as their routes files describe them
    let docs = vec![
        ("/openapi.json".to_owned(), BasicApiDocs::openapi()),
        (
            "/associations/openapi.json".to_owned(),
            AssociationApiDocs::openapi(),
        ),
        (
            "/entities/openapi.json".to_owned(),
            EntityApiDocs::openapi(),
        ),
        ("/events/openapi.json".to_owned(), EventApiDocs::openapi()),
        ("/files/openapi.json".to_owned(), FileApiDocs::openapi()),
        ("/groups/openapi.json".to_owned(), GroupApiDocs::openapi()),
        ("/images/openapi.json".to_owned(), ImageApiDocs::openapi()),
        ("/jobs/openapi.json".to_owned(), JobApiDocs::openapi()),
        (
            "/networkpolicies/openapi.json".to_owned(),
            NetworkPolicyDocs::openapi(),
        ),
        (
            "/pipelines/openapi.json".to_owned(),
            PipelineApiDocs::openapi(),
        ),
        (
            "/reactions/openapi.json".to_owned(),
            ReactionApiDocs::openapi(),
        ),
        ("/repos/openapi.json".to_owned(), RepoApiDocs::openapi()),
        ("/search/openapi.json".to_owned(), SearchApiDocs::openapi()),
        (
            format!("/search/events/{}", ResultSearchEvent::url()),
            ResultSearchEventApiDocs::openapi(),
        ),
        (
            format!("/search/events/{}", TagSearchEvent::url()),
            TagSearchEventApiDocs::openapi(),
        ),
        ("/stream/openapi.json".to_owned(), StreamApiDocs::openapi()),
        ("/system/openapi.json".to_owned(), SystemApiDocs::openapi()),
        ("/trees/openapi.json".to_owned(), TreeApiDocs::openapi()),
        ("/users/openapi.json".to_owned(), UserApiDocs::openapi()),
        ("/oauth/openapi.json".to_owned(), OAuthApiDocs::openapi()),
    ];
    // tidy each one before it is served
    docs.into_iter()
        .map(|(url, doc)| {
            let doc = tidy(&url, doc);
            (url, doc)
        })
        .collect()
}

/// Every `OpenAPI` document the swagger UI serves as JSON, with the url it is served at
fn served_values() -> Vec<(String, Value)> {
    // fix what the handlers' types cannot say in each tidied document
    served_docs()
        .into_iter()
        .map(|(url, doc)| {
            // send requests to the root the document's paths are relative to
            let mut doc = polish(&doc);
            doc["servers"] =
                serde_json::json!([{"url": server_url(&url), "description": "This Thorium"}]);
            (url, doc)
        })
        .collect()
}

/// The urls the swagger UI fetches its documents from
///
/// Each url is relative to the UI's own page, which is served at `<prefix>/api/docs/swagger-ui/`, so
/// it keeps working behind an ingress that mounts Thorium under a path prefix: the page and the
/// documents move together.
fn swagger_urls() -> Vec<String> {
    // climb from the UI's page (`/api/docs/swagger-ui/`) to the api prefix, then go down to the document
    served_docs()
        .into_iter()
        .map(|(url, _)| format!("../..{url}"))
        .collect()
}

/// The url of the root the paths of a document are relative to, relative to where the document is served
///
/// Paths in the documents are absolute paths such as `/api/files/`, so the server a request is sent
/// to is the root of the host, or of the path prefix an ingress mounts Thorium under. Climbing from
/// the document's directory to that root keeps requests under the prefix the document was fetched from.
///
/// # Arguments
///
/// * `url` - The url the document is registered at, before the api prefix
fn server_url(url: &str) -> String {
    // the document is served at the api prefix plus its url, and every directory above it is one step up
    let served = format!("{API_PREFIX}{url}");
    let depth = served.trim_start_matches('/').split('/').count() - 1;
    "../".repeat(depth)
}

/// Build the swagger UI with every document we serve
///
/// The routes of the documents are registered relative to the router the UI is merged into, but the UI
/// lists the urls its browser requests, so its config names them relative to its own page. The UI's
/// redirect from its url without a trailing slash is made relative too, so it stays under the api
/// prefix and under an ingress prefix.
fn swagger_ui<S: Clone + Send + Sync + 'static>() -> Router<S> {
    // add every document we serve to the swagger ui, which has the browser fetch them relative to its page
    let ui = served_values()
        .into_iter()
        .fold(SwaggerUi::new("/docs/swagger-ui"), |ui, (url, doc)| {
            ui.external_url_unchecked(url, doc)
        })
        .config(Config::new(swagger_urls()));
    // keep the UI's redirect inside the prefix it was reached under
    Router::from(ui).layer(axum::middleware::map_response(relative_redirect))
}

/// Add the docs routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>, conf: &Conf) -> Router<AppState> {
    router
        .nest_service("/docs/user", user(conf))
        .nest_service("/docs/dev", dev(conf))
        .merge(swagger_ui())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use regex::Regex;
    use serde_json::Value;
    use tower::ServiceExt;

    use super::{served_docs, served_values};
    use crate::models::search::events::SearchEvent;
    use crate::models::{ResultSearchEvent, TagSearchEvent};

    /// The routes files that mount REST handlers, with their source
    ///
    /// Every handler they mount is documented, and every document they serve describes exactly
    /// the routes they mount.
    const REST_FILES: [(&str, &str); 20] = [
        ("associations", include_str!("associations.rs")),
        ("basic", include_str!("basic.rs")),
        ("entities", include_str!("entities.rs")),
        ("events", include_str!("events.rs")),
        ("files", include_str!("files.rs")),
        ("groups", include_str!("groups.rs")),
        ("images", include_str!("images.rs")),
        ("jobs", include_str!("jobs.rs")),
        ("network_policies", include_str!("network_policies.rs")),
        ("oauth", include_str!("oauth.rs")),
        ("pipelines", include_str!("pipelines.rs")),
        ("reactions", include_str!("reactions.rs")),
        ("repos", include_str!("repos.rs")),
        ("search", include_str!("search.rs")),
        (
            "search/events/results",
            include_str!("search/events/results.rs"),
        ),
        ("search/events/tags", include_str!("search/events/tags.rs")),
        ("streams", include_str!("streams.rs")),
        ("system", include_str!("system.rs")),
        ("trees", include_str!("trees.rs")),
        ("users", include_str!("users.rs")),
    ];

    /// The routes files that mount protocol or static endpoints instead of REST handlers, so
    /// no `OpenAPI` document describes them
    ///
    /// The MCP server speaks the streamable HTTP MCP protocol on one path, the binaries and
    /// ui routes serve files, and the docs routes serve the documents themselves.
    const NOT_REST_FILES: [(&str, &str); 4] = [
        ("binaries", include_str!("binaries.rs")),
        ("docs", include_str!("docs.rs")),
        ("mcp", include_str!("mcp.rs")),
        ("ui", include_str!("ui.rs")),
    ];

    /// The source of a routes file without its tests or commented out lines
    ///
    /// # Arguments
    ///
    /// * `source` - The source of a routes file
    fn code_of(source: &str) -> String {
        // only the code before the file's tests counts, without its commented out lines
        source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(source)
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Change a router path into the template an annotation must use
    ///
    /// The router names a wildcard `{*name}` and the document names it `{name}`.
    ///
    /// # Arguments
    ///
    /// * `route` - The path the router mounts a handler at, without the `/api` prefix
    fn document_path(route: &str) -> String {
        // the router nests every route under the api prefix, so its root route answers at the prefix itself
        if route == "/" {
            return "/api".to_owned();
        }
        // every other route is served under the api prefix, and wildcards lose their star
        format!("/api{}", route.replace("{*", "{"))
    }

    /// The (method, path, handler) a routes file mounts, with paths as documents spell them
    ///
    /// # Arguments
    ///
    /// * `code` - The code of a routes file
    fn mounts(code: &str) -> Vec<(String, String, String)> {
        // the mount function's body
        let mount = Regex::new(r"fn mount\(")
            .expect("a mount pattern")
            .split(code)
            .nth(1)
            .expect("a mount function");
        // the handlers each route chains onto its path
        let handler =
            Regex::new(r"\b(get|post|patch|put|delete)\(\s*(?:[a-z_]+::)*([a-z_][a-z0-9_]*)\s*\)")
                .expect("a handler pattern");
        // a route's path is the first string in its call
        let path = Regex::new(r#""([^"]+)""#).expect("a path pattern");
        let mut found = Vec::new();
        for chunk in mount.split(".route(").skip(1) {
            // a route without a literal path can't be checked
            let Some(route) = path.captures(chunk) else {
                continue;
            };
            for capture in handler.captures_iter(chunk) {
                found.push((
                    capture[1].to_owned(),
                    document_path(&route[1]),
                    capture[2].to_owned(),
                ));
            }
        }
        found
    }

    /// The (method, path, handler, annotation body) of every `#[utoipa::path]` in a routes file
    ///
    /// # Arguments
    ///
    /// * `code` - The code of a routes file
    fn annotations(code: &str) -> Vec<(String, String, String, String)> {
        // an annotation names its method and path, and sits above the handler it documents
        let pattern = Regex::new(
            r#"(?s)#\[utoipa::path\(\s*(\w+),\s*path\s*=\s*"([^"]+)"(.*?)\n\s*\)\]\s*(?:#\[[^\]]*\]\s*)*(?:pub )?async fn (\w+)\("#,
        )
        .expect("an annotation pattern");
        pattern
            .captures_iter(code)
            .map(|found| {
                (
                    found[1].to_owned(),
                    found[2].to_owned(),
                    found[4].to_owned(),
                    found[3].to_owned(),
                )
            })
            .collect()
    }

    /// The handlers a routes file's `OpenAPI` docs list
    ///
    /// # Arguments
    ///
    /// * `code` - The code of a routes file
    fn listed(code: &str) -> BTreeSet<String> {
        // the names in the docs struct's `paths(...)`
        Regex::new(r"(?s)#\[openapi\(\s*paths\(([^)]*)\)")
            .expect("a paths pattern")
            .captures(code)
            .map_or_else(BTreeSet::new, |found| {
                found[1]
                    .split(',')
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
    }

    /// Every handler a routes file mounts is in its `OpenAPI` docs, and no docs list a handler
    /// that isn't mounted
    #[test]
    fn mounted_handlers_are_documented() {
        for (name, source) in REST_FILES {
            // the handlers the file mounts and the handlers its docs list
            let code = code_of(source);
            let routed = mounts(&code)
                .into_iter()
                .map(|(_, _, handler)| handler)
                .collect::<BTreeSet<_>>();
            let listed = listed(&code);
            assert!(!routed.is_empty(), "no handlers were found in {name}");
            assert!(!listed.is_empty(), "no documented paths in {name}");
            // nothing mounted may be missing from the docs, and nothing listed may be unmounted
            let missing = routed.difference(&listed).collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{name} mounts routes without docs: {missing:?}"
            );
            let extra = listed.difference(&routed).collect::<Vec<_>>();
            assert!(
                extra.is_empty(),
                "{name} documents unmounted handlers: {extra:?}"
            );
        }
    }

    /// The protocol and static files mount routes, and none of them serves `OpenAPI` docs
    #[test]
    fn protocol_endpoints_are_not_documented() {
        for (name, source) in NOT_REST_FILES {
            // the file mounts routes but registers no docs or annotations
            let code = code_of(source);
            assert!(code.contains("pub fn mount("), "{name} mounts no routes");
            assert!(
                !code.contains("#[openapi(") || name == "docs",
                "{name} gained docs: move it to REST_FILES"
            );
            assert!(
                !code.contains("#[utoipa::path("),
                "{name} has annotations: move it to REST_FILES"
            );
        }
    }

    /// Every annotation documents the method and path the router mounts its handler at, with
    /// the router's trailing slash and parameter names, and every mounted route is annotated
    ///
    /// The router is the source of truth: a trailing slash is part of a route.
    #[test]
    fn annotations_match_the_router() {
        // the problems found across all files
        let mut problems = Vec::new();
        // path parameters name exactly the template's names
        let braces = Regex::new(r"\{(\w+)\}").expect("a brace pattern");
        // parameter entries that name a type but no location
        let located = Regex::new(r#"\(\s*"(\w+)"\s*=\s*[^,()]+(?:<[^>]*>)?\s*,\s*description"#)
            .expect("a located pattern");
        let pathed = Regex::new(r#"\(\s*"(\w+)"\s*=\s*[^,()]+(?:<[^>]*>)?\s*,\s*Path"#)
            .expect("a path pattern");
        for (name, source) in REST_FILES {
            // search event routes build their paths at runtime and are checked by the generated docs
            if name.starts_with("search/events") {
                continue;
            }
            let code = code_of(source);
            let routed = mounts(&code);
            let noted = annotations(&code);
            // every annotation matches a mount of its handler
            for (method, path, handler, body) in &noted {
                let wanted = (method.clone(), path.clone(), handler.clone());
                if !routed.contains(&wanted) {
                    let at = routed
                        .iter()
                        .filter(|(_, _, mounted)| mounted == handler)
                        .collect::<Vec<_>>();
                    problems.push(format!(
                        "{name}::{handler}: annotated {method} {path}, mounted {at:?}"
                    ));
                }
                // path parameters are declared once each, and nothing else is declared as one
                let template = braces
                    .captures_iter(path)
                    .map(|found| found[1].to_owned())
                    .collect::<BTreeSet<_>>();
                let declared = pathed
                    .captures_iter(body)
                    .map(|found| found[1].to_owned())
                    .collect::<BTreeSet<_>>();
                if template != declared {
                    problems.push(format!("{name}::{handler}: path parameters {declared:?} against template {template:?}"));
                }
                // a parameter without a location would document a phantom path parameter
                for found in located.captures_iter(body) {
                    problems.push(format!(
                        "{name}::{handler}: parameter {} has no location",
                        &found[1]
                    ));
                }
            }
            // every mounted route has an annotation for its method and path
            for (method, path, handler) in &routed {
                if !noted
                    .iter()
                    .any(|(m, p, h, _)| m == method && p == path && h == handler)
                {
                    problems.push(format!(
                        "{name}::{handler}: mounted {method} {path} has no annotation"
                    ));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "annotations disagree with the router:\n{}",
            problems.join("\n")
        );
    }

    /// Every served document, as the JSON the swagger ui renders it from
    fn served_json() -> Vec<(String, Value)> {
        served_values()
    }

    /// Collect the references in a JSON value
    ///
    /// # Arguments
    ///
    /// * `value` - The value to search
    /// * `found` - The references found so far
    fn collect_refs(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    // a reference names a component the document must hold
                    if key == "$ref" {
                        if let Some(reference) = inner.as_str() {
                            found.insert(reference.to_owned());
                        }
                    } else {
                        collect_refs(inner, found);
                    }
                }
            }
            Value::Array(list) => list.iter().for_each(|inner| collect_refs(inner, found)),
            _ => (),
        }
    }

    /// Every reference in every served document names a schema that document holds
    #[test]
    fn served_documents_resolve_their_references() {
        // the references that dangle across all documents
        let mut dangling = BTreeSet::new();
        for (url, doc) in served_json() {
            // the schemas this document holds
            let schemas = doc["components"]["schemas"]
                .as_object()
                .map_or_else(BTreeSet::new, |map| {
                    map.keys().cloned().collect::<BTreeSet<_>>()
                });
            // the references it makes
            let mut refs = BTreeSet::new();
            collect_refs(&doc, &mut refs);
            // find the references that name no schema in this document
            for reference in refs {
                let name = reference.strip_prefix("#/components/schemas/");
                if name.is_none_or(|name| !schemas.contains(name)) {
                    dangling.insert((url.clone(), name.unwrap_or(&reference).to_owned()));
                }
            }
        }
        assert!(dangling.is_empty(), "dangling references: {dangling:?}");
    }

    /// The served documents are well formed `OpenAPI` 3 documents
    ///
    /// Each names its version and info, templates every path with braces, gives every operation a
    /// unique id and a response with a description, and declares exactly the path parameters
    /// its template names, each required.
    #[test]
    fn served_documents_are_well_formed() {
        // the problems found across all documents
        let mut problems = Vec::new();
        let braces = Regex::new(r"\{(\w+)\}").expect("a brace pattern");
        for (url, doc) in served_json() {
            // a document names its version and describes itself
            if !doc["openapi"].as_str().is_some_and(|v| v.starts_with("3.")) {
                problems.push(format!("{url}: no OpenAPI 3 version"));
            }
            if doc["info"]["title"].as_str().unwrap_or_default().is_empty() {
                problems.push(format!("{url}: no title"));
            }
            let mut ids = BTreeMap::<String, usize>::new();
            for (path, item) in doc["paths"].as_object().expect("paths") {
                // a path is templated with braces only
                if path.contains(':') || path.contains('*') {
                    problems.push(format!("{url}: {path} is not templated with braces"));
                }
                let template = braces
                    .captures_iter(path)
                    .map(|found| found[1].to_owned())
                    .collect::<BTreeSet<_>>();
                for (method, operation) in item.as_object().expect("a path item") {
                    // every operation names its id once per document
                    if let Some(id) = operation["operationId"].as_str() {
                        *ids.entry(id.to_owned()).or_default() += 1;
                    }
                    // path parameters are the template's names, each required, once
                    let mut declared = Vec::new();
                    for parameter in operation["parameters"].as_array().into_iter().flatten() {
                        if parameter["in"] == "path" {
                            declared
                                .push(parameter["name"].as_str().unwrap_or_default().to_owned());
                            if parameter["required"] != true {
                                problems.push(format!(
                                    "{url}: {method} {path} path parameter {} is not required",
                                    parameter["name"]
                                ));
                            }
                        }
                    }
                    let declared_set = declared.iter().cloned().collect::<BTreeSet<_>>();
                    if declared_set != template || declared.len() != declared_set.len() {
                        problems.push(format!("{url}: {method} {path} declares path parameters {declared:?} against template {template:?}"));
                    }
                    // an operation has responses, and each describes itself
                    let responses = operation["responses"].as_object().expect("responses");
                    if responses.is_empty() {
                        problems.push(format!("{url}: {method} {path} has no responses"));
                    }
                    for (status, response) in responses {
                        if response["description"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .is_empty()
                        {
                            problems.push(format!(
                                "{url}: {method} {path} {status} has no description"
                            ));
                        }
                    }
                }
            }
            // no operation id repeats inside a document
            for (id, count) in ids {
                if count > 1 {
                    problems.push(format!("{url}: operation id {id} repeats {count} times"));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "malformed documents:\n{}",
            problems.join("\n")
        );
    }

    /// The arguments of a handler, as its signature spells them
    ///
    /// # Arguments
    ///
    /// * `code` - The code of the routes file the handler is in
    /// * `handler` - The name of the handler
    fn handler_args(code: &str, handler: &str) -> String {
        // the arguments run from the handler's opening paren to the paren before its return type
        Regex::new(&format!(r"(?s)async fn {handler}\((.*?)\)\s*->"))
            .expect("a handler signature pattern")
            .captures(code)
            .map_or_else(String::new, |found| found[1].to_owned())
    }

    /// A route lists a security requirement exactly when its handler authenticates the caller
    ///
    /// The API reads credentials only through the `AuthedUser` and `AuthResponse` extractors, so a
    /// handler without one needs no credentials and its operation must not claim any.
    #[test]
    fn security_requirements_match_the_extractors() {
        // the problems found across all files
        let mut problems = Vec::new();
        for (name, source) in REST_FILES {
            // search event routes build their paths at runtime and are checked through the generated docs
            if name.starts_with("search/events") {
                continue;
            }
            let code = code_of(source);
            for (method, path, handler, body) in annotations(&code) {
                // a handler authenticates its caller through one of the two credential extractors
                let authenticates = {
                    let args = handler_args(&code, &handler);
                    args.contains("AuthedUser") || args.contains("AuthResponse")
                };
                let secured = body.contains("security(");
                if authenticates != secured {
                    problems.push(format!(
                        "{name}::{handler}: {method} {path} authenticates {authenticates} but lists security {secured}"
                    ));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "security disagrees with the extractors:\n{}",
            problems.join("\n")
        );
    }

    /// Every secured operation accepts basic or bearer credentials, and its document declares both
    /// schemes with a description
    #[test]
    fn secured_operations_accept_basic_or_bearer() {
        // the problems found across all documents
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            // the schemes this document declares
            let schemes = &doc["components"]["securitySchemes"];
            for (scheme, kind) in [("basic", "basic"), ("bearer", "bearer")] {
                if schemes[scheme]["type"] != "http" || schemes[scheme]["scheme"] != kind {
                    problems.push(format!("{url}: no http {kind} security scheme"));
                }
                if schemes[scheme]["description"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
                {
                    problems.push(format!("{url}: the {scheme} scheme has no description"));
                }
            }
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    // an operation either lists no security or lists basic and bearer as alternatives
                    let wanted = serde_json::json!([{"basic": []}, {"bearer": []}]);
                    match operation.get("security") {
                        None => (),
                        Some(security) if *security == wanted => (),
                        Some(security) => problems
                            .push(format!("{url}: {method} {path} lists security {security}")),
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "unexpected security requirements:\n{}",
            problems.join("\n")
        );
    }

    /// Every operation documents the statuses its inputs and credentials can cause
    ///
    /// A secured operation answers 401 for missing or refused credentials, an operation that takes a
    /// body answers 400 for one that is malformed (unless it refuses every write with 410), an operation
    /// that takes a JSON body answers 415 for another content type, an operation with a query parameter
    /// answers 400 for a value of the wrong type, and every operation but the two that touch no backing
    /// service answers 500 when one fails.
    #[test]
    fn operations_document_the_statuses_their_inputs_cause() {
        // the problems found across all documents
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    let responses = &operation["responses"];
                    // the statuses this operation lists a response for
                    let mut wanted = Vec::new();
                    if operation.get("security").is_some() {
                        wanted.push("401");
                    }
                    // a body is read only after a write that is refused outright, so those never answer 400
                    if operation.get("requestBody").is_some() && responses.get("410").is_none() {
                        wanted.push("400");
                    }
                    // a JSON body sent with another content type is refused before the handler runs
                    if operation["requestBody"]["content"]["application/json"].is_object()
                        && responses.get("410").is_none()
                    {
                        wanted.push("415");
                    }
                    // a query parameter that must parse, with a value that does not, is a bad request
                    let queried = operation["parameters"].as_array().is_some_and(|list| {
                        list.iter().any(|param| {
                            param["in"] == "query"
                                && super::can_misparse(&doc["components"], &param["schema"])
                        })
                    });
                    if queried && responses.get("410").is_none() && !wanted.contains(&"400") {
                        wanted.push("400");
                    }
                    if !["/api/health", "/api/banner"].contains(&path.as_str()) {
                        wanted.push("500");
                    }
                    for status in wanted {
                        if responses.get(status).is_none() {
                            problems.push(format!("{url}: {method} {path} lists no {status}"));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "operations missing statuses:\n{}",
            problems.join("\n")
        );
    }

    /// Operation ids are unique across every served document, every operation is tagged with its
    /// area, and no description carries the Rust argument list of its handler
    #[test]
    fn operations_are_identified_and_described_for_readers() {
        // the ids seen so far and the problems found across all documents
        let mut seen = BTreeSet::new();
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    // ids stay unique across documents so tools that merge them keep working
                    let id = operation["operationId"].as_str().unwrap_or_default();
                    if id.is_empty() || !seen.insert(id.to_owned()) {
                        problems.push(format!("{url}: {method} {path} has the id {id:?}"));
                    }
                    // the Rust argument list names variables that are not parts of the request
                    let description = operation["description"].as_str().unwrap_or_default();
                    if description.contains("# Arguments") {
                        problems.push(format!("{url}: {method} {path} lists Rust arguments"));
                    }
                    // an operation is grouped under its area
                    if operation["tags"].as_array().is_none_or(Vec::is_empty) {
                        problems.push(format!("{url}: {method} {path} has no tag"));
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "operations are hard to read:\n{}",
            problems.join("\n")
        );
    }

    /// The image create and update operations answer 410 Gone and are marked deprecated, and no
    /// other operation is deprecated
    #[test]
    fn removed_image_writes_are_deprecated() {
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    // the image routes that refuse every write
                    let removed = url == "/images/openapi.json"
                        && matches!(
                            (method.as_str(), path.as_str()),
                            ("post", "/api/images/") | ("patch", "/api/images/{group}/{image}")
                        );
                    assert_eq!(
                        operation["deprecated"] == true,
                        removed,
                        "{url}: {method} {path} has the wrong deprecated flag"
                    );
                    assert!(
                        !removed || operation["responses"].get("410").is_some(),
                        "{url}: {method} {path} lists no 410"
                    );
                }
            }
        }
    }

    /// Check that an example parses as the schema it is given for
    ///
    /// # Arguments
    ///
    /// * `doc` - The document that holds the schemas a reference may name
    /// * `schema` - The schema the example must match
    /// * `example` - The example to check
    fn example_matches(doc: &Value, schema: &Value, example: &Value) -> Result<(), String> {
        // follow a reference to the schema it names
        let schema = match schema["$ref"].as_str() {
            Some(reference) => {
                let name = reference.rsplit('/').next().unwrap_or_default();
                &doc["components"]["schemas"][name]
            }
            None => schema,
        };
        // an enum example is one of the enum's values
        if let Some(values) = schema["enum"].as_array() {
            return if values.contains(example) {
                Ok(())
            } else {
                Err(format!("{example} is not one of {values:?}"))
            };
        }
        // the type the example must have is the first one that is not null
        let kind = match &schema["type"] {
            Value::String(kind) => kind.as_str(),
            Value::Array(kinds) => kinds
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| *kind != "null")
                .unwrap_or("null"),
            _ => return Ok(()),
        };
        // check the example against the type and format
        let format = schema["format"].as_str().unwrap_or_default();
        match kind {
            "string" => {
                let text = example.as_str().ok_or("not a string")?;
                match format {
                    "uuid" => uuid::Uuid::parse_str(text)
                        .map(|_| ())
                        .map_err(|error| format!("{text} is not a uuid: {error}")),
                    "date-time" => chrono::DateTime::parse_from_rfc3339(text)
                        .map(|_| ())
                        .map_err(|error| format!("{text} is not a date time: {error}")),
                    _ => Ok(()),
                }
            }
            "integer" => {
                let number = example.as_i64().ok_or("not an integer")?;
                if format == "int32" && i32::try_from(number).is_err() {
                    return Err(format!("{number} does not fit an int32"));
                }
                Ok(())
            }
            "number" if example.is_number() => Ok(()),
            "boolean" if example.is_boolean() => Ok(()),
            "object" if example.is_object() => Ok(()),
            "array" => {
                let items = example.as_array().ok_or("not an array")?;
                items
                    .iter()
                    .try_for_each(|item| example_matches(doc, &schema["items"], item))
            }
            other => Err(format!("{example} is not a {other}")),
        }
    }

    /// Every required parameter has an example that parses as its declared schema, and no optional
    /// parameter has one
    ///
    /// Swagger UI's Try it out sends every example it prefills, so an example on an optional
    /// parameter, such as a made up cursor, group or date range, changes what the request asks for
    /// and makes list routes answer 404 until the example is cleared. Object parameters, such as the
    /// tag filters, have no plain example.
    #[test]
    fn parameter_examples_match_their_schemas() {
        // the problems found across all documents
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    for parameter in operation["parameters"].as_array().into_iter().flatten() {
                        let name = parameter["name"].as_str().unwrap_or_default();
                        let schema = &parameter["schema"];
                        // an optional parameter is left empty so Try it out never sends an invented value
                        if parameter["required"] != true {
                            if parameter.get("example").is_some() {
                                problems.push(format!(
                                    "{url}: {method} {path} optional parameter {name} has an example"
                                ));
                            }
                            continue;
                        }
                        // an object parameter has no plain example to give
                        let target = match schema["$ref"].as_str() {
                            Some(reference) => {
                                let id = reference.rsplit('/').next().unwrap_or_default();
                                &doc["components"]["schemas"][id]
                            }
                            None => schema,
                        };
                        if target["type"] == "object" {
                            continue;
                        }
                        // every other required parameter has an example that parses as its schema
                        match parameter.get("example") {
                            None => problems.push(format!(
                                "{url}: {method} {path} parameter {name} has no example"
                            )),
                            Some(example) => {
                                if let Err(error) = example_matches(&doc, schema, example) {
                                    problems.push(format!(
                                        "{url}: {method} {path} parameter {name}: {error}"
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "parameter examples are missing or wrong:\n{}",
            problems.join("\n")
        );
    }

    /// Every route the REST files mount appears, with its method, in a served document
    #[test]
    fn every_mounted_route_is_served() {
        // every path and method the served documents hold
        let mut served = BTreeSet::new();
        for (_, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for method in item.as_object().expect("a path item").keys() {
                    served.insert((method.clone(), path.clone()));
                }
            }
        }
        // the routes the files mount, with the search event urls filled in
        let mut missing = Vec::new();
        for (name, source) in REST_FILES {
            let url = if name.ends_with("results") {
                ResultSearchEvent::url()
            } else if name.ends_with("tags") {
                TagSearchEvent::url()
            } else {
                ""
            };
            for (method, path, handler) in mounts(&code_of(source)) {
                let path = path.replace("{url}", url);
                if !served.contains(&(method.clone(), path.clone())) {
                    missing.push(format!("{name}::{handler}: {method} {path}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "mounted routes no document serves:\n{}",
            missing.join("\n")
        );
    }

    /// Fetch a path from a router, returning the status, the content type, and the body
    ///
    /// # Arguments
    ///
    /// * `app` - The router to request from
    /// * `path` - The path to request
    async fn fetch(app: &axum::Router, path: &str) -> (u16, String, Vec<u8>) {
        // ask the router the way a browser would
        let request = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("a request");
        let response = app.clone().oneshot(request).await.expect("a response");
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        (status, content_type, body.to_vec())
    }

    /// The urls the swagger UI lists are the documents, as the api serves them, with or without a prefix
    ///
    /// The UI is nested under the api prefix like the real router. The urls it lists are relative to
    /// its own page, so resolving them from `/api/docs/swagger-ui/` and from the same page behind an
    /// ingress prefix (`/thorium/api/docs/swagger-ui/`) reaches the same documents under each prefix.
    #[tokio::test]
    async fn swagger_ui_lists_the_urls_the_documents_are_served_at() {
        // nest the UI under the api prefix like the real router
        let app = axum::Router::new().nest(super::API_PREFIX, super::swagger_ui::<()>());
        // read the urls the UI's config lists
        let (status, _, body) = fetch(&app, "/api/docs/swagger-ui/swagger-initializer.js").await;
        assert_eq!(status, 200, "the UI config");
        let listed = Regex::new(r#""url":\s*"([^"]+)""#)
            .expect("a url pattern")
            .captures_iter(&String::from_utf8_lossy(&body))
            .map(|found| found[1].to_owned())
            .collect::<Vec<_>>();
        // the UI lists every document
        assert_eq!(listed, super::swagger_urls());
        assert_eq!(listed.len(), served_docs().len());
        for prefix in ["", "/thorium"] {
            // the page the browser is on, with and without an ingress prefix
            let page =
                url::Url::parse(&format!("http://thorium.test{prefix}/api/docs/swagger-ui/"))
                    .expect("a page url");
            for relative in &listed {
                // resolve the listed url the way the browser does
                let resolved = page.join(relative).expect("a relative url");
                // the ingress prefix is kept, and what follows it is where the router serves the document
                let served = resolved
                    .path()
                    .strip_prefix(prefix)
                    .unwrap_or_else(|| panic!("{relative} left the prefix {prefix}"));
                assert!(
                    served.starts_with("/api/"),
                    "{relative} resolves to {served}"
                );
                let (status, content_type, body) = fetch(&app, served).await;
                assert_eq!(status, 200, "{relative} behind {prefix:?}");
                assert!(content_type.contains("json"), "{relative}: {content_type}");
                let doc: Value = serde_json::from_slice(&body).expect("a JSON document");
                assert!(
                    doc["openapi"].is_string() && doc["paths"].is_object(),
                    "{relative}"
                );
            }
        }
        // a url without the api prefix is not a document the api serves
        for (url, _) in served_docs() {
            assert_eq!(
                fetch(&app, &url).await.0,
                404,
                "{url} is served without the prefix"
            );
        }
    }

    /// Requests from the Swagger UI go to the host root or to the same path prefix the document came from
    ///
    /// Every document names the server its absolute paths are relative to as a relative url, so
    /// resolving it from the document's own url, with and without an ingress prefix, gives the root.
    #[test]
    fn documents_name_a_server_that_keeps_the_ingress_prefix() {
        for (url, doc) in served_json() {
            let relative = doc["servers"][0]["url"].as_str().expect("a server url");
            for prefix in ["", "/thorium"] {
                // where the browser fetched this document from
                let fetched = url::Url::parse(&format!("http://thorium.test{prefix}/api{url}"))
                    .expect("a document url");
                // the server resolves to the root of the host or of the prefix
                let server = fetched.join(relative).expect("a relative server url");
                assert_eq!(
                    server.path().trim_end_matches('/'),
                    prefix,
                    "{url} behind {prefix:?}"
                );
            }
        }
    }

    /// Every multipart request body describes its form fields instead of a byte array
    ///
    /// The multipart routes read named form fields, so a body documented as an array of numbers gives
    /// the Swagger UI nothing to fill in. Each form is an object with named properties, and every
    /// property that carries a file is a binary string.
    #[test]
    fn multipart_bodies_describe_their_form_fields() {
        // the problems found across all documents
        let mut problems = Vec::new();
        // how many multipart bodies were checked
        let mut checked = 0;
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    let form = &operation["requestBody"]["content"]["multipart/form-data"];
                    if !form.is_object() {
                        continue;
                    }
                    checked += 1;
                    let schema = &form["schema"];
                    // the form is an object that names its fields
                    let properties = schema["properties"].as_object();
                    if schema["type"] != "object"
                        || properties.is_none_or(serde_json::Map::is_empty)
                    {
                        problems.push(format!("{url}: {method} {path} describes no form fields"));
                        continue;
                    }
                    // the fields that carry files are binary strings
                    let binary = properties
                        .into_iter()
                        .flatten()
                        .filter(|(_, field)| {
                            field["format"] == "binary" || field["items"]["format"] == "binary"
                        })
                        .count();
                    let upload = !path.contains("/entities/") || method == "post";
                    if upload && binary == 0 && !path.ends_with("/tags/{id}") {
                        problems.push(format!("{url}: {method} {path} has no binary field"));
                    }
                }
            }
        }
        // the routes that read multipart forms are all documented as forms
        assert!(checked >= 9, "only {checked} multipart bodies were found");
        assert!(
            problems.is_empty(),
            "multipart bodies are not described:\n{}",
            problems.join("\n")
        );
    }

    /// Byte stream responses are documented as binary strings, never as arrays of numbers
    #[test]
    fn binary_responses_are_binary_strings() {
        // the problems found across all documents
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    for (status, response) in operation["responses"].as_object().expect("responses")
                    {
                        let stream = &response["content"]["application/octet-stream"]["schema"];
                        if stream.is_object()
                            && (stream["type"] != "string" || stream["format"] != "binary")
                        {
                            problems.push(format!(
                                "{url}: {method} {path} {status} is not a binary string"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "byte streams are not binary strings:\n{}",
            problems.join("\n")
        );
    }

    /// The statuses a route answers beyond its credentials and inputs are documented
    ///
    /// Each entry is a route that answers a status its handler's own checks produce: an unknown user
    /// to log out is a 404, a bulk reaction request naming a missing pipeline is a 404, and the OAuth
    /// link routes answer 400 when their query parameters are missing.
    #[test]
    fn statuses_found_by_calling_the_routes_are_documented() {
        // the routes and the extra statuses they answer
        let wanted = [
            ("get", "/api/users/logout/{target}", "404"),
            ("post", "/api/reactions/bulk/", "404"),
            ("post", "/api/oauth/{name}/link", "400"),
            ("delete", "/api/oauth/{name}/link", "400"),
        ];
        // the problems found across all documents
        let mut problems = Vec::new();
        let docs = served_json();
        for (method, path, status) in wanted {
            let operation = docs
                .iter()
                .find_map(|(_, doc)| doc["paths"][path].get(method));
            match operation {
                None => problems.push(format!("{method} {path} is not documented")),
                Some(operation) if operation["responses"].get(status).is_none() => {
                    problems.push(format!("{method} {path} lists no {status}"));
                }
                Some(_) => (),
            }
        }
        assert!(
            problems.is_empty(),
            "statuses missing:\n{}",
            problems.join("\n")
        );
    }

    /// No operation holds a member whose value is null, such as `"parameters": null`
    ///
    /// OpenAPI members are absent when empty, and a null parameter list is not a list.
    #[test]
    fn operations_hold_no_null_members() {
        // the problems found across all documents
        let mut problems = Vec::new();
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    for (member, value) in operation.as_object().expect("an operation") {
                        if value.is_null() {
                            problems.push(format!("{url}: {method} {path} has a null {member}"));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "null members:\n{}",
            problems.join("\n")
        );
    }

    /// Query objects are deep objects whose values are plain text
    ///
    /// The tag filters are sent as `tags[key]=value`. A value typed as a list is sent as its JSON
    /// text by the Swagger UI, which matches no tag at all, so each value is documented as a string.
    #[test]
    fn query_objects_are_deep_objects_of_strings() {
        // the problems found across all documents
        let mut problems = Vec::new();
        let mut checked = 0;
        for (url, doc) in served_json() {
            for (path, item) in doc["paths"].as_object().expect("paths") {
                for (method, operation) in item.as_object().expect("a path item") {
                    for parameter in operation["parameters"].as_array().into_iter().flatten() {
                        if parameter["in"] != "query" || parameter["schema"]["type"] != "object" {
                            continue;
                        }
                        checked += 1;
                        let name = parameter["name"].as_str().unwrap_or_default();
                        if parameter["style"] != "deepObject" || parameter["explode"] != true {
                            problems.push(format!(
                                "{url}: {method} {path} {name} is not a deep object"
                            ));
                        }
                        if parameter["schema"]["additionalProperties"]["type"] != "string" {
                            problems.push(format!(
                                "{url}: {method} {path} {name} takes non-string values"
                            ));
                        }
                    }
                }
            }
        }
        assert!(checked >= 7, "only {checked} query objects were found");
        assert!(
            problems.is_empty(),
            "query objects:\n{}",
            problems.join("\n")
        );
    }

    /// Fetch a path from a router, returning the status and the redirect's location
    ///
    /// # Arguments
    ///
    /// * `app` - The router to request from
    /// * `path` - The path to request
    async fn fetch_location(app: &axum::Router, path: &str) -> (u16, String) {
        // ask the router the way a browser would
        let request = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("a request");
        let response = app.clone().oneshot(request).await.expect("a response");
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        (response.status().as_u16(), location)
    }

    /// A url that names a directory or the UI without its trailing slash redirects inside its prefix
    ///
    /// The redirect is relative to the request, so resolving it from the url that was asked for, with
    /// and without an ingress prefix, gives the same url with a slash added instead of an absolute path
    /// that left the api prefix behind.
    #[tokio::test]
    async fn redirects_to_a_trailing_slash_stay_inside_the_prefix() {
        // a docs directory with one page below a subdirectory
        let dir = std::env::temp_dir().join(format!("thorium-docs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).expect("a docs directory");
        std::fs::write(dir.join("sub/index.html"), "page").expect("a page");
        // the docs and the UI, nested under the api prefix like the real router
        let app = axum::Router::new().nest(
            super::API_PREFIX,
            axum::Router::new()
                .nest_service(
                    "/docs/user",
                    super::docs_service(&dir, &dir.join("sub/index.html")),
                )
                .merge(super::swagger_ui::<()>()),
        );
        for path in ["/api/docs/user/sub", "/api/docs/swagger-ui"] {
            for prefix in ["", "/thorium"] {
                // ask for the directory without its slash
                let (status, location) = fetch_location(&app, path).await;
                assert!((300..400).contains(&status), "{path} answered {status}");
                // resolve the redirect from the url the browser asked for
                let asked = url::Url::parse(&format!("http://thorium.test{prefix}{path}"))
                    .expect("a request url");
                let target = asked.join(&location).expect("a location");
                assert_eq!(
                    target.path(),
                    format!("{prefix}{path}/"),
                    "{path} behind {prefix:?} redirected to {location}"
                );
            }
            // and the page with its slash is served
            let (status, _, _) = fetch(&app, &format!("{path}/")).await;
            assert_eq!(status, 200, "{path}/");
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
