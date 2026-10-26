//! Tests the thorctl models commands against the test API

mod common;

use common::editor::{EditorScript, recovery_path};
use common::{TestUser, assert_success, stdout, unique};
use serde_json::{Value, json};
use std::fmt::Write as _;
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
    owner.record("models-refresh-wait", &args, &out);
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
    let mut owner = TestUser::new("models-entities").await?;
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
    owner.evidence.register_secret(&secret);
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
    owner.evidence.register_secret(&rotated);
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
    let mut user = TestUser::new("models-settings").await?;
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
    user.evidence.register_secret(&secret);
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

/// The environment that lets thorctl open an editor script without a terminal
const EDIT_ENV: [(&str, &str); 1] = [("THORCTL_EDIT_ASSUME_TTY", "1")];

/// Run thorctl to completion with extra environment variables and record its transcript
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `name` - The name of the transcript
/// * `args` - The thorctl arguments after the global flags
/// * `envs` - The environment variables to set
async fn run_with(
    user: &TestUser,
    name: &str,
    args: &[&str],
    envs: &[(&str, &str)],
) -> std::process::Output {
    // set the variables and run, giving up if it hangs
    let mut command = user.command(args);
    command.envs(envs.iter().copied());
    let output = tokio::time::timeout(Duration::from_secs(120), command.output())
        .await
        .expect("thorctl timed out")
        .expect("failed to run thorctl");
    user.record(name, args, &output);
    output
}

/// Run thorctl with `--stdin` text piped to it
///
/// # Arguments
///
/// * `user` - The user to run as
/// * `args` - The thorctl arguments after the global flags
/// * `input` - The text to write to stdin
async fn run_with_stdin(user: &TestUser, args: &[&str], input: &str) -> std::process::Output {
    // start thorctl with a pipe for stdin
    let mut child = user
        .command(args)
        .stdin(Stdio::piped())
        .spawn()
        .expect("failed to start thorctl");
    child
        .stdin
        .take()
        .expect("no stdin")
        .write_all(input.as_bytes())
        .await
        .expect("failed to write stdin");
    child.wait_with_output().await.expect("thorctl failed")
}

/// The text a command wrote to stderr
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Allow a second keyless gateway on the deployment's whitelist
async fn allow_second_gateway() -> Result<(), Error> {
    // a url the edit tests move an endpoint to
    let provider = ModelProvider::LiteLlm {
        base_url: "https://gw2.local/v1".to_owned(),
        key_header: None,
    };
    generators::allow_model_endpoint(&provider).await
}

/// Create a keyless gateway endpoint with a description and some catalog models
///
/// # Arguments
///
/// * `owner` - The group's owner
/// * `models` - The catalog model ids
async fn gateway(owner: &TestUser, models: &[&str]) -> String {
    // list each model in the spec
    let mut spec = String::from(
        "provider:\n  LiteLlm:\n    base_url: https://gw.local/v1\n    key_header: null\ncredential: null\nmodels:\n",
    );
    for model in models {
        let _ = writeln!(spec, "  - model_id: {model}\n    context_window: 100");
    }
    let spec = write_file("endpoint", &spec);
    let name = unique("endpoint");
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &owner.group,
        "-d",
        "before",
    ];
    assert_success(&owner.run("models-edit-endpoint", &args).await);
    name
}

/// Read a model entity as the API returns it
///
/// # Arguments
///
/// * `owner` - The user to read as
/// * `kind` - `endpoints` or `profiles`
/// * `name` - The entity's name
async fn entity_json(owner: &TestUser, kind: &str, name: &str) -> Value {
    // describe it as JSON
    let out = owner
        .run(
            "models-edit-read",
            &["models", kind, "describe", name, "-o", "json"],
        )
        .await;
    assert_success(&out);
    json(&out)
}

/// Read a model entity's `modified` time, which any update moves
///
/// # Arguments
///
/// * `owner` - The user to read as
/// * `kind` - `endpoints` or `profiles`
/// * `name` - The entity's name
/// * `variant` - The metadata variant (`ModelEndpoint` or `ModelProfile`)
async fn modified(owner: &TestUser, kind: &str, name: &str, variant: &str) -> Value {
    entity_json(owner, kind, name).await["metadata"][variant]["modified"].clone()
}

/// Read the document `spec` prints for a model entity
///
/// # Arguments
///
/// * `owner` - The user to read as
/// * `kind` - `endpoints` or `profiles`
/// * `name` - The entity's name
async fn spec_doc(owner: &TestUser, kind: &str, name: &str) -> Value {
    // print the spec and read it as YAML
    let out = owner
        .run("models-edit-spec", &["models", kind, "spec", name])
        .await;
    assert_success(&out);
    serde_norway::from_str(&stdout(&out)).expect("spec was not YAML")
}

/// `endpoints edit` changes an endpoint in an editor, and cancels, refuses and recovers as the
/// shared edit flow does, with every change checked in the API
#[tokio::test]
// one scenario walks each editor outcome against the same endpoint
#[allow(clippy::too_many_lines)]
async fn models_endpoints_edit_interactive() -> Result<(), Error> {
    // an owner with a gateway endpoint listing two models
    let owner = TestUser::new("models-endpoint-edit").await?;
    allow_test_endpoints().await?;
    allow_second_gateway().await?;
    let name = gateway(&owner, &["m1", "m2"]).await;
    let target = ["models", "endpoints", "edit", &name];
    // an editor changing one field applies it, after showing the header and read-only marks
    let script = EditorScript::replace("description: before", "description: after");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-description", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let shown = script.seen(0).expect("the editor wasn't opened");
    assert!(shown.contains("# Editing model endpoint"), "{shown}");
    assert!(shown.contains("*id*"), "{shown}");
    assert!(shown.contains("Secrets are never shown"), "{shown}");
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "after"
    );
    // saving without a change says so and writes nothing
    let before = modified(&owner, "endpoints", &name, "ModelEndpoint").await;
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-noop", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(
        modified(&owner, "endpoints", &name, "ModelEndpoint").await,
        before
    );
    // a whole new document changes the provider, the catalog and the disabled models at once
    let mut doc = spec_doc(&owner, "endpoints", &name).await;
    doc["provider"] = json!({"LiteLlm": {"base_url": "https://gw2.local/v1", "key_header": null}});
    doc["disabled_models"] = json!(["m1"]);
    doc["catalog"] = json!([
        {"model_id": "m2", "context_window": 100},
        {"model_id": "m3", "context_window": 7, "capabilities": {"vision": true}},
    ]);
    let script = EditorScript::write(&serde_norway::to_string(&doc).unwrap());
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-document", &args, &EDIT_ENV).await;
    assert_success(&out);
    let meta = &entity_json(&owner, "endpoints", &name).await["metadata"]["ModelEndpoint"];
    assert_eq!(
        meta["provider"]["LiteLlm"]["base_url"],
        "https://gw2.local/v1"
    );
    assert_eq!(meta["disabled_models"], json!(["m1"]));
    let ids: Vec<&str> = meta["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["m2", "m3"]);
    assert_eq!(meta["catalog"][1]["capabilities"]["vision"], true);
    assert_eq!(meta["catalog"][1]["limits"]["context_window"], 7);
    // an edit to a read-only field is warned about and ignored while the rest applies
    let mut doc = spec_doc(&owner, "endpoints", &name).await;
    doc["*id*"] = json!(Uuid::new_v4().to_string());
    doc["description"] = json!("third");
    let script = EditorScript::write(&serde_norway::to_string(&doc).unwrap());
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-readonly", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "third"
    );
    // a document that isn't YAML, cancelled, fails and keeps the text in a recovery file
    let broken = "name: [unterminated";
    let script = EditorScript::write(broken);
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "cancel")];
    let out = run_with(&owner, "models-edit-invalid", &args, &envs).await;
    assert!(!out.status.success());
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains(broken));
    let _ = std::fs::remove_file(path);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "third"
    );
    // a provider the deployment doesn't allow is refused by the API with its reason
    let mut doc = spec_doc(&owner, "endpoints", &name).await;
    doc["provider"] =
        json!({"LiteLlm": {"base_url": "https://not-allowed.local/v1", "key_header": null}});
    let edited = serde_norway::to_string(&doc).unwrap();
    let script = EditorScript::write(&edited);
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-refused", &args, &EDIT_ENV).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("400"), "{}", stderr(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("not-allowed.local"));
    let _ = std::fs::remove_file(path);
    // declining the confirmation applies nothing and keeps the text in a recovery file
    let script = EditorScript::replace("description: third", "description: declined");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-declined", &args, &envs).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("description: declined"));
    let _ = std::fs::remove_file(path);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "third"
    );
    // an empty buffer cancels without an error
    let script = EditorScript::write("");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-edit-empty", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Cancelled"), "{}", stdout(&out));
    // editing without a terminal and without a change flag points at the flags
    let out = owner.run("models-edit-no-tty", &target).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--set"), "{}", stderr(&out));
    Ok(())
}

/// `endpoints update` takes `--file`, `--patch`, `--set` and `--unset` and shows every result
/// the way the shared edit flow does
#[tokio::test]
// one scenario walks each change source against the same endpoint
#[allow(clippy::too_many_lines)]
async fn models_endpoints_update_documents() -> Result<(), Error> {
    // an owner with a gateway endpoint listing two models
    let owner = TestUser::new("models-endpoint-update").await?;
    allow_test_endpoints().await?;
    allow_second_gateway().await?;
    let name = gateway(&owner, &["m1", "m2"]).await;
    let update = ["models", "endpoints", "update", &name];
    // --set changes one field and nested ones through indexes
    let args = [
        update.as_slice(),
        &[
            "--set",
            "description=set",
            "--set",
            "catalog[0].context_window=555",
        ],
    ]
    .concat();
    let out = owner.run("models-update-set", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let meta = entity_json(&owner, "endpoints", &name).await;
    assert_eq!(meta["description"], "set");
    assert_eq!(
        meta["metadata"]["ModelEndpoint"]["catalog"][0]["limits"]["context_window"],
        555
    );
    // --set to the current value changes nothing
    let before = modified(&owner, "endpoints", &name, "ModelEndpoint").await;
    let args = [update.as_slice(), &["--set", "description=set"]].concat();
    let out = owner.run("models-update-same", &args).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(
        modified(&owner, "endpoints", &name, "ModelEndpoint").await,
        before
    );
    // --patch merges, and null clears the description
    let args = [
        update.as_slice(),
        &[
            "--patch",
            r#"{"description": null, "disabled_models": ["m2"]}"#,
        ],
    ]
    .concat();
    assert_success(&owner.run("models-update-patch", &args).await);
    let meta = entity_json(&owner, "endpoints", &name).await;
    assert!(meta["description"].is_null());
    assert_eq!(
        meta["metadata"]["ModelEndpoint"]["disabled_models"],
        json!(["m2"])
    );
    // --unset removes a field, and removing one that is already gone is fine
    let args = [
        update.as_slice(),
        &["--unset", "disabled_models", "--unset", "no_such_key"],
    ]
    .concat();
    assert_success(&owner.run("models-update-unset", &args).await);
    let meta = entity_json(&owner, "endpoints", &name).await;
    assert_eq!(
        meta["metadata"]["ModelEndpoint"]["disabled_models"],
        json!([])
    );
    // --file replaces the document, from a path and from stdin
    let mut doc = spec_doc(&owner, "endpoints", &name).await;
    doc["description"] = json!("from-file");
    let file = write_file("endpoint-doc", &serde_norway::to_string(&doc).unwrap());
    let args = [update.as_slice(), &["--file", &file]].concat();
    assert_success(&owner.run("models-update-file", &args).await);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "from-file"
    );
    doc["description"] = json!("from-stdin");
    let args = [update.as_slice(), &["--file", "-"]].concat();
    let out = run_with_stdin(&owner, &args, &serde_norway::to_string(&doc).unwrap()).await;
    assert_success(&out);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "from-stdin"
    );
    // a document with an unknown key or a bad value is refused before any change
    let before = modified(&owner, "endpoints", &name, "ModelEndpoint").await;
    for (transcript, bad) in [
        (
            "models-update-unknown-key",
            "name: x\nprovider: {LiteLlm: {base_url: 'https://gw.local/v1', key_header: null}}\nnmae: y\n",
        ),
        (
            "models-update-bad-url",
            "name: x\nprovider: {LiteLlm: {base_url: 'not a url', key_header: null}}\n",
        ),
    ] {
        let file = write_file("endpoint-bad", bad);
        let args = [update.as_slice(), &["--file", &file]].concat();
        let out = owner.run(transcript, &args).await;
        assert!(!out.status.success(), "{transcript} was accepted");
    }
    assert_eq!(
        modified(&owner, "endpoints", &name, "ModelEndpoint").await,
        before
    );
    // a provider the API refuses prints its status
    let args = [
        update.as_slice(),
        &[
            "--set",
            "provider.LiteLlm.base_url=https://not-allowed.local/v1",
        ],
    ]
    .concat();
    let out = owner.run("models-update-refused", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("400"), "{}", stderr(&out));
    // read-only fields can't be set
    let args = [update.as_slice(), &["--set", "*id*=abc"]].concat();
    let out = owner.run("models-update-readonly", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    // --dry-run prints the changes and writes nothing, and -o json pins the result's shape
    let args = [
        update.as_slice(),
        &["--set", "description=dry", "--dry-run", "-o", "json"],
    ]
    .concat();
    let out = owner.run("models-update-dry-run", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["status"], "dry_run");
    assert_eq!(result["changed"], false);
    assert_eq!(result["kind"], "model endpoint");
    assert_eq!(result["diff"][0]["path"], "description");
    assert_eq!(result["diff"][0]["new"], "dry");
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "from-stdin"
    );
    let args = [
        update.as_slice(),
        &["--set", "description=json", "-o", "json"],
    ]
    .concat();
    let out = owner.run("models-update-json", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["name"], name);
    assert_eq!(result["resource"]["description"], "json");
    // the flags that change one setting each can't be mixed with a document change
    let args = [
        update.as_slice(),
        &["--set", "description=a", "--disable-model", "x"],
    ]
    .concat();
    let out = owner.run("models-update-mixed", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not both"), "{}", stderr(&out));
    let args = [update.as_slice(), &["--dry-run"]].concat();
    let out = owner.run("models-update-lone-dry-run", &args).await;
    assert!(!out.status.success());
    // the printed spec feeds straight back with no change
    let out = owner
        .run(
            "models-update-spec",
            &["models", "endpoints", "spec", &name],
        )
        .await;
    assert_success(&out);
    let args = [update.as_slice(), &["--file", "-"]].concat();
    let out = run_with_stdin(&owner, &args, &stdout(&out)).await;
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // the flags still work on their own
    let args = [update.as_slice(), &["-d", "flag"]].concat();
    assert_success(&owner.run("models-update-flag", &args).await);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "flag"
    );
    Ok(())
}

/// A change made by someone else between the read and the apply is refused unless forced
#[tokio::test]
async fn models_endpoints_update_guards_remote_changes() -> Result<(), Error> {
    // an owner with a gateway endpoint
    let owner = TestUser::new("models-endpoint-guard").await?;
    allow_test_endpoints().await?;
    let name = gateway(&owner, &["m1"]).await;
    // a hook that changes the endpoint with thorctl itself after the edit's first read
    let hook = format!(
        "{} --keys {} --skip-update models endpoints update {name} -d remote",
        env!("CARGO_BIN_EXE_thorctl"),
        owner.keys.display()
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let update = ["models", "endpoints", "update", &name];
    // the edit stops, and the remote change stays
    let args = [update.as_slice(), &["--set", "description=mine"]].concat();
    let out = run_with(&owner, "models-guard-refused", &args, &envs).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "remote"
    );
    // --force applies it anyway
    let args = [update.as_slice(), &["--set", "description=mine", "--force"]].concat();
    let out = run_with(&owner, "models-guard-forced", &args, &envs).await;
    assert_success(&out);
    assert_eq!(
        entity_json(&owner, "endpoints", &name).await["description"],
        "mine"
    );
    Ok(())
}

/// An endpoint document names its credential, never shows a secret, and the credential can be
/// swapped or removed by name
#[tokio::test]
// one scenario creates the credentials the later steps use
#[allow(clippy::too_many_lines)]
async fn models_endpoints_edit_never_shows_secrets() -> Result<(), Error> {
    // an owner with two credentials and an Anthropic endpoint that uses the first
    let mut owner = TestUser::new("models-endpoint-secrets").await?;
    allow_test_endpoints().await?;
    let group = owner.group.clone();
    let spec = write_file("cred", "kind: ApiKey\n");
    let mut names = Vec::new();
    let mut ids = Vec::new();
    let mut secrets = Vec::new();
    for _ in 0..2 {
        let secret = format!("sk-{}", unique("secret"));
        owner.evidence.register_secret(&secret);
        let secret_file = write_file("secret", &secret);
        let cred = unique("cred");
        let args = [
            "models",
            "credentials",
            "create",
            "--spec",
            &spec,
            "-n",
            &cred,
            "-g",
            &group,
            "--secret-file",
            &secret_file,
        ];
        let out = owner.run("models-secrets-cred", &args).await;
        assert_success(&out);
        ids.push(created_id(&out));
        names.push(cred);
        secrets.push(secret);
    }
    let endpoint_spec = write_file(
        "endpoint",
        &format!(
            "provider:\n  Anthropic:\n    base_url: null\n    workspace_id: null\ncredential: {}\n",
            ids[0]
        ),
    );
    let name = unique("endpoint");
    let args = [
        "models",
        "endpoints",
        "create",
        "--spec",
        &endpoint_spec,
        "-n",
        &name,
        "-g",
        &group,
    ];
    assert_success(&owner.run("models-secrets-endpoint", &args).await);
    // the spec names the credential and holds nothing secret
    let out = owner
        .run(
            "models-secrets-spec",
            &["models", "endpoints", "spec", &name],
        )
        .await;
    assert_success(&out);
    let printed = stdout(&out);
    assert!(
        printed.contains(&format!("credential: {}", names[0])),
        "{printed}"
    );
    for secret in &secrets {
        assert_no_secret(&out, secret, "endpoints spec");
        assert_no_secret(
            &out,
            &ModelSecret::from(secret.as_str()).fingerprint(),
            "endpoints spec",
        );
    }
    // an editor sees the same document and its diff stays free of secrets
    let script = EditorScript::replace("description: null", "description: shown");
    let editor = script.editor();
    let args = ["models", "endpoints", "edit", &name, "--editor", &editor];
    let out = run_with(&owner, "models-secrets-edit", &args, &EDIT_ENV).await;
    assert_success(&out);
    let shown = script.seen(0).expect("the editor wasn't opened");
    for secret in &secrets {
        assert!(
            !shown.contains(secret.as_str()),
            "the editor was shown a secret"
        );
        assert_no_secret(&out, secret, "endpoints edit");
    }
    // swapping the credential by name moves the endpoint to it
    let update = ["models", "endpoints", "edit", &name];
    let setting = format!("credential={}", names[1]);
    let args = [update.as_slice(), &["--set", &setting]].concat();
    let out = owner.run("models-secrets-swap", &args).await;
    assert_success(&out);
    let meta = entity_json(&owner, "endpoints", &name).await;
    assert_eq!(meta["metadata"]["ModelEndpoint"]["credential"], ids[1]);
    // removing it clears the credential, which this provider allows only if keyless is allowed
    let args = [
        update.as_slice(),
        &["--unset", "credential", "--dry-run", "-o", "json"],
    ]
    .concat();
    let out = owner.run("models-secrets-unset-dry-run", &args).await;
    assert_success(&out);
    assert_eq!(json(&out)["diff"][0]["path"], "credential");
    assert_eq!(json(&out)["diff"][0]["new"], Value::Null);
    // a credential that doesn't exist is refused and nothing changes
    let args = [
        update.as_slice(),
        &["--set", "credential=no-such-credential"],
    ]
    .concat();
    let out = owner.run("models-secrets-missing", &args).await;
    assert!(!out.status.success());
    let meta = entity_json(&owner, "endpoints", &name).await;
    assert_eq!(meta["metadata"]["ModelEndpoint"]["credential"], ids[1]);
    Ok(())
}

/// `endpoints spec` prints a document `edit --file -` accepts unchanged, and can write it to a
/// file as YAML or JSON
#[tokio::test]
async fn models_endpoints_spec_round_trips() -> Result<(), Error> {
    // an owner with a gateway endpoint
    let owner = TestUser::new("models-endpoint-spec").await?;
    allow_test_endpoints().await?;
    let name = gateway(&owner, &["m1", "m2"]).await;
    // the printed spec feeds straight back with no change
    let out = owner
        .run("models-spec-print", &["models", "endpoints", "spec", &name])
        .await;
    assert_success(&out);
    let printed = stdout(&out);
    assert!(
        printed.starts_with("*id*:")
            || printed.starts_with("'*id*':")
            || printed.starts_with("\"*id*\":"),
        "{printed}"
    );
    let args = ["models", "endpoints", "edit", &name, "--file", "-"];
    let out = run_with_stdin(&owner, &args, &printed).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    // -o writes a file, and --format json writes JSON
    let path = std::env::temp_dir().join(unique("spec-out"));
    let file = path.to_string_lossy().into_owned();
    let args = [
        "models",
        "endpoints",
        "spec",
        &name,
        "-o",
        &file,
        "--format",
        "json",
    ];
    assert_success(&owner.run("models-spec-file", &args).await);
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&path)?).expect("not JSON");
    assert_eq!(written["name"], name);
    assert_eq!(written["catalog"][1]["model_id"], "m2");
    let _ = std::fs::remove_file(path);
    // an endpoint that doesn't exist is an error
    let out = owner
        .run(
            "models-spec-missing",
            &["models", "endpoints", "spec", "no-such"],
        )
        .await;
    assert!(!out.status.success());
    Ok(())
}

/// Create a profile on a gateway endpoint
///
/// # Arguments
///
/// * `owner` - The group's owner
/// * `endpoint_id` - The endpoint the profile calls
async fn profile_on(owner: &TestUser, endpoint_id: &str) -> String {
    // a profile that calls the endpoint's first model
    let spec = write_file(
        "profile",
        &format!(
            "endpoint: {endpoint_id}\nmodel_id: m1\napi: OpenAiChat\nparameters:\n  temperature: 0.2\n"
        ),
    );
    let name = unique("profile");
    let args = [
        "models",
        "profiles",
        "create",
        "--spec",
        &spec,
        "-n",
        &name,
        "-g",
        &owner.group,
        "-d",
        "before",
    ];
    assert_success(&owner.run("models-edit-profile", &args).await);
    name
}

/// `profiles edit` changes a profile in an editor, and cancels, refuses and recovers as the
/// shared edit flow does, with every change checked in the API
#[tokio::test]
// one scenario walks each editor outcome against the same profile
#[allow(clippy::too_many_lines)]
async fn models_profiles_edit_interactive() -> Result<(), Error> {
    // an owner with a gateway endpoint listing two models and a profile on it
    let owner = TestUser::new("models-profile-edit").await?;
    allow_test_endpoints().await?;
    let endpoint = gateway(&owner, &["m1", "m2"]).await;
    let endpoint_id = entity_json(&owner, "endpoints", &endpoint).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let name = profile_on(&owner, &endpoint_id).await;
    let target = ["models", "profiles", "edit", &name];
    // an editor changing one field applies it, after showing the header and the endpoint's name
    let script = EditorScript::replace("model_id: m1", "model_id: m2");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-profile-edit-model", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let shown = script.seen(0).expect("the editor wasn't opened");
    assert!(shown.contains("# Editing model profile"), "{shown}");
    assert!(shown.contains(&format!("endpoint: {endpoint}")), "{shown}");
    let meta = entity_json(&owner, "profiles", &name).await;
    assert_eq!(meta["metadata"]["ModelProfile"]["model_id"], "m2");
    // saving without a change says so and writes nothing
    let before = modified(&owner, "profiles", &name, "ModelProfile").await;
    let script = EditorScript::noop();
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-profile-edit-noop", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(
        modified(&owner, "profiles", &name, "ModelProfile").await,
        before
    );
    // a whole new document changes the protocol, parameters, limits and capabilities
    let mut doc = spec_doc(&owner, "profiles", &name).await;
    doc["api"] = json!("OpenAiResponses");
    doc["parameters"] = json!({"temperature": 0.9, "stop_sequences": ["END"]});
    doc["limits"] = json!({"context_window": 50, "max_output_tokens": 10});
    doc["capabilities"] = json!({"tool_calling": true});
    let script = EditorScript::write(&serde_norway::to_string(&doc).unwrap());
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    assert_success(&run_with(&owner, "models-profile-edit-document", &args, &EDIT_ENV).await);
    let meta = &entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"];
    assert_eq!(meta["api"], "OpenAiResponses");
    assert_eq!(meta["parameters"]["temperature"], 0.9);
    assert_eq!(meta["parameters"]["stop_sequences"], json!(["END"]));
    assert_eq!(meta["limits"]["context_window"], 50);
    assert_eq!(meta["capabilities"]["tool_calling"], true);
    // leaving limits and capabilities out of the document clears them
    let mut doc = spec_doc(&owner, "profiles", &name).await;
    doc["limits"] = Value::Null;
    doc["capabilities"] = Value::Null;
    let script = EditorScript::write(&serde_norway::to_string(&doc).unwrap());
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    assert_success(&run_with(&owner, "models-profile-edit-clear", &args, &EDIT_ENV).await);
    let meta = &entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"];
    assert!(meta["limits"].is_null() && meta["capabilities"].is_null());
    // a misspelled parameter, cancelled, fails and keeps the text in a recovery file
    let mut doc = spec_doc(&owner, "profiles", &name).await;
    doc["parameters"]["temprature"] = json!(1.0);
    let broken = serde_norway::to_string(&doc).unwrap();
    let script = EditorScript::write(&broken);
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let envs = [EDIT_ENV[0], ("THORCTL_EDIT_PROMPT", "cancel")];
    let out = run_with(&owner, "models-profile-edit-invalid", &args, &envs).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("temprature"), "{}", stderr(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    assert!(std::fs::read_to_string(&path)?.contains("temprature"));
    let _ = std::fs::remove_file(path);
    // a model the endpoint doesn't serve is refused by the API with its status
    let script = EditorScript::replace("model_id: m2", "model_id: nope");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor]].concat();
    let out = run_with(&owner, "models-profile-edit-refused", &args, &EDIT_ENV).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("400"), "{}", stderr(&out));
    let path = recovery_path(&stderr(&out)).expect("no recovery file named");
    let _ = std::fs::remove_file(path);
    let meta = entity_json(&owner, "profiles", &name).await;
    assert_eq!(meta["metadata"]["ModelProfile"]["model_id"], "m2");
    // a dry run through the editor shows the changes and writes nothing
    let script = EditorScript::replace("description: before", "description: after");
    let editor = script.editor();
    let args = [target.as_slice(), &["--editor", &editor, "--dry-run"]].concat();
    let out = run_with(&owner, "models-profile-edit-dry-run", &args, &EDIT_ENV).await;
    assert_success(&out);
    assert!(stdout(&out).contains("Dry run"), "{}", stdout(&out));
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["description"],
        "before"
    );
    Ok(())
}

/// `profiles update` takes `--file`, `--patch`, `--set` and `--unset`, finding a new endpoint by
/// name, and the flags that change one setting each keep working
#[tokio::test]
// one scenario walks each change source against the same profile
#[allow(clippy::too_many_lines)]
async fn models_profiles_update_documents() -> Result<(), Error> {
    // an owner with two gateway endpoints serving the same models and a profile on the first
    let owner = TestUser::new("models-profile-update").await?;
    allow_test_endpoints().await?;
    let first = gateway(&owner, &["m1", "m2"]).await;
    let second = gateway(&owner, &["m1", "m2"]).await;
    let first_id = entity_json(&owner, "endpoints", &first).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let second_id = entity_json(&owner, "endpoints", &second).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let name = profile_on(&owner, &first_id).await;
    let update = ["models", "profiles", "update", &name];
    // --set changes nested fields
    let args = [
        update.as_slice(),
        &[
            "--set",
            "parameters.temperature=0.9",
            "--set",
            "model_id=m2",
        ],
    ]
    .concat();
    let out = owner.run("models-profile-update-set", &args).await;
    assert_success(&out);
    assert!(
        stdout(&out).contains("updated successfully"),
        "{}",
        stdout(&out)
    );
    let meta = entity_json(&owner, "profiles", &name).await;
    assert_eq!(
        meta["metadata"]["ModelProfile"]["parameters"]["temperature"],
        0.9
    );
    assert_eq!(meta["metadata"]["ModelProfile"]["model_id"], "m2");
    // --set to the current value changes nothing
    let before = modified(&owner, "profiles", &name, "ModelProfile").await;
    let args = [update.as_slice(), &["--set", "model_id=m2"]].concat();
    let out = owner.run("models-profile-update-same", &args).await;
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    assert_eq!(
        modified(&owner, "profiles", &name, "ModelProfile").await,
        before
    );
    // --patch sets limits, and --unset clears them again
    let args = [
        update.as_slice(),
        &["--patch", r#"{"limits": {"context_window": 64}}"#],
    ]
    .concat();
    assert_success(&owner.run("models-profile-update-patch", &args).await);
    let meta = entity_json(&owner, "profiles", &name).await;
    assert_eq!(
        meta["metadata"]["ModelProfile"]["limits"]["context_window"],
        64
    );
    let args = [update.as_slice(), &["--unset", "limits"]].concat();
    assert_success(&owner.run("models-profile-update-unset", &args).await);
    assert!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["limits"]
            .is_null()
    );
    // a new endpoint is found by name and by id
    let setting = format!("endpoint={second}");
    let args = [update.as_slice(), &["--set", &setting]].concat();
    assert_success(&owner.run("models-profile-update-endpoint", &args).await);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["endpoint"],
        second_id
    );
    let setting = format!("endpoint={first_id}");
    let args = [update.as_slice(), &["--set", &setting]].concat();
    assert_success(&owner.run("models-profile-update-endpoint-id", &args).await);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["endpoint"],
        first_id
    );
    // --file replaces the document, from a path and from stdin
    let mut doc = spec_doc(&owner, "profiles", &name).await;
    doc["description"] = json!("from-file");
    let file = write_file("profile-doc", &serde_norway::to_string(&doc).unwrap());
    let args = [update.as_slice(), &["--file", &file]].concat();
    assert_success(&owner.run("models-profile-update-file", &args).await);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["description"],
        "from-file"
    );
    doc["description"] = json!("from-stdin");
    let args = [update.as_slice(), &["--file", "-"]].concat();
    let out = run_with_stdin(&owner, &args, &serde_norway::to_string(&doc).unwrap()).await;
    assert_success(&out);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["description"],
        "from-stdin"
    );
    // a misspelled parameter or an unknown protocol is refused before any change
    let before = modified(&owner, "profiles", &name, "ModelProfile").await;
    for (transcript, setting) in [
        (
            "models-profile-update-unknown-parameter",
            "parameters.temprature=1",
        ),
        ("models-profile-update-unknown-api", "api=Telepathy"),
    ] {
        let args = [update.as_slice(), &["--set", setting]].concat();
        let out = owner.run(transcript, &args).await;
        assert!(!out.status.success(), "{transcript} was accepted");
    }
    assert_eq!(
        modified(&owner, "profiles", &name, "ModelProfile").await,
        before
    );
    // a model the endpoint doesn't serve prints the API's status
    let args = [update.as_slice(), &["--set", "model_id=nope"]].concat();
    let out = owner.run("models-profile-update-refused", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("400"), "{}", stderr(&out));
    // an endpoint that doesn't exist is refused
    let args = [update.as_slice(), &["--set", "endpoint=no-such-endpoint"]].concat();
    assert!(
        !owner
            .run("models-profile-update-missing", &args)
            .await
            .status
            .success()
    );
    // read-only fields can't be set
    let args = [update.as_slice(), &["--set", "*id*=abc"]].concat();
    let out = owner.run("models-profile-update-readonly", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("read-only"), "{}", stderr(&out));
    // --dry-run prints the changes and writes nothing, and -o json pins the result's shape
    let args = [
        update.as_slice(),
        &["--set", "model_id=m1", "--dry-run", "-o", "json"],
    ]
    .concat();
    let out = owner.run("models-profile-update-dry-run", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["status"], "dry_run");
    assert_eq!(result["kind"], "model profile");
    assert_eq!(result["diff"][0]["path"], "model_id");
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["model_id"],
        "m2"
    );
    let args = [update.as_slice(), &["--set", "model_id=m1", "-o", "json"]].concat();
    let out = owner.run("models-profile-update-json", &args).await;
    assert_success(&out);
    let result = json(&out);
    assert_eq!(result["status"], "updated");
    assert_eq!(result["changed"], true);
    assert_eq!(result["resource"]["model_id"], "m1");
    // a change made by someone else between the read and the apply is refused unless forced
    let hook = format!(
        "{} --keys {} --skip-update models profiles update {name} --api OpenAiResponses",
        env!("CARGO_BIN_EXE_thorctl"),
        owner.keys.display()
    );
    let envs = [("THORCTL_EDIT_BEFORE_APPLY", hook.as_str())];
    let args = [update.as_slice(), &["--set", "model_id=m2"]].concat();
    let out = run_with(&owner, "models-profile-update-guard", &args, &envs).await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("changed remotely"),
        "{}",
        stderr(&out)
    );
    let args = [update.as_slice(), &["--set", "model_id=m2", "--force"]].concat();
    assert_success(&run_with(&owner, "models-profile-update-force", &args, &envs).await);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["model_id"],
        "m2"
    );
    // the flags that change one setting each can't be mixed with a document change, and still
    // work on their own
    let args = [
        update.as_slice(),
        &["--set", "model_id=m1", "--clear-limits"],
    ]
    .concat();
    let out = owner.run("models-profile-update-mixed", &args).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not both"), "{}", stderr(&out));
    let args = [update.as_slice(), &["--api", "OpenAiChat"]].concat();
    assert_success(&owner.run("models-profile-update-flag", &args).await);
    assert_eq!(
        entity_json(&owner, "profiles", &name).await["metadata"]["ModelProfile"]["api"],
        "OpenAiChat"
    );
    // the printed spec feeds straight back with no change
    let out = owner
        .run(
            "models-profile-spec",
            &["models", "profiles", "spec", &name],
        )
        .await;
    assert_success(&out);
    let args = [update.as_slice(), &["--file", "-"]].concat();
    let out = run_with_stdin(&owner, &args, &stdout(&out)).await;
    assert!(stdout(&out).contains("No changes"), "{}", stdout(&out));
    Ok(())
}
