//! Tests the thorctl models commands against the test API

mod common;

use common::{TestUser, assert_success, stdout, unique};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use thorium::models::{
    EntityMetadataRequest, EntityMetadataUpdate, EntityUpdate, GroupUpdate, GroupUsersUpdate,
    ListedModel, ModelApi, ModelBan, ModelBanUpdate, ModelCatalogEntryRequest,
    ModelEndpointEntityRequest, ModelProfileEntityRequest, ModelProvider, ModelRefreshReason,
    ModelRefreshReport, ModelSecret,
};
use thorium::test_utilities::{events, generators};
use thorium::{Error, Thorium};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

/// Write a document to a unique file in the temp dir
///
/// # Arguments
///
/// * `name` - A prefix for the file name
/// * `contents` - What to write
fn write_file(name: &str, contents: &str) -> String {
    // pick a unique path and write the contents
    let path = std::env::temp_dir().join(unique(name));
    std::fs::write(&path, contents).expect("failed to write a test file");
    path.to_string_lossy().into_owned()
}

/// Parse a command's stdout as JSON
///
/// # Arguments
///
/// * `output` - The command's output
fn json(output: &std::process::Output) -> Value {
    serde_json::from_str(&stdout(output)).expect("stdout was not JSON")
}

/// Make sure a command printed no secret, without ever printing its output
///
/// # Arguments
///
/// * `output` - The command's output
/// * `secret` - The secret that must not appear
/// * `what` - What the command was, for the failure message
fn assert_no_secret(output: &std::process::Output, secret: &str, what: &str) {
    // check both streams
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!printed.contains(secret), "{what} printed the secret");
}

/// Get the id an `Created ... (<id>)` line printed
///
/// # Arguments
///
/// * `output` - The command's output
fn created_id(output: &std::process::Output) -> String {
    // the id is in the parentheses of the first line
    let out = stdout(output);
    let line = out.lines().next().expect("the command printed nothing");
    line.rsplit_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(id, _)| id.to_owned())
        .expect("the command printed no id")
}

/// Allow the endpoints these tests create on the deployment's whitelist
async fn allow_test_endpoints() -> Result<(), Error> {
    // the default Anthropic url and the keyless gateway the specs use
    let providers = [
        generators::gen_anthropic_provider(),
        ModelProvider::LiteLlm {
            base_url: "https://gw.local/v1".to_owned(),
            key_header: None,
        },
    ];
    for provider in &providers {
        generators::allow_model_endpoint(provider).await?;
    }
    Ok(())
}

/// Wait for the manual refresh of an endpoint to be queued and answer it the way the AI worker
/// does, with an admin report
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `endpoint` - The endpoint being refreshed
/// * `seen` - How many manual refreshes of the endpoint were queued before
/// * `models` - The models the report lists
async fn answer_refresh(
    admin: &Thorium,
    endpoint: Uuid,
    seen: usize,
    models: &[&str],
) -> Result<(), Error> {
    // wait until the refresh is queued
    for _ in 0..100 {
        let manual = events::refresh_events(admin, endpoint)
            .await?
            .iter()
            .filter_map(events::refresh_event)
            .filter(|refresh| refresh.reason == ModelRefreshReason::Manual)
            .count();
        if manual > seen {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // send the listing as the AI worker would
    let report =
        ModelRefreshReport::listed(models.iter().map(|id| ListedModel::new(*id)).collect());
    admin
        .entities
        .update(
            endpoint,
            EntityUpdate::default()
                .metadata(EntityMetadataUpdate::model_endpoint().refresh_report(report)),
        )
        .await?;
    Ok(())
}

/// `models refresh` queues a refresh and with `--wait-for refreshed` returns once its report is
/// merged, and `models catalogs describe` then shows the listed models and a removed one's ban
#[tokio::test]
// one scenario walks the command and the reads after it
#[allow(clippy::too_many_lines)]
async fn models_refresh() -> Result<(), Error> {
    // an owner's keyless gateway endpoint and a plain member
    let owner = TestUser::new("models-refresh").await?;
    allow_test_endpoints().await?;
    let member = TestUser::new("models-refresh-member").await?;
    owner
        .admin
        .groups
        .update(
            &owner.group,
            &GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&member.username)),
        )
        .await?;
    let provider = ModelProvider::LiteLlm {
        base_url: "https://gw.local/v1".to_owned(),
        key_header: None,
    };
    let endpoint = generators::model_endpoint(
        &[owner.group.as_str()],
        ModelEndpointEntityRequest::new(provider),
        &owner.client,
    )
    .await?;
    let id = endpoint.id.to_string();
    // waiting for the refresh returns once the report is merged
    let args = [
        "models",
        "refresh",
        &endpoint.name,
        "--wait-for",
        "refreshed",
        "--timeout",
        "60s",
        "-o",
        "json",
    ];
    let child = owner.spawn(&args);
    answer_refresh(&owner.admin, endpoint.id, 0, &["gw-a", "gw-b"]).await?;
    let out = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("thorctl timed out")
        .expect("thorctl failed");
    assert_success(&out);
    let view = json(&out);
    assert_eq!(view["endpoint"], json!(id));
    assert_eq!(view["state"], json!("Succeeded"));
    assert!(view["refreshed"].is_string());
    // a refresh without waiting prints the pending state
    let args = ["models", "refresh", &id];
    let out = owner.run("models-refresh-pending", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Pending"));
    answer_refresh(&owner.admin, endpoint.id, 1, &["gw-a"]).await?;
    // the catalog shows the listed model and the removed one's ban
    let args = ["models", "catalogs", "describe", &id, "gw-a", "-o", "json"];
    let out = owner.run("models-refresh-listed", &args).await;
    assert_success(&out);
    assert_eq!(json(&out)["model_id"], json!("gw-a"));
    let args = ["models", "catalogs", "describe", &id, "gw-b", "-o", "json"];
    let out = owner.run("models-refresh-removed", &args).await;
    assert_success(&out);
    let bans = json(&out)["bans"].clone();
    let kinds = bans.as_object().map_or_default(|bans| {
        bans.values()
            .map(|ban| ban["ban_kind"].clone())
            .collect::<Vec<Value>>()
    });
    assert_eq!(kinds, vec![json!("ModelRemoved")]);
    // a member who doesn't manage the group gets the API's 401
    let out = member
        .run("models-refresh-member", &["models", "refresh", &id])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    Ok(())
}

/// Endpoints, profiles, and credentials are created, read, updated, and deleted by an owner,
/// secrets are read from files or stdin and only fingerprints are printed, bans are added and
/// cleared, and a plain member gets the API's refusals
#[tokio::test]
// one scenario walks every command so later steps can use what earlier ones created
#[allow(clippy::too_many_lines)]
async fn models_entities() -> Result<(), Error> {
    // an owner of a group and a plain member of it
    let owner = TestUser::new("models-entities").await?;
    allow_test_endpoints().await?;
    let member = TestUser::new("models-entities-member").await?;
    owner
        .admin
        .groups
        .update(
            &owner.group,
            &GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&member.username)),
        )
        .await?;
    let group = owner.group.clone();
    let secret = format!("sk-{}", unique("secret"));
    // create a credential from a spec and a secret file, printing only its fingerprint
    let spec = write_file("cred", "kind: ApiKey\n");
    let secret_file = write_file("secret", &format!("{secret}\n"));
    let name = unique("cred");
    let args = [
        "models",
        "credentials",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &group,
        "--secret-file",
        &secret_file,
    ];
    let out = owner.run("models-cred-create", &args).await;
    assert_success(&out);
    assert_no_secret(&out, &secret, "credentials create");
    let fingerprint = ModelSecret::from(secret.as_str()).fingerprint();
    assert!(
        stdout(&out).contains(&fingerprint),
        "create didn't print the fingerprint"
    );
    let cred_id = created_id(&out);
    // get and describe show the fingerprint and never the secret
    for (transcript, args) in [
        (
            "models-cred-get",
            vec!["models", "credentials", "get", "-g", &group, "-o", "json"],
        ),
        (
            "models-cred-describe",
            vec!["models", "credentials", "describe", &name],
        ),
    ] {
        let out = owner.run(transcript, &args).await;
        assert_success(&out);
        assert_no_secret(&out, &secret, transcript);
        assert!(
            stdout(&out).contains(&fingerprint),
            "{transcript} lost the fingerprint"
        );
    }
    // a member reads it but can't rotate or delete it
    let rotate_file = write_file("rotate", "sk-member-rotation");
    let out = member
        .run(
            "models-cred-rotate-member",
            &[
                "models",
                "credentials",
                "rotate",
                &cred_id,
                "--secret-file",
                &rotate_file,
            ],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    // rotating from stdin prints only the new fingerprint
    let rotated = format!("sk-{}", unique("rotated"));
    let mut child = owner
        .command(&[
            "models",
            "credentials",
            "rotate",
            &name,
            "--secret-file",
            "-",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start thorctl");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(rotated.as_bytes())
        .await?;
    let out = child.wait_with_output().await?;
    assert_success(&out);
    assert_no_secret(&out, &rotated, "credentials rotate");
    assert!(stdout(&out).contains(&ModelSecret::from(rotated.as_str()).fingerprint()));
    // create an endpoint that uses it, then a profile on that endpoint
    let endpoint_spec = json!({
        "provider": {"Anthropic": {"base_url": null, "workspace_id": null}},
        "credential": cred_id,
        "models": [{"model_id": "claude-a", "context_window": 200_000, "max_output_tokens": 8_192}],
    });
    let endpoint_file = write_file(
        "endpoint",
        &serde_norway::to_string(&endpoint_spec).unwrap(),
    );
    let endpoint_name = unique("endpoint");
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &endpoint_file,
        "-n",
        &endpoint_name,
        "-g",
        &group,
    ];
    let out = owner.run("models-endpoint-create", &args).await;
    assert_success(&out);
    let endpoint_id = created_id(&out);
    // a member can't create an endpoint
    let other = unique("endpoint");
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &endpoint_file,
        "-n",
        &other,
        "-g",
        &group,
    ];
    let out = member.run("models-endpoint-create-member", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    let profile_spec = format!(
        "endpoint: {endpoint_id}\nmodel_id: claude-a\napi: AnthropicMessages\nparameters:\n  temperature: 0.2\n"
    );
    let profile_file = write_file("profile", &profile_spec);
    let profile_name = unique("profile");
    let args = [
        "models",
        "profiles",
        "create",
        "--spec",
        &profile_file,
        "-n",
        &profile_name,
        "-g",
        &group,
    ];
    let out = owner.run("models-profile-create", &args).await;
    assert_success(&out);
    // get and describe the endpoint and profile
    let out = owner
        .run(
            "models-endpoint-get",
            &["models", "endpoints", "get", "-g", &group, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(1));
    let out = owner
        .run(
            "models-endpoint-describe",
            &["models", "endpoints", "describe", &endpoint_name],
        )
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains("claude-a"));
    let out = owner
        .run(
            "models-profile-describe",
            &[
                "models",
                "profiles",
                "describe",
                &profile_name,
                "-o",
                "json",
            ],
        )
        .await;
    assert_success(&out);
    assert_eq!(
        json(&out)["metadata"]["ModelProfile"]["parameters"]["temperature"],
        json!(0.2)
    );
    // the catalog commands list and describe the endpoint's models
    let out = owner
        .run(
            "models-catalog-get",
            &["models", "catalogs", "get", &endpoint_name],
        )
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains("claude-a"));
    let out = owner
        .run(
            "models-catalog-describe",
            &[
                "models",
                "catalogs",
                "describe",
                &endpoint_name,
                "claude-a",
                "-o",
                "json",
            ],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out)["limits"]["context_window"], json!(200_000));
    // updates by an owner work, and a member's profile update works too
    let out = owner
        .run(
            "models-endpoint-update",
            &[
                "models",
                "endpoints",
                "update",
                &endpoint_name,
                "-d",
                "updated",
            ],
        )
        .await;
    assert_success(&out);
    let out = member
        .run(
            "models-profile-update",
            &[
                "models",
                "profiles",
                "update",
                &profile_name,
                "--clear-limits",
            ],
        )
        .await;
    assert_success(&out);
    let out = owner
        .run(
            "models-cred-update",
            &[
                "models",
                "credentials",
                "update",
                &name,
                "--expires",
                "2030-01-01T00:00:00Z",
            ],
        )
        .await;
    assert_success(&out);
    let out = owner
        .run(
            "models-cred-json",
            &["models", "credentials", "describe", &name, "-o", "json"],
        )
        .await;
    assert_eq!(
        json(&out)["metadata"]["ModelCredential"]["expires"],
        json!("2030-01-01T00:00:00Z")
    );
    // an owner adds a ban to a catalog model and clears it, and a member can't add one
    let args = [
        "models",
        "bans",
        "add",
        &endpoint_name,
        "--model",
        "claude-a",
        "--message",
        "paused",
    ];
    let out = owner.run("models-ban-add", &args).await;
    assert_success(&out);
    let out = owner
        .run(
            "models-ban-get",
            &[
                "models",
                "bans",
                "get",
                "--endpoint",
                &endpoint_name,
                "-o",
                "json",
            ],
        )
        .await;
    assert_success(&out);
    let bans = json(&out);
    assert_eq!(bans.as_array().map(Vec::len), Some(1));
    assert_eq!(bans[0]["model"], json!("claude-a"));
    let ban_id = bans[0]["ban"]["id"].as_str().unwrap().to_owned();
    let out = owner
        .run(
            "models-ban-describe",
            &["models", "bans", "describe", &ban_id, "-g", &group],
        )
        .await;
    assert_success(&out);
    assert!(stdout(&out).contains("paused"));
    let out = member
        .run(
            "models-ban-add-member",
            &["models", "bans", "add", &endpoint_name, "--message", "x"],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    let out = owner
        .run(
            "models-ban-clear",
            &["models", "bans", "clear", &endpoint_name, &ban_id],
        )
        .await;
    assert_success(&out);
    let out = owner
        .run(
            "models-ban-get-empty",
            &[
                "models",
                "bans",
                "get",
                "--endpoint",
                &endpoint_name,
                "-o",
                "json",
            ],
        )
        .await;
    assert_eq!(json(&out).as_array().map(Vec::len), Some(0));
    // deletes go profile, endpoint, credential, and a member can't delete the endpoint
    let out = member
        .run(
            "models-endpoint-delete-member",
            &["models", "endpoints", "delete", &endpoint_name],
        )
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    for (transcript, kind, name) in [
        ("models-profile-delete", "profiles", &profile_name),
        ("models-endpoint-delete", "endpoints", &endpoint_name),
        ("models-cred-delete", "credentials", &name),
    ] {
        let out = owner
            .run(transcript, &["models", kind, "delete", name])
            .await;
        assert_success(&out);
    }
    let out = owner
        .run(
            "models-cred-get-empty",
            &["models", "credentials", "get", "-g", &group, "-o", "json"],
        )
        .await;
    assert_eq!(json(&out).as_array().map(Vec::len), Some(0));
    Ok(())
}

/// Profile spec and `--parameters` files holding a key that isn't a known parameter are refused
/// with the key named before anything is sent, in YAML and JSON alike
#[tokio::test]
// one scenario covers create and update so the update can target a profile the create made
#[allow(clippy::too_many_lines)]
async fn models_profile_parameters_refuse_unknown_keys() -> Result<(), Error> {
    // an owner of a group, both named for this test
    let suffix = generators::name_suffix();
    let owner = TestUser::named(
        "models-profile-parameters",
        &generators::readable_name("io", &suffix),
        &generators::readable_name("minerva", &suffix),
    )
    .await?;
    allow_test_endpoints().await?;
    let group = owner.group.clone();
    // a keyless gateway endpoint serving one model
    let spec = write_file(
        "endpoint",
        "provider:\n  LiteLlm:\n    base_url: https://gw.local/v1\n    key_header: null\ncredential: null\nmodels:\n  - model_id: gw-a\n",
    );
    let endpoint_name = generators::readable_name("gateway", &suffix);
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &spec,
        "-n",
        &endpoint_name,
        "-g",
        &group,
    ];
    let out = owner.run("models-parameters-endpoint", &args).await;
    assert_success(&out);
    let endpoint_id = created_id(&out);
    // a YAML spec whose parameters hold a misspelled key is refused and creates nothing
    let bad_spec = write_file(
        "profile",
        &format!(
            "endpoint: {endpoint_id}\nmodel_id: gw-a\napi: OpenAiChat\nparameters:\n  temperature: 0.2\n  max_tokens: 100\n"
        ),
    );
    let bad_name = generators::readable_name("bad-profile", &suffix);
    let args = [
        "models", "profiles", "create", "--spec", &bad_spec, "-n", &bad_name, "-g", &group,
    ];
    let out = owner.run("models-parameters-create-unknown", &args).await;
    assert!(
        !out.status.success(),
        "a spec with an unknown parameter was accepted"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("max_tokens"),
        "the error didn't name the key: {stderr}"
    );
    let out = owner
        .run(
            "models-parameters-create-none",
            &["models", "profiles", "get", "-g", &group, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out), json!([]));
    // a spec with known parameters creates the profile
    let spec = write_file(
        "profile",
        &format!(
            "endpoint: {endpoint_id}\nmodel_id: gw-a\napi: OpenAiChat\nparameters:\n  temperature: 0.2\n"
        ),
    );
    let name = generators::readable_name("profile", &suffix);
    let args = [
        "models", "profiles", "create", "--spec", &spec, "-n", &name, "-g", &group,
    ];
    assert_success(&owner.run("models-parameters-create", &args).await);
    // YAML and JSON parameter files with a misspelled key are refused and change nothing
    let yaml = write_file("parameters", "temperature: 0.7\nmax_tokens: 100\n");
    let json_file = write_file("parameters", r#"{"temperature": 0.7, "max_tokens": 100}"#);
    for (transcript, file) in [
        ("models-parameters-update-yaml", &yaml),
        ("models-parameters-update-json", &json_file),
    ] {
        let args = ["models", "profiles", "update", &name, "--parameters", file];
        let out = owner.run(transcript, &args).await;
        assert!(
            !out.status.success(),
            "{transcript}: parameters with an unknown key were accepted"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("max_tokens"),
            "{transcript}: the error didn't name the key: {stderr}"
        );
    }
    let out = owner
        .run(
            "models-parameters-unchanged",
            &["models", "profiles", "describe", &name, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_eq!(
        json(&out)["metadata"]["ModelProfile"]["parameters"],
        json!({"temperature": 0.2, "top_p": null, "max_output_tokens": null, "thinking_budget_tokens": null, "stop_sequences": []})
    );
    // a parameters file with known keys updates the profile
    let good = write_file("parameters", "temperature: 0.7\nmax_output_tokens: 100\n");
    let args = ["models", "profiles", "update", &name, "--parameters", &good];
    assert_success(&owner.run("models-parameters-update", &args).await);
    let out = owner
        .run(
            "models-parameters-updated",
            &["models", "profiles", "describe", &name, "-o", "json"],
        )
        .await;
    let parameters = &json(&out)["metadata"]["ModelProfile"]["parameters"];
    assert_eq!(parameters["temperature"], json!(0.7));
    assert_eq!(parameters["max_output_tokens"], json!(100));
    Ok(())
}

/// `endpoints update --disable-model` and `--enable-model` change the endpoint's disabled ids,
/// a member's update prints the API's 401, and no tool instance is created
#[tokio::test]
async fn models_disable_models() -> Result<(), Error> {
    // an owner of a group and a plain member of it
    let owner = TestUser::new("models-disable").await?;
    allow_test_endpoints().await?;
    let member = TestUser::new("models-disable-member").await?;
    owner
        .admin
        .groups
        .update(
            &owner.group,
            &GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&member.username)),
        )
        .await?;
    let group = owner.group.clone();
    // a keyless gateway endpoint
    let spec = write_file(
        "endpoint",
        "provider:\n  LiteLlm:\n    base_url: https://gw.local/v1\n    key_header: null\ncredential: null\n",
    );
    let name = unique("endpoint");
    let out = owner
        .run(
            "models-disable-create",
            &[
                "models",
                "endpoints",
                "create",
                "--spec",
                &spec,
                "-n",
                &name,
                "-g",
                &group,
            ],
        )
        .await;
    assert_success(&out);
    // disabling two ids and enabling one leaves the other
    let args = [
        "models",
        "endpoints",
        "update",
        &name,
        "--disable-model",
        "a-latest",
        "--disable-model",
        "b-latest",
    ];
    assert_success(&owner.run("models-disable-two", &args).await);
    let args = [
        "models",
        "endpoints",
        "update",
        &name,
        "--enable-model",
        "a-latest",
    ];
    assert_success(&owner.run("models-enable-one", &args).await);
    let out = owner
        .run(
            "models-disable-read",
            &["models", "endpoints", "describe", &name, "-o", "json"],
        )
        .await;
    assert_eq!(
        json(&out)["metadata"]["ModelEndpoint"]["disabled_models"],
        json!(["b-latest"])
    );
    // a member's update prints the API's 401
    let args = [
        "models",
        "endpoints",
        "update",
        &name,
        "--disable-model",
        "c-latest",
    ];
    let out = member.run("models-disable-member", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("401 Unauthorized"));
    // no tool or tool instance was created
    let out = owner
        .run(
            "models-disable-tools",
            &["tools", "get", "-g", &group, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out), json!([]));
    Ok(())
}

/// `models settings` adds and removes the caller's own endpoint, sets and removes their own
/// credential printing only its fingerprint, sets and clears default profiles, and disables and
/// enables model ids on their own endpoint
#[tokio::test]
// one scenario walks every command so later steps can use what earlier ones created
#[allow(clippy::too_many_lines)]
async fn models_settings() -> Result<(), Error> {
    // a user with a group endpoint and a profile in their group
    let user = TestUser::new("models-settings").await?;
    allow_test_endpoints().await?;
    let group = user.group.clone();
    let req = ModelEndpointEntityRequest::new(generators::gen_anthropic_provider())
        .model(ModelCatalogEntryRequest::new("claude-a"));
    let group_endpoint = generators::model_endpoint(&[&group], req, &user.client).await?;
    let profile = user
        .client
        .entities
        .create(generators::gen_model_entity(
            &[&group],
            EntityMetadataRequest::ModelProfile(ModelProfileEntityRequest::new(
                group_endpoint.id,
                "claude-a",
                ModelApi::AnthropicMessages,
            )),
        ))
        .await?
        .id
        .to_string();
    // add an endpoint of the user's own
    let spec = write_file(
        "own-endpoint",
        "name: my-gateway\nprovider:\n  LiteLlm:\n    base_url: https://gw.local/v1\n    key_header: null\nmodels: []\n",
    );
    let out = user
        .run(
            "models-settings-add-endpoint",
            &["models", "settings", "add-endpoint", "--spec", &spec],
        )
        .await;
    assert_success(&out);
    let own_id = created_id(&out);
    // set a credential for the group endpoint from a file, printing only its fingerprint
    let secret = format!("sk-{}", unique("settings"));
    let cred_spec = write_file("own-cred", "kind: ApiKey\n");
    let secret_file = write_file("own-secret", &format!("{secret}\n"));
    let group_id = group_endpoint.id.to_string();
    let args = [
        "models",
        "settings",
        "set-credential",
        &group_id,
        "--spec",
        &cred_spec,
        "--secret-file",
        &secret_file,
    ];
    let out = user.run("models-settings-set-credential", &args).await;
    assert_success(&out);
    assert_no_secret(&out, &secret, "settings set-credential");
    let fingerprint = ModelSecret::from(secret.as_str()).fingerprint();
    assert!(
        stdout(&out).contains(&fingerprint),
        "set-credential didn't print the fingerprint"
    );
    // set the default profile
    let out = user
        .run(
            "models-settings-set-default",
            &["models", "settings", "set-default", &profile],
        )
        .await;
    assert_success(&out);
    // get shows the endpoint, the fingerprint, and the default, and never the secret
    let out = user
        .run(
            "models-settings-get",
            &["models", "settings", "get", "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_no_secret(&out, &secret, "settings get");
    let read = json(&out);
    assert_eq!(read["endpoints"][&own_id]["name"], json!("my-gateway"));
    assert_eq!(
        read["credentials"][&group_id]["secrets"]["fingerprint"],
        json!(fingerprint)
    );
    assert_eq!(read["default_profiles"], json!([profile]));
    let human = user
        .run("models-settings-get-human", &["models", "settings", "get"])
        .await;
    assert_success(&human);
    assert_no_secret(&human, &secret, "settings get");
    assert!(stdout(&human).contains("my-gateway"));
    // disable two ids on the own endpoint and enable one of them
    let args = [
        "models",
        "settings",
        "update-endpoint",
        &own_id,
        "--disable-model",
        "a-latest",
        "--disable-model",
        "b-latest",
    ];
    assert_success(&user.run("models-settings-disable", &args).await);
    let args = [
        "models",
        "settings",
        "update-endpoint",
        "my-gateway",
        "--enable-model",
        "a-latest",
    ];
    assert_success(&user.run("models-settings-enable", &args).await);
    let out = user
        .run(
            "models-settings-disabled-read",
            &["models", "settings", "get", "-o", "json"],
        )
        .await;
    assert_eq!(
        json(&out)["endpoints"][&own_id]["disabled_models"],
        json!(["b-latest"])
    );
    // a group endpoint can't disable models through the user's settings
    let args = [
        "models",
        "settings",
        "update-endpoint",
        &group_id,
        "--disable-model",
        "c-latest",
    ];
    let out = user.run("models-settings-disable-group", &args).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("400"));
    // remove the credential, clear the defaults, and remove the endpoint
    for (transcript, args) in [
        (
            "models-settings-remove-credential",
            vec!["models", "settings", "remove-credential", &group_id],
        ),
        (
            "models-settings-clear-default",
            vec!["models", "settings", "set-default"],
        ),
        (
            "models-settings-remove-endpoint",
            vec!["models", "settings", "remove-endpoint", "my-gateway"],
        ),
    ] {
        assert_success(&user.run(transcript, &args).await);
    }
    let out = user
        .run(
            "models-settings-empty",
            &["models", "settings", "get", "-o", "json"],
        )
        .await;
    let read = json(&out);
    assert_eq!(read["endpoints"], json!({}));
    assert_eq!(read["credentials"], json!({}));
    assert_eq!(read["default_profiles"], json!([]));
    // no tool or tool instance was created
    let out = user
        .run(
            "models-settings-tools",
            &["tools", "get", "-g", &group, "-o", "json"],
        )
        .await;
    assert_success(&out);
    assert_eq!(json(&out), json!([]));
    Ok(())
}

/// Lists and the bans of every endpoint read past the first page
#[tokio::test]
async fn models_lists_page() -> Result<(), Error> {
    // an owner with 51 banned endpoints, more than one page holds
    let owner = TestUser::new("models-lists-page").await?;
    allow_test_endpoints().await?;
    for _ in 0..51 {
        let req = ModelEndpointEntityRequest::new(generators::gen_anthropic_provider());
        let endpoint = generators::model_endpoint(&[&owner.group], req, &owner.client).await?;
        let meta = EntityMetadataUpdate::model_endpoint()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("paused")));
        owner
            .client
            .entities
            .update(endpoint.id, EntityUpdate::default().metadata(meta))
            .await?;
    }
    // a limit above a page lists all of them
    let args = [
        "models",
        "endpoints",
        "get",
        "-g",
        &owner.group,
        "--limit",
        "60",
        "-o",
        "json",
    ];
    let out = owner.run("models-endpoints-get-pages", &args).await;
    assert_success(&out);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(51));
    // the bans of every endpoint are listed
    let args = ["models", "bans", "get", "-g", &owner.group, "-o", "json"];
    let out = owner.run("models-bans-get-pages", &args).await;
    assert_success(&out);
    assert_eq!(json(&out).as_array().map(Vec::len), Some(51));
    Ok(())
}
