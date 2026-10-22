use axum::Router;
use axum::routing::{MethodRouter, get_service};
use tower_http::services::{ServeDir, ServeFile};
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_swagger_ui::SwaggerUi;

use super::BasicApiDocs;
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
use super::users::UserApiDocs;

use crate::models::{ResultSearchEvent, SearchEvent, TagSearchEvent};
use crate::{Conf, utils::AppState};

/// The struct containing our OpenAPI security info
pub struct OpenApiSecurity;

impl Modify for OpenApiSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // get our components
        let components = openapi.components.as_mut().unwrap();
        components.add_security_scheme(
            "basic",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Basic)),
        );
    }
}

/// Serve our docs
///
///  # Arguments
///
/// * `conf` - The Thorium config
fn user(conf: &Conf) -> MethodRouter {
    // build the full path to our target file
    let full = &conf.thorium.assets.user_docs.as_path();
    // build the router for our user docs
    get_service(
        ServeDir::new(full).not_found_service(ServeFile::new(&conf.thorium.assets.not_found)),
    )
}

/// Serve our developer docs
///
///  # Arguments
///
/// * `conf` - The Thorium config
fn dev(conf: &Conf) -> MethodRouter {
    // build the full path to our target file
    let full = &conf.thorium.assets.dev_docs.as_path();
    // build the router for our user docs
    get_service(
        ServeDir::new(full).not_found_service(ServeFile::new(&conf.thorium.assets.not_found)),
    )
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
        .merge(
            SwaggerUi::new("/docs/swagger-ui")
                .url("/openapi.json", BasicApiDocs::openapi())
                .url("/events/openapi.json", EventApiDocs::openapi())
                .url("/files/openapi.json", FileApiDocs::openapi())
                .url("/groups/openapi.json", GroupApiDocs::openapi())
                .url("/images/openapi.json", ImageApiDocs::openapi())
                .url("/jobs/openapi.json", JobApiDocs::openapi())
                .url(
                    "/networkpolicies/openapi.json",
                    NetworkPolicyDocs::openapi(),
                )
                .url("/pipelines/openapi.json", PipelineApiDocs::openapi())
                .url("/reactions/openapi.json", ReactionApiDocs::openapi())
                .url("/repos/openapi.json", RepoApiDocs::openapi())
                .url("/search/openapi.json", SearchApiDocs::openapi())
                .url(
                    format!("/search/events/{}", ResultSearchEvent::url()),
                    ResultSearchEventApiDocs::openapi(),
                )
                .url(
                    format!("/search/events/{}", TagSearchEvent::url()),
                    TagSearchEventApiDocs::openapi(),
                )
                .url("/stream/openapi.json", StreamApiDocs::openapi())
                .url("/system/openapi.json", SystemApiDocs::openapi())
                .url("/users/openapi.json", UserApiDocs::openapi())
                .url("/oauth/openapi.json", OAuthApiDocs::openapi()),
        )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use regex::Regex;

    /// The handlers a routes file mounts and the handlers its `OpenAPI` docs list
    ///
    /// # Arguments
    ///
    /// * `source` - The source of a routes file
    fn mounted_and_documented(source: &str) -> (BTreeSet<String>, BTreeSet<String>) {
        // only the code before the file's tests counts
        let code = source.split("#[cfg(test)]").next().unwrap_or(source);
        // the handlers the file's mount function registers on its routes
        let mount = code
            .split("pub fn mount(")
            .nth(1)
            .expect("a mount function");
        let routed = Regex::new(r"\b(?:get|post|patch|put|delete)\(\s*([a-z_][a-z0-9_]*)\s*\)")
            .expect("a handler pattern")
            .captures_iter(mount)
            .map(|found| found[1].to_owned())
            .collect();
        // the handlers the file's OpenAPI docs list
        let listed = Regex::new(r"(?s)#\[openapi\(\s*paths\(([^)]*)\)")
            .expect("a paths pattern")
            .captures(code)
            .map_or_else(BTreeSet::new, |found| {
                found[1]
                    .split(',')
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            });
        (routed, listed)
    }

    /// Reaction routes that are mounted without `OpenAPI` docs
    ///
    /// Every other route in the files checked must be documented, and a route leaves this list
    /// when it gains docs.
    const UNDOCUMENTED: [&str; 5] = [
        "create_bulk_by_user",
        "get_cache",
        "update_cache",
        "update_cache_files",
        "download_cache_file",
    ];

    /// Every handler the jobs, reactions, events, and users routes mount is in its `OpenAPI` docs,
    /// except the routes that never were
    #[test]
    fn mounted_handlers_are_documented() {
        // check each routes file in turn
        for (name, source) in [
            ("jobs", include_str!("jobs.rs")),
            ("reactions", include_str!("reactions.rs")),
            ("events", include_str!("events.rs")),
            ("users", include_str!("users.rs")),
        ] {
            // a handler mounted without being listed is missing from the docs
            let (routed, listed) = mounted_and_documented(source);
            assert!(!routed.is_empty(), "no handlers were found in {name}");
            assert!(!listed.is_empty(), "no documented paths in {name}");
            let missing = routed
                .difference(&listed)
                .filter(|handler| !UNDOCUMENTED.contains(&handler.as_str()))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{name} mounts handlers its docs don't list: {missing:?}"
            );
            // a route that gained docs leaves the list of routes without them
            let stale = UNDOCUMENTED
                .iter()
                .filter(|handler| listed.contains(**handler))
                .collect::<Vec<_>>();
            assert!(
                stale.is_empty(),
                "{name} documents routes that are still exempt: {stale:?}"
            );
        }
    }
}
