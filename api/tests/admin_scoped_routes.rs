//! Tests that the routes only admins may use refuse an admin's scoped token

use reqwest::Method;
use serde_json::{Value, json};
use thorium::models::{ScopedTokenRequest, ScopedTokenRole, UserRole};
use thorium::test_utilities;
use thorium::test_utilities::generators::{self, GroupRoles};
use thorium::{Error, is};
use uuid::Uuid;

/// A route only an admin authenticated with their full token may use
struct AdminRoute {
    /// What the route does, for failure messages
    what: &'static str,
    /// The method of the request
    method: Method,
    /// The path of the request
    path: String,
    /// The body of the request, if it takes one
    body: Option<Value>,
    /// The status the admin's full token gets, which is never a refusal
    full: u16,
}

/// Send a request to the test API with an authorization header
///
/// Returns the status the API answered with.
///
/// # Arguments
///
/// * `host` - The API to send the request to
/// * `route` - The route to request
/// * `auth` - The value of the authorization header to send
async fn send(host: &str, route: &AdminRoute, auth: &str) -> Result<u16, Error> {
    // build the request with its authorization header
    let mut req = reqwest::Client::new()
        .request(route.method.clone(), format!("{host}{}", route.path))
        .header("authorization", auth);
    // add the body if the route takes one
    if let Some(body) = &route.body {
        req = req.json(body);
    }
    Ok(req.send().await?.status().as_u16())
}

/// An admin's scoped token is refused on every route that reads or writes secrets or runtimes
/// for the platform, where the admin's full token is not refused
#[tokio::test]
async fn admin_scoped_token_is_refused_on_admin_routes() -> Result<(), Error> {
    // an admin who owns a group, and a plain user to notify
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let boss = generators::named_user_with_role(
        &admin,
        &generators::readable_name("kalyke", &suffix),
        UserRole::Admin,
    )
    .await?;
    let spare = generators::moon_user(&admin, "chaldene", &suffix).await?;
    let group = generators::god_group(
        &admin,
        "apollo",
        &suffix,
        &GroupRoles::default().owner(&boss.username),
    )
    .await?;
    // the admin's scoped token with the broadest role a scoped token can have
    let role = ScopedTokenRole::Developer {
        k8s: true,
        bare_metal: true,
        windows: true,
        external: true,
        kvm: true,
    };
    let req = ScopedTokenRequest::new(format!("scoped-{suffix}"))
        .group(&group)
        .role(role);
    let scoped = boss.client.users.create_scoped_token(&req).await?;
    let scoped_auth = test_utilities::auth_header(&scoped.token);
    let full_auth = boss.auth_header();
    // the routes that return secrets or runtimes or write notifications for another user
    let missing = Uuid::new_v4();
    let routes = vec![
        AdminRoute {
            what: "run models",
            method: Method::GET,
            path: format!("/api/reactions/{group}/{missing}/run-models"),
            body: None,
            full: 404,
        },
        AdminRoute {
            what: "update workload runtimes",
            method: Method::POST,
            path: "/api/jobs/workloads/runtimes/update".to_owned(),
            body: None,
            full: 204,
        },
        AdminRoute {
            what: "read workload runtimes",
            method: Method::GET,
            path: "/api/jobs/workloads/runtimes".to_owned(),
            body: None,
            full: 200,
        },
        AdminRoute {
            what: "read a model credential's secret",
            method: Method::GET,
            path: format!("/api/entities/models/credentials/{missing}/secret"),
            body: None,
            full: 404,
        },
        AdminRoute {
            what: "read a user's model credential",
            method: Method::GET,
            path: format!(
                "/api/users/user/{}/model-credentials/{missing}",
                spare.username
            ),
            body: None,
            full: 404,
        },
        AdminRoute {
            what: "create a user's notification",
            method: Method::POST,
            path: format!("/api/users/notifications/{}", spare.username),
            body: Some(json!({"msg": "from a scoped token", "level": "Info"})),
            full: 204,
        },
    ];
    // every route refuses the scoped token, and all of them are tried before any is judged
    let mut accepted = Vec::new();
    for route in &routes {
        let status = send(&admin.host, route, &scoped_auth).await?;
        if status != 401 {
            accepted.push(format!("{} answered {status}", route.what));
        }
    }
    is!(accepted, Vec::<String>::new());
    // the refused notification wrote nothing
    is!(spare.client.users.notifications().await?.len(), 0);
    // the admin's full token is not refused on any of them
    for route in &routes {
        let status = send(&admin.host, route, &full_auth).await?;
        is!(status, route.full, route.what);
    }
    // and the notification only the full token created is the one the user sees
    let notes = spare.client.users.notifications().await?;
    is!(notes.len(), 1);
    is!(notes[0].msg.as_str(), "from a scoped token");
    Ok(())
}
