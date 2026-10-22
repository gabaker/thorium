//! Tests for the files a per-run worker writes for its jobs

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use thorium::models::{
    AwsRoleSource, AzureClientAuth, CredentialKind, CredentialSecrets, HarnessCredential,
    HarnessEnv, HarnessEnvVar, HarnessInterface, HarnessModelEntry, HarnessModels, ModelApi,
    ModelConfigRef, ModelProvider, ModelSecret, RunModels,
};
use uuid::Uuid;

use super::{HarnessPaths, RunEnv, write_for_job, write_run_files};
use crate::libs::test_server;

/// The run-scoped token the tests' runs hold
const RUN_TOKEN: &str = "run-scoped-token-value";

/// Build a credential with a primary secret as a harness gets it
///
/// # Arguments
///
/// * `kind` - The credential's kind
/// * `secret` - Its primary secret, if it has one
/// * `source` - The access key an assumed role uses
fn credential(
    kind: CredentialKind,
    secret: Option<&str>,
    source: Option<HarnessCredential>,
) -> HarnessCredential {
    HarnessCredential::new(
        ModelConfigRef::Group(Uuid::new_v4()),
        kind,
        CredentialSecrets {
            primary: secret.map(ModelSecret::from),
            session_token: None,
            fingerprint: None,
        },
        source,
    )
}

/// Build an agent's model entry with a credential
///
/// # Arguments
///
/// * `provider` - The model's provider
/// * `credential` - The credential, if any
fn entry(provider: ModelProvider, credential: Option<HarnessCredential>) -> HarnessModelEntry {
    HarnessModelEntry {
        provider,
        model_id: "model".to_owned(),
        api: ModelApi::OpenAiChat,
        parameters: thorium::models::ModelParameters::default(),
        limits: thorium::models::ModelLimits::default(),
        capabilities: thorium::models::ModelCapabilities::default(),
        credential,
    }
}

/// Build the paths under a scratch directory a test writes to
///
/// # Arguments
///
/// * `dir` - The scratch directory
fn paths(dir: &Path) -> HarnessPaths {
    HarnessPaths {
        keys: vec![
            dir.join("harness/keys.yml"),
            dir.join("tmp/keys.yml"),
            dir.join("opt/keys.yml"),
        ],
        models: dir.join("ephemeral/models.json"),
        credentials: dir.join("credentials"),
    }
}

/// Get a file's mode bits
///
/// # Arguments
///
/// * `path` - The file
fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A run env needs both the url and token, never prints its token, and builds a client
#[tokio::test]
async fn run_env_from_values() {
    // both values make a run env, with an empty models value ignored
    let url = || Some("http://api.thorium:80".to_owned());
    let token = || Some(RUN_TOKEN.to_owned());
    let env = RunEnv::from_values(url(), token(), Some(String::new())).unwrap();
    assert_eq!(env.url, "http://api.thorium:80");
    assert_eq!(env.token, RUN_TOKEN);
    assert!(env.models.is_none());
    // either value missing or empty means the keys file is used
    assert!(RunEnv::from_values(url(), None, None).is_none());
    assert!(RunEnv::from_values(None, token(), None).is_none());
    assert!(RunEnv::from_values(url(), Some(String::new()), None).is_none());
    // its debug output holds no secret
    let env = RunEnv::from_values(url(), token(), Some("{\"agents\":{}}".to_owned())).unwrap();
    let debug = format!("{env:?}");
    assert!(
        !debug.contains(RUN_TOKEN) && !debug.contains("agents"),
        "{debug}"
    );
}

/// The standard paths put the keys where the harness reads them and at both standard paths,
/// and the credentials where the env values the API gives the harness point
#[test]
fn standard_paths() {
    // a harness reading its keys and models from its own paths
    let mut interface = HarnessInterface::default();
    interface.inputs.thorium_keys = "/work/keys.yml".to_owned();
    interface.inputs.models = "/work/models.json".to_owned();
    let paths = HarnessPaths::standard(Some(&interface));
    assert_eq!(
        paths.keys,
        vec![
            PathBuf::from("/work/keys.yml"),
            PathBuf::from("/tmp/thorium/keys.yml"),
            PathBuf::from("/opt/thorium-keys/keys.yml"),
        ]
    );
    assert_eq!(paths.models, PathBuf::from("/work/models.json"));
    assert_eq!(paths.credentials, PathBuf::from("/tmp/thorium/credentials"));
    // a tool job writes the keys at both standard paths once each
    let paths = HarnessPaths::standard(None);
    assert_eq!(
        paths.keys,
        vec![
            PathBuf::from("/tmp/thorium/keys.yml"),
            PathBuf::from("/opt/thorium-keys/keys.yml"),
        ]
    );
    // the harness env values the API builds name the files the agent writes
    let gcp = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        Some("{}"),
        None,
    );
    let vertex = ModelProvider::Vertex {
        project_id: Some("proj".to_owned()),
        location: thorium::models::VertexLocation::Global,
        endpoint_url: None,
    };
    let env = HarnessEnv {
        export: [HarnessEnvVar::GoogleApplicationCredentials]
            .into_iter()
            .collect(),
        names: BTreeMap::new(),
    };
    let values = RunModels::harness_env(&env, &entry(vertex, Some(gcp.clone())), "/models.json");
    assert_eq!(
        values["GOOGLE_APPLICATION_CREDENTIALS"].expose(),
        paths
            .credentials
            .join(gcp.file.unwrap())
            .to_string_lossy()
            .as_ref()
    );
}

/// A harness's per-run worker writes the keys file at every keys path, the models file, and one
/// file per credential SDKs read from a file, each readable only by the runner; a tool run's
/// worker writes only the keys file, and a shared worker writes nothing
#[test]
#[allow(clippy::too_many_lines)]
fn harness_files_written() {
    // a run whose agents use a GCP JSON, an Entra certificate, an assumed role, and an API key
    let dir = test_server::scratch();
    let paths = paths(&dir);
    let gcp = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        Some("{\"type\":\"service_account\"}"),
        None,
    );
    let cert = credential(
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant".to_owned(),
            client_id: "client".to_owned(),
            auth: AzureClientAuth::Certificate,
        },
        Some("-----BEGIN CERTIFICATE-----"),
        None,
    );
    let key = credential(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIAEXAMPLE".to_owned(),
        },
        Some("aws-secret"),
        None,
    );
    let role = credential(
        CredentialKind::AwsAssumeRole {
            role_arn: "arn:aws:iam::1:role/triage".to_owned(),
            external_id: None,
            session_name: None,
            duration_seconds: None,
            source: AwsRoleSource::AccessKey(Uuid::new_v4()),
        },
        None,
        Some(key),
    );
    let api_key = credential(CredentialKind::ApiKey, Some("sk-plain"), None);
    let openai = ModelProvider::OpenAi {
        base_url: None,
        organization: None,
        project: None,
    };
    let mut models = HarnessModels::default();
    for credential in [gcp.clone(), cert.clone(), role, api_key] {
        models
            .agents
            .insert(Uuid::new_v4(), entry(openai.clone(), Some(credential)));
    }
    let env = RunEnv {
        url: "http://api.thorium:80".to_owned(),
        token: RUN_TOKEN.to_owned(),
        models: Some(serde_json::to_string(&models).unwrap()),
    };
    // a file left with a looser mode is replaced
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    std::fs::write(&paths.keys[1], "stale").unwrap();
    std::fs::set_permissions(&paths.keys[1], std::fs::Permissions::from_mode(0o644)).unwrap();
    write_run_files(&env, Some(&HarnessInterface::default()), &paths).unwrap();
    // every keys file holds the url and the run-scoped token, readable only by the runner
    let uid = std::fs::metadata(&dir).unwrap().uid();
    for path in &paths.keys {
        let keys: thorium::client::Keys =
            serde_norway::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(keys.api, "http://api.thorium:80");
        assert_eq!(keys.token.as_deref(), Some(RUN_TOKEN));
        assert_eq!(mode(path), 0o600);
        assert_eq!(std::fs::metadata(path).unwrap().uid(), uid);
    }
    // the models file holds every agent's model
    let written: HarnessModels =
        serde_json::from_slice(&std::fs::read(&paths.models).unwrap()).unwrap();
    assert_eq!(written, models);
    assert_eq!(mode(&paths.models), 0o600);
    // one file per file-read credential, and none for the API key
    let mut files = std::fs::read_dir(&paths.credentials)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<String>>();
    files.sort();
    let mut expected = vec![
        gcp.file.clone().unwrap(),
        cert.file.clone().unwrap(),
        "aws-config".to_owned(),
    ];
    expected.sort();
    assert_eq!(files, expected);
    for file in &files {
        let path = paths.credentials.join(file);
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::metadata(&path).unwrap().uid(), uid);
    }
    assert_eq!(
        std::fs::read_to_string(paths.credentials.join(gcp.file.unwrap())).unwrap(),
        "{\"type\":\"service_account\"}"
    );
    assert_eq!(
        std::fs::read_to_string(paths.credentials.join(cert.file.unwrap())).unwrap(),
        "-----BEGIN CERTIFICATE-----"
    );
    let config = std::fs::read_to_string(paths.credentials.join("aws-config")).unwrap();
    assert!(config.contains("[profile thorium]"));
    assert!(config.contains("role_arn = arn:aws:iam::1:role/triage"));
    assert!(config.contains("aws_access_key_id = AKIAEXAMPLE"));
    // a tool run's worker writes only the keys file
    let tool_dir = test_server::scratch();
    let tool_paths = paths_for_tool(&tool_dir);
    write_run_files(&env, None, &tool_paths).unwrap();
    assert!(tool_paths.keys.iter().all(|path| path.exists()));
    assert!(!tool_paths.models.exists());
    assert!(!tool_paths.credentials.exists());
    // a shared worker writes nothing
    let shared_dir = test_server::scratch();
    let shared_paths = self::paths(&shared_dir);
    write_for_job(None, Some(&HarnessInterface::default()), &shared_paths).unwrap();
    assert_eq!(std::fs::read_dir(&shared_dir).unwrap().count(), 0);
    // clean up
    for dir in [dir, tool_dir, shared_dir] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// Build the paths a tool run's worker writes to under a scratch directory
///
/// # Arguments
///
/// * `dir` - The scratch directory
fn paths_for_tool(dir: &Path) -> HarnessPaths {
    paths(dir)
}

/// A harness's worker without its models, invalid models, or a credential file name that leaves
/// the credentials directory fails before anything is written there
#[test]
fn bad_models_refused() {
    // a harness job with no models
    let dir = test_server::scratch();
    let paths = paths(&dir);
    let mut env = RunEnv {
        url: "http://api.thorium:80".to_owned(),
        token: RUN_TOKEN.to_owned(),
        models: None,
    };
    let harness = HarnessInterface::default();
    assert!(write_run_files(&env, Some(&harness), &paths).is_err());
    // invalid models are refused without echoing them
    env.models = Some("{\"agents\": sk-leak".to_owned());
    let error = write_run_files(&env, Some(&harness), &paths)
        .unwrap_err()
        .to_string();
    assert!(!error.contains("sk-leak"), "{error}");
    // a credential file name with a path is refused
    let mut gcp = credential(
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        Some("{}"),
        None,
    );
    gcp.file = Some("../escape.json".to_owned());
    let mut models = HarnessModels::default();
    models.agents.insert(
        Uuid::new_v4(),
        entry(
            ModelProvider::Vertex {
                project_id: Some("p".to_owned()),
                location: thorium::models::VertexLocation::Global,
                endpoint_url: None,
            },
            Some(gcp),
        ),
    );
    env.models = Some(serde_json::to_string(&models).unwrap());
    assert!(write_run_files(&env, Some(&harness), &paths).is_err());
    assert!(!dir.join("escape.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
}
