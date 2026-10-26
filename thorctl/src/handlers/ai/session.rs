//! Build ThorPal sessions from the Thorctl config

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use thorium::client::conf::ResolvedAiProvider;
use thorium::{CtlConf, Error, Thorium};
use thorpal::{
    AiProviderConfig, CredentialRefresher, GhidraSettings, PauseController, PauseStatus,
    SessionConfig, ThorPalEvent, ThoriumMcpAuth, ThoriumMcpResolver, Workflow,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::args::ai::PauseWait;

/// Builds the credential refresher a paused session probes with
///
/// Re-reads the thorctl config file and resolves its `ai` block into a fully
/// credentialed provider config, so an operator can fix an expired key in the
/// config and the paused session picks it up on its next probe without
/// restarting. The request models are pinned per agent at build time, so a
/// drifted `model` field in the re-read config cannot change what a running
/// session talks to.
///
/// # Arguments
///
/// * `config_path` - The thorctl config file to re-read on each refresh
pub(super) fn credential_refresher(config_path: PathBuf) -> CredentialRefresher {
    Arc::new(move || {
        // clone the path into the returned future
        let config_path = config_path.clone();
        Box::pin(async move {
            // re-read the config file for the freshest credentials
            let conf = CtlConf::from_path(&config_path).map_err(|err| {
                anyhow::anyhow!("failed to re-read '{}': {err}", config_path.display())
            })?;
            // resolve the ai block into a fully-credentialed provider config
            let ai = conf.ai.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "the re-read config '{}' no longer has an `ai` block",
                    config_path.display()
                )
            })?;
            let resolved = ai.resolve().map_err(|err| anyhow::anyhow!("{err}"))?;
            let mut provider = to_thorpal_provider(resolved);
            // batch runs deliver credentials through an env file, not the
            // config: overlay the freshest file material when one is in play
            overlay_env_file_credentials(&mut provider);
            Ok(provider)
        })
    })
}

/// Builds the resolver minting the built-in Thorium MCP server's credentials
///
/// Mirrors thorctl's own client bootstrap precedence: an activated scoped
/// token wins, then basic auth mints a fresh token, then an explicit raw token
/// from the keys is used as-is. The resolver is invoked lazily -- only when a
/// workflow enables `thorium_mcp` and an agent references the built-in server
/// -- so ordinary offline runs never touch the network. The MCP endpoint
/// expects the raw token (it wraps the token in its own onward client, which
/// base64-encodes it), so no encoding happens here. Of the config's client
/// settings only `invalid_certs` carries over to the MCP connection; custom
/// certificate authorities and proxy settings do not apply to it.
///
/// # Arguments
///
/// * `conf` - The thorctl config to resolve credentials from
pub(super) fn thorium_mcp_resolver(conf: &CtlConf) -> ThoriumMcpResolver {
    // the resolver owns its own copy of the config
    let conf = conf.clone();
    Arc::new(move || {
        // clone again so each invocation owns its inputs
        let conf = conf.clone();
        Box::pin(async move {
            // the MCP endpoint lives under the configured API url
            let url = conf.keys.api.clone();
            // resolve a raw token with the same precedence as `get_client`
            let token = if let Some(active) = &conf.scoped_token {
                // an activated scoped token replaces the primary credentials
                active.token.clone()
            } else if let (Some(username), Some(password)) =
                (&conf.keys.username, &conf.keys.password)
            {
                // basic auth mints a fresh raw token from the API, tolerating
                // the same invalid certs thorctl's own client tolerates
                let client = reqwest::Client::builder()
                    .danger_accept_invalid_certs(conf.client.invalid_certs)
                    .build()
                    .map_err(|err| anyhow::anyhow!("failed to build an http client: {err}"))?;
                let (token, _expires) = Thorium::auth(&url, username, password, &client)
                    .await
                    .map_err(|err| anyhow::anyhow!("failed to authenticate with Thorium: {err}"))?;
                token
            } else if let Some(token) = &conf.keys.token {
                // an explicit token from the keys is already raw
                token.clone()
            } else {
                anyhow::bail!(
                    "the thorctl config has no Thorium credentials (token or \
                    username/password) to reach the built-in Thorium MCP server with"
                );
            };
            Ok(ThoriumMcpAuth {
                url,
                token,
                // the MCP connection tolerates the same invalid certs
                accept_invalid_certs: conf.client.invalid_certs,
            })
        })
    })
}

/// Overlay AWS credentials from a batch env file onto a refreshed config
///
/// Batch runs source their credentials from an env file at spawn time, so a
/// RUNNING session's process env is frozen: when an operator refreshes the
/// file, the pause gate's re-read of the thorctl config finds no credentials
/// (the config defers to the SDK chain) and every probe keeps signing with
/// the stale material — the dev-10 validation run measured probes still
/// failing with 401 after a refresh. When the pool advertises the env file
/// via `THORPAL_CREDS_ENV_FILE`, a refresh re-parses it and sets the fresh
/// key material explicitly, which outranks the chain downstream.
///
/// # Arguments
///
/// * `config` - The freshly re-read provider config to overlay onto
fn overlay_env_file_credentials(config: &mut AiProviderConfig) {
    // only a pool-advertised env file can hold fresher material
    let Ok(path) = std::env::var("THORPAL_CREDS_ENV_FILE") else {
        return;
    };
    // parse with the pool's own parser so the accepted formats stay aligned
    match super::batch::scan::parse_env_file(Path::new(&path)) {
        Ok(vars) => apply_env_credentials(config, vars),
        Err(error) => {
            // a missing or garbled file is not fatal: probe with the config
            // exactly as it was re-read
            tracing::warn!(
                path,
                ?error,
                "failed to parse the creds env file; probing without the overlay"
            );
        }
    }
}

/// Apply parsed env-file variables as explicit Bedrock credentials
///
/// An explicit key pair is required to outrank the SDK chain, so a file
/// missing either half leaves the config untouched; the session token is
/// optional and tracks whatever the file carries. Non-Bedrock providers are
/// left as re-read — their credentials live in the config itself.
///
/// # Arguments
///
/// * `config` - The provider config to set credentials on
/// * `vars` - The `KEY=VALUE` pairs parsed from the env file
fn apply_env_credentials(config: &mut AiProviderConfig, vars: Vec<(String, String)>) {
    // index the parsed pairs for the three AWS fields
    let mut map: std::collections::HashMap<String, String> = vars.into_iter().collect();
    // require the full pair before outranking the chain
    let (Some(access), Some(secret)) = (
        map.remove("AWS_ACCESS_KEY_ID"),
        map.remove("AWS_SECRET_ACCESS_KEY"),
    ) else {
        return;
    };
    // set the fresh material on the Bedrock variant only
    if let AiProviderConfig::Bedrock {
        access_key_id,
        secret_access_key,
        session_token,
        ..
    } = config
    {
        *access_key_id = Some(access);
        *secret_access_key = Some(secret);
        *session_token = map.remove("AWS_SESSION_TOKEN");
    }
}

/// Absolutize a path and lexically normalize away `.`/`..` components
///
/// `std::path::absolute` is purely lexical and keeps `..` components, which
/// produces persisted paths like `thorctl/../../test_data/0fl` in saved
/// sessions and error messages. This strips `.` components and resolves each
/// `..` against the preceding normal component without touching the
/// filesystem (no symlink resolution), never popping past the root.
///
/// # Arguments
///
/// * `path` - The path to absolutize and normalize
pub(super) fn clean_absolute(path: &Path) -> Result<PathBuf, Error> {
    // make the path absolute first so every `..` has something to resolve against
    let absolute = std::path::absolute(path)
        .map_err(|err| Error::new(format!("failed to absolutize the path: {err}")))?;
    // rebuild the path component by component, folding `.`/`..` away lexically
    let mut clean = PathBuf::new();
    for component in absolute.components() {
        match component {
            // drop no-op `.` components entirely
            std::path::Component::CurDir => {}
            // resolve `..` by popping the last normal component; the root
            // absorbs any excess `..` (POSIX defines `/..` as `/`)
            std::path::Component::ParentDir => {
                clean.pop();
            }
            // keep roots, prefixes, and normal components as-is
            _ => clean.push(component),
        }
    }
    Ok(clean)
}

/// Map the Thorctl config's resolved AI provider onto ThorPal's provider config
///
/// The two types mirror each other but live in different crates (the Thorctl
/// config must not depend on thorpal), so this is a straight field-for-field
/// conversion.
///
/// # Arguments
///
/// * `resolved` - The active provider resolved from the Thorctl config
pub(super) fn to_thorpal_provider(resolved: ResolvedAiProvider) -> AiProviderConfig {
    // move each provider's settings into the matching thorpal variant
    match resolved {
        ResolvedAiProvider::OpenAi(openai) => AiProviderConfig::OpenAi {
            endpoint: openai.endpoint,
            api_key: openai.api_key,
            model: openai.model,
        },
        ResolvedAiProvider::LiteLlm(litellm) => AiProviderConfig::LiteLlm {
            endpoint: litellm.endpoint,
            api_key: litellm.api_key,
            model: litellm.model,
        },
        ResolvedAiProvider::Anthropic(anthropic) => AiProviderConfig::Anthropic {
            endpoint: anthropic.endpoint,
            api_key: anthropic.api_key,
            model: anthropic.model,
        },
        ResolvedAiProvider::Bedrock(bedrock) => AiProviderConfig::Bedrock {
            region: bedrock.region,
            profile: bedrock.profile,
            mantle: bedrock.mantle,
            access_key_id: bedrock.access_key_id,
            secret_access_key: bedrock.secret_access_key,
            session_token: bedrock.session_token,
            model: bedrock.model,
        },
    }
}

/// Build a ThorPal session config for a local binary from the Thorctl config
///
/// The LLM provider comes from the `ai` block of the Thorctl config, which
/// names the provider and nests its settings under a block named after it. The
/// Ghidra install and workflow come from their flags when set, then the `ai`
/// block. A missing Ghidra install is not an error here; ThorPal refuses to build
/// only if a configured agent actually uses the Ghidra tools. The binary and
/// projects paths are absolutized so a saved session can be resumed from any
/// working directory.
///
/// # Arguments
///
/// * `conf` - The Thorctl config whose `ai` block supplies the LLM provider
/// * `binary` - The local binary to analyze
/// * `extra_binaries` - Sibling samples analyzed alongside the primary in the
///   same session (each becomes an additional Ghidra program), empty for a
///   classic single-sample run
/// * `ghidra_install` - The Ghidra install passed on the command line if any
/// * `workflow` - The workflow path passed on the command line if any
/// * `name` - The explicit session name passed on the command line if any
/// * `fresh_project` - Always create a new Ghidra project instead of reusing one
/// * `anonymize` - Analyze anonymized SHA-256-named staged copies of the
///   samples so the model never sees the original file names or paths
/// * `redact_tokens` - Deny tokens scrubbed out of every model-visible
///   script-library surface (see `load_redact_tokens`), empty for none
/// * `blind_families` - The analyzed family's name and aliases, whose
///   cross-run `family-intel` memories are hidden entirely, empty for none
#[allow(clippy::too_many_arguments)]
pub async fn build_config(
    conf: &CtlConf,
    binary: &Path,
    extra_binaries: &[PathBuf],
    ghidra_install: Option<&PathBuf>,
    workflow: Option<&PathBuf>,
    name: Option<&String>,
    fresh_project: bool,
    anonymize: bool,
    redact_tokens: Vec<String>,
    blind_families: Vec<String>,
) -> Result<SessionConfig, Error> {
    // ThorPal requires an ai block naming the LLM provider to talk to
    let ai = conf.ai.as_ref().ok_or_else(|| {
        Error::new(
            "No `ai` config set: ThorPal needs an `ai` block with a provider \
            section such as `openai`, `litellm`, `anthropic`, or `bedrock`",
        )
    })?;
    // resolve which provider is active before any heavier validation runs
    let provider = to_thorpal_provider(ai.resolve()?);
    // reject an invalid session name before any heavier work runs
    if let Some(name) = name {
        thorpal::validate_session_name(name).map_err(Error::new)?;
    }
    // validate the binary up front so a bad path fails before the analysis starts
    if !binary.is_file() {
        return Err(Error::new(format!(
            "The binary path '{}' does not exist or is not a file",
            binary.display(),
        )));
    }
    // absolutize and normalize the binary so a resumed session finds it from
    // any cwd and never persists `..` components
    let binary = clean_absolute(binary)?;
    // prefer the Ghidra install from the command line over the config
    let ghidra_install = ghidra_install.or(ai.ghidra_install.as_ref()).cloned();
    // validate a configured Ghidra install so a typo fails before the analysis starts
    if let Some(install) = ghidra_install.as_ref().filter(|install| !install.is_dir()) {
        return Err(Error::new(format!(
            "The Ghidra install path '{}' does not exist or is not a directory",
            install.display(),
        )));
    }
    // use the configured projects directory or fall back to the shared default
    let projects_dir = match ai.ghidra_projects.clone() {
        Some(dir) => dir,
        None => {
            thorpal::default_ghidra_projects_dir().map_err(|err| Error::new(format!("{err:#}")))?
        }
    };
    // absolutize and normalize the projects dir too, so the saved session's
    // project resolves identically no matter where thorctl is later resumed from
    let projects_dir = clean_absolute(&projects_dir)?;
    // reject a projects dir Ghidra itself would refuse (any path element
    // starting with '.'), so a config typo fails here with the real reason
    // instead of as a bare analyzeHeadless exit 1 mid-analysis
    thorpal::validate_projects_dir(&projects_dir).map_err(Error::new)?;
    // when anonymizing, stage a SHA-256-named copy under the projects root and
    // analyze that instead, so no model-visible path or name carries the
    // operator's naming; the pre-staging path is deliberately not recorded
    // anywhere in the session store (anonymize-at-rest)
    let binary = match anonymize {
        true => thorpal::stage_anonymized_sample(&binary, &projects_dir)
            .await
            .map_err(|err| Error::new(format!("{err:#}")))?,
        false => binary,
    };
    // validate, absolutize, and (under --anonymize) stage every sibling
    // sample, dropping duplicates whose content the session already holds
    let mut extra_samples: Vec<thorpal::ExtraSampleMeta> = Vec::new();
    for extra in extra_binaries {
        // validate each sibling up front so a bad path fails before analysis
        if !extra.is_file() {
            return Err(Error::new(format!(
                "The sibling binary path '{}' does not exist or is not a file",
                extra.display(),
            )));
        }
        let extra = clean_absolute(extra)?;
        // stage each sibling by its own SHA-256 under --anonymize, exactly
        // like the primary, so every model-visible name is content-derived;
        // the pre-staging path survives only as a local for the duplicate
        // warning below and is never recorded in the store
        let (analyzed, original) = match anonymize {
            true => {
                let staged = thorpal::stage_anonymized_sample(&extra, &projects_dir)
                    .await
                    .map_err(|err| Error::new(format!("{err:#}")))?;
                (staged, Some(extra))
            }
            false => (extra, None),
        };
        // identical content stages to the same copy (and the same path twice
        // is the same sample): skip it with a warning, it adds nothing
        if analyzed == binary || extra_samples.iter().any(|sample| sample.binary == analyzed) {
            eprintln!(
                "warning: skipping duplicate sample '{}' (same content as an earlier one)",
                original.as_deref().unwrap_or(&analyzed).display()
            );
            continue;
        }
        extra_samples.push(thorpal::ExtraSampleMeta { binary: analyzed });
    }
    // sibling programs are named by file name, so every sample needs a
    // distinct one (sha staging guarantees it under --anonymize)
    let mut names: Vec<_> = std::iter::once(binary.as_path())
        .chain(extra_samples.iter().map(|sample| sample.binary.as_path()))
        .filter_map(Path::file_name)
        .collect();
    let total = names.len();
    names.sort();
    names.dedup();
    if names.len() != total {
        return Err(Error::new(
            "every sample in a multi-sample session needs a distinct file name \
             (sibling programs are named by file name); rename the colliding \
             files or pass --anonymize to stage them by content hash",
        ));
    }
    // resolve the nested analysis folder for this binary, reusing the most
    // recent analysis unless a fresh project was demanded
    let mut ghidra =
        GhidraSettings::for_analysis(binary, projects_dir, ghidra_install, fresh_project)
            .map_err(|err| Error::new(format!("{err:#}")))?;
    // the analyzed sibling paths ride the Ghidra settings into the bootstrap
    // import and the session snapshot
    ghidra.extra_binaries = extra_samples
        .iter()
        .map(|sample| sample.binary.clone())
        .collect();
    // load the workflow from the command line, then the config, then the builtin one
    let workflow = match workflow.or(ai.workflow.as_ref()) {
        Some(path) => Workflow::load(path).await,
        None => Workflow::builtin(),
    }
    .map_err(|err| Error::new(format!("{err:#}")))?;
    // build the run config around the explicitly configured provider
    let mut config = SessionConfig::new(provider, ghidra, workflow);
    // record the operator's explicit session name when one was given
    config.session_name = name.cloned();
    // point the persistent script library at the configured directory, when
    // one is set (the default lives under ~/.thorium inside thorpal)
    config.scripts_dir = ai.scripts_dir.clone();
    // point the persistent memory library at the configured directory too
    config.memories_dir = ai.memories_dir.clone();
    // point the seed-time YARA scan at the configured rules, when set
    config.yara_rules = ai.yara_rules.clone();
    // record the anonymization fact so a resume keeps blinding library names
    config.anonymized = anonymize;
    // install the operator's library deny tokens (never persisted; the model
    // only ever sees their effects on library surfaces)
    config.redact_tokens = redact_tokens;
    // install the blinded family's aliases (never persisted; matching
    // family-intel memories are hidden outright)
    config.blind_families = blind_families;
    // record the sibling samples' staged paths for meta.json
    config.extra_samples = extra_samples;
    // install the built-in Thorium MCP server's credential resolver; it is
    // invoked lazily, so runs that never reference the server stay offline
    config.thorium_mcp = Some(thorium_mcp_resolver(conf));
    Ok(config)
}

/// Load the operator's library deny-token file, when one was passed
///
/// One case-insensitive token per line; blank lines and `#` comments are
/// skipped. Prefer tokens of at least 4 characters -- shorter ones over-redact
/// ordinary prose.
///
/// # Arguments
///
/// * `path` - The `--redact-tokens` file path, if the flag was passed
pub(super) async fn load_redact_tokens(path: Option<&PathBuf>) -> Result<Vec<String>, Error> {
    // no flag means no deny list; library surfaces render verbatim
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    // read and parse the operator's token file, failing loudly on a bad path
    let contents = tokio::fs::read_to_string(path).await.map_err(|err| {
        Error::new(format!(
            "failed to read the --redact-tokens file '{}': {err}",
            path.display()
        ))
    })?;
    let tokens = thorpal::parse_token_file(&contents);
    // an empty parsed list is almost certainly a mistake worth failing on
    if tokens.is_empty() {
        return Err(Error::new(format!(
            "the --redact-tokens file '{}' contains no tokens",
            path.display()
        )));
    }
    Ok(tokens)
}

/// Resolve the default report template: the flag outranks the `ai` config
///
/// Leaving both unset returns `None`, which falls back to the builtin template
/// inside thorpal. An explicitly resolved template must exist, so a typo fails
/// before the session claims a store directory (and, for the TUI, before it
/// takes the screen).
///
/// # Arguments
///
/// * `conf` - The Thorctl config whose `ai` block may name a default template
/// * `flag` - The template path passed on the command line if any
pub(super) fn resolve_report_template(
    conf: &CtlConf,
    flag: Option<&PathBuf>,
) -> Result<Option<PathBuf>, Error> {
    // the command line flag outranks the ai config's default template
    let template = flag
        .cloned()
        .or_else(|| conf.ai.as_ref().and_then(|ai| ai.report_template.clone()));
    // an explicitly resolved template must exist before any heavier work runs
    if let Some(template) = &template
        && !template.is_file()
    {
        return Err(Error::new(format!(
            "the report template '{}' does not exist or is not a file",
            template.display()
        )));
    }
    Ok(template)
}

/// Re-inject live secrets into a session's stored (sanitized) provider config
///
/// Session snapshots never carry credentials, so a resume takes them from the
/// operator's live config instead: when the stored and live configs are the
/// same provider, the stored endpoint/model are kept and only the secrets are
/// copied over; when the provider changed since the session was saved, the
/// live config is used wholesale (with a warning) since the stored one can no
/// longer be authenticated.
///
/// # Arguments
///
/// * `stored` - The sanitized provider config loaded from the saved session
/// * `live` - The fully-credentialed provider config from the live Thorctl config
pub(super) fn reinject_secrets(
    stored: AiProviderConfig,
    live: AiProviderConfig,
) -> AiProviderConfig {
    // pair the stored settings with the live secrets variant by variant
    match (stored, live) {
        // same provider: keep the stored endpoint/model, take the live key
        (
            AiProviderConfig::OpenAi {
                endpoint, model, ..
            },
            AiProviderConfig::OpenAi { api_key, .. },
        ) => AiProviderConfig::OpenAi {
            endpoint,
            api_key,
            model,
        },
        (
            AiProviderConfig::LiteLlm {
                endpoint, model, ..
            },
            AiProviderConfig::LiteLlm { api_key, .. },
        ) => AiProviderConfig::LiteLlm {
            endpoint,
            api_key,
            model,
        },
        (
            AiProviderConfig::Anthropic {
                endpoint, model, ..
            },
            AiProviderConfig::Anthropic { api_key, .. },
        ) => AiProviderConfig::Anthropic {
            endpoint,
            api_key,
            model,
        },
        // bedrock's secret is the explicit access key trio, when one is set
        (
            AiProviderConfig::Bedrock {
                region,
                profile,
                mantle,
                model,
                ..
            },
            AiProviderConfig::Bedrock {
                access_key_id,
                secret_access_key,
                session_token,
                ..
            },
        ) => AiProviderConfig::Bedrock {
            region,
            profile,
            mantle,
            access_key_id,
            secret_access_key,
            session_token,
            model,
        },
        // the provider changed since the session was saved: the stored config
        // can no longer be authenticated, so run with the live one wholesale
        // (resume runs before the TUI takes the screen, so printing is safe)
        (stored, live) => {
            eprintln!(
                "warning: the configured AI provider changed since this session was \
                 saved (stored model '{}', live model '{}'); resuming with the live \
                 provider config",
                stored.model(),
                live.model(),
            );
            live
        }
    }
}

/// Print ThorPal run events as plain progress lines on stderr
///
/// The printer drains the session's event channel until every sender clone
/// drops with the session, so it ends on its own once the run is over. Only
/// the events that describe run progress are printed; the TUI-oriented stream
/// (usage ticks, forum refreshes, streamed tool logs) is skipped since the
/// full detail is already persisted in the session store and the log file.
///
/// # Arguments
///
/// * `events` - The receiving end of the session's event channel
pub(super) async fn print_events(mut events: UnboundedReceiver<ThorPalEvent>) {
    // drain events until the session drops its last sender clone
    while let Some(event) = events.recv().await {
        match event {
            // the main agent or a sub-agent produced assistant text
            ThorPalEvent::AgentMessage { agent, text } => eprintln!("[{agent}] {text}"),
            // a sub-agent delegation started (or resumed) on a goal
            ThorPalEvent::SubAgentStarted {
                name,
                goal,
                resumed,
                ..
            } => {
                let label = if resumed { "resume" } else { "spawn" };
                eprintln!("[{label}] {name}: {goal}");
            }
            // a sub-agent's turn budget was extended (or an extension reserved)
            ThorPalEvent::SubAgentExtended {
                name,
                granted,
                justification,
                trigger,
                ..
            } => eprintln!("[turns] {name} +{granted} ({trigger}): {justification}"),
            // a sub-agent's conversation memory crossed a pressure threshold
            ThorPalEvent::SubAgentContextPressure {
                name,
                level,
                used_tokens,
                context_window,
            } => eprintln!(
                "[context] {name} level {level}: ~{used_tokens} of {context_window} tokens"
            ),
            // a sub-agent died; name the class and any salvage forum post
            ThorPalEvent::SubAgentTerminated {
                name,
                reason,
                class,
                salvage_post,
            } => match salvage_post {
                Some(post) => {
                    eprintln!("[died] {name} ({class}): {reason}; handoff posted as #{post}");
                }
                None => eprintln!("[died] {name} ({class}): {reason}"),
            },
            // a swarm of independent sub-agent tasks was spawned (or resumed)
            ThorPalEvent::SwarmStarted {
                name,
                goal,
                members,
                resumed,
            } => {
                let verb = if resumed { " resumed" } else { "" };
                eprintln!("[swarm] {name}{verb} ({} members): {goal}", members.len());
            }
            // every member of a swarm finished, successfully or not
            ThorPalEvent::SwarmFinished {
                name,
                succeeded,
                failed,
            } => eprintln!("[swarm] {name} finished: {succeeded} succeeded, {failed} failed"),
            // a tool call is about to run; keep its args to one bounded line
            ThorPalEvent::ToolCallStarted {
                agent, tool, args, ..
            } => {
                // flatten the raw json args onto a single truncated line
                let flat = args.replace('\n', " ");
                let flat: String = flat.chars().take(120).collect();
                eprintln!("[tool] {agent} -> {tool} {flat}");
            }
            // a tool call finished; flag failures loudly
            ThorPalEvent::ToolCallFinished {
                agent,
                tool,
                success,
                ..
            } => {
                // only failures are worth a line of their own
                if !success {
                    eprintln!("[tool] {agent} -> {tool} FAILED");
                }
            }
            // a turn failed after exhausting its retry budget
            ThorPalEvent::TurnFailed { agent, error } => eprintln!("[error] {agent}: {error}"),
            // a provider request is backing off before another attempt
            ThorPalEvent::LlmRetrying {
                agent,
                attempt,
                max_attempts,
                error,
                ..
            } => eprintln!("[retry] {agent} attempt {attempt}/{max_attempts}: {error}"),
            // a content filter killed a request; a scrubbed retry fired
            ThorPalEvent::LlmScrubbed {
                agent,
                level,
                error,
                ..
            } => eprintln!(
                "[scrubbed] {agent}: content filter blocked a request ({error}); retrying at scrub level {level}"
            ),
            // report generation stepped down its content-filter ladder
            ThorPalEvent::ReportDegraded { rung, detail } => {
                eprintln!("[report] degraded to rung {rung}: {detail}");
            }
            // the whole session paused on a provider fault; the supervisor
            // watches the config file and the --pause-wait cap meanwhile
            ThorPalEvent::SessionPaused {
                kind,
                error,
                probe_attempts,
                next_probe_at,
                ..
            } => eprintln!(
                "[paused] {kind}: {error} ({probe_attempts} failed probe(s); next probe at {})",
                next_probe_at.format("%H:%M:%S UTC")
            ),
            // the pause resolved, one way or the other
            ThorPalEvent::SessionResumed { kind, recovered } => {
                if recovered {
                    eprintln!("[resumed] {kind} recovered");
                } else {
                    eprintln!("[resumed] pause aborted; parked requests surface errors");
                }
            }
            // an analyst ask resolved; headless this is always the recorded
            // default (auto_default), so name the fork that was taken
            ThorPalEvent::AnalystAskResolved {
                agent,
                question,
                chosen,
                source,
                ..
            } => eprintln!("[ask] {agent} ({source}): {question} -> {chosen:?}"),
            // an agent flagged something for the analyst's review queue
            ThorPalEvent::ReviewFlagRaised { agent, title, .. } => {
                eprintln!("[review] {agent} flagged: {title}");
            }
            // a pending ask should never surface headless (nothing attaches a
            // responder), but print it defensively rather than hiding a hang
            ThorPalEvent::AnalystAskPending {
                agent, question, ..
            } => {
                eprintln!("[ask] {agent} is waiting on an analyst: {question}");
            }
            // everything else is TUI-oriented detail headless runs skip
            _ => {}
        }
    }
}

/// The modification time of a file, when it is readable
///
/// # Arguments
///
/// * `path` - The file to stat
async fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    // a missing or unreadable file just reads as "no mtime"
    tokio::fs::metadata(path)
        .await
        .ok()
        .and_then(|meta| meta.modified().ok())
}

/// Supervises a headless run's pauses: config-watch probes and the wait cap
///
/// While the session is paused this polls the thorctl config file's mtime
/// every 2s -- an edit (a refreshed token) triggers an immediate probe -- and
/// enforces `--pause-wait`: with the fail-fast default (`none`) every pause is
/// aborted immediately so the parked requests surface their original errors;
/// with an opted-in wait, expiry aborts the pause, the expired flag tells the
/// caller to exit 75, and the session still finishes cleanly (snapshot +
/// Ghidra flush) so it can be resumed once credentials are fixed.
///
/// # Arguments
///
/// * `controller` - The session's pause controller to watch and steer
/// * `config_path` - The thorctl config file watched for credential edits
/// * `pause_wait` - The configured cap on how long a pause may hold the run
/// * `expired` - The flag set when the cap expires, read by the exit path
pub(super) async fn supervise_pauses(
    controller: PauseController,
    config_path: PathBuf,
    pause_wait: PauseWait,
    expired: Arc<AtomicBool>,
) {
    // watch the session's live pause status
    let mut status = controller.status();
    loop {
        // wait for the session to pause (a closed channel means the run ended)
        loop {
            if matches!(*status.borrow_and_update(), PauseStatus::Paused { .. }) {
                break;
            }
            if status.changed().await.is_err() {
                return;
            }
        }
        // arm the deadline when the wait is bounded
        let deadline = match pause_wait {
            // fail fast: surface the pause, then abort it immediately so the
            // parked requests fail with their original errors
            PauseWait::None => {
                // name the pause we are giving up on, kind and error included
                if let PauseStatus::Paused { kind, error, .. } = &*status.borrow_and_update() {
                    eprintln!("[paused] {kind}: {error}; --pause-wait is none, giving up");
                }
                // the shutdown abort freezes the run for resume: the parked
                // requests surface as interruptions, no failure debris is
                // written, and the caller exits 75 with a resume hint
                controller.abort_for_shutdown();
                // the latch is permanent, so no later pause episode can start;
                // the run is unwinding and this supervisor's work is done
                return;
            }
            PauseWait::Forever => None,
            PauseWait::For(wait) => Some(tokio::time::Instant::now() + wait),
        };
        // the config mtime at pause time; any change means fresh credentials
        let mut last_mtime = file_mtime(&config_path).await;
        // babysit the pause until it resolves
        loop {
            // leave once the session resumed (recovered or aborted)
            if matches!(*status.borrow_and_update(), PauseStatus::Running) {
                break;
            }
            tokio::select! {
                // a status change re-checks the loop condition
                changed = status.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                // poll the config file and the deadline every 2 seconds
                () = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                    // an edited config means fresh credentials: probe right now
                    let mtime = file_mtime(&config_path).await;
                    if mtime != last_mtime {
                        last_mtime = mtime;
                        eprintln!("[paused] config change detected; probing the provider now");
                        controller.retry_now();
                    }
                    // enforce the --pause-wait cap on every pause episode that
                    // outlives its deadline; the store is idempotent, so a
                    // second expiry just re-aborts
                    if let Some(deadline) = deadline
                        && tokio::time::Instant::now() >= deadline
                    {
                        eprintln!("[paused] --pause-wait exceeded; giving up on the pause");
                        expired.store(true, Ordering::Release);
                        // the shutdown abort freezes the run for resume
                        // instead of settling the parked requests as failures
                        controller.abort_for_shutdown();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a chain-credentialed Bedrock config for the overlay tests
    fn bedrock_config() -> AiProviderConfig {
        AiProviderConfig::Bedrock {
            region: None,
            profile: None,
            mantle: true,
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            model: "test-model".to_owned(),
        }
    }

    /// A full env-file pair (plus token) outranks the chain; a partial pair
    /// and a non-Bedrock provider are both left untouched
    #[test]
    fn env_credentials_overlay_requires_the_full_pair() {
        // the full pair plus token lands on the Bedrock variant
        let mut config = bedrock_config();
        apply_env_credentials(
            &mut config,
            vec![
                ("AWS_ACCESS_KEY_ID".to_owned(), "AKID".to_owned()),
                ("AWS_SECRET_ACCESS_KEY".to_owned(), "SECRET".to_owned()),
                ("AWS_SESSION_TOKEN".to_owned(), "TOKEN".to_owned()),
            ],
        );
        let AiProviderConfig::Bedrock {
            access_key_id,
            secret_access_key,
            session_token,
            ..
        } = &config
        else {
            panic!("the provider variant changed");
        };
        assert_eq!(access_key_id.as_deref(), Some("AKID"));
        assert_eq!(secret_access_key.as_deref(), Some("SECRET"));
        assert_eq!(session_token.as_deref(), Some("TOKEN"));
        // a token-less pair clears any stale token rather than keeping it
        apply_env_credentials(
            &mut config,
            vec![
                ("AWS_ACCESS_KEY_ID".to_owned(), "AKID2".to_owned()),
                ("AWS_SECRET_ACCESS_KEY".to_owned(), "SECRET2".to_owned()),
            ],
        );
        let AiProviderConfig::Bedrock {
            access_key_id,
            session_token,
            ..
        } = &config
        else {
            panic!("the provider variant changed");
        };
        assert_eq!(access_key_id.as_deref(), Some("AKID2"));
        assert_eq!(session_token.as_deref(), None);
        // half a pair must not outrank the chain
        let mut partial = bedrock_config();
        apply_env_credentials(
            &mut partial,
            vec![("AWS_ACCESS_KEY_ID".to_owned(), "AKID".to_owned())],
        );
        let AiProviderConfig::Bedrock { access_key_id, .. } = &partial else {
            panic!("the provider variant changed");
        };
        assert_eq!(access_key_id.as_deref(), None);
    }

    /// `..` and `.` components are folded away lexically
    #[test]
    fn clean_absolute_normalizes_dot_components() {
        // a `..` pops the component before it
        let clean = clean_absolute(Path::new("/a/b/../c")).unwrap();
        assert_eq!(clean, PathBuf::from("/a/c"));
        // `.` components are dropped entirely
        let clean = clean_absolute(Path::new("/a/./b")).unwrap();
        assert_eq!(clean, PathBuf::from("/a/b"));
        // excess `..` is absorbed by the root rather than escaping it
        let clean = clean_absolute(Path::new("/a/../../b")).unwrap();
        assert_eq!(clean, PathBuf::from("/b"));
    }

    /// An already-clean absolute path passes through unchanged
    #[test]
    fn clean_absolute_is_idempotent() {
        // run the normalizer twice and expect a fixed point
        let once = clean_absolute(Path::new("/a/b/c")).unwrap();
        let twice = clean_absolute(&once).unwrap();
        assert_eq!(once, twice);
        assert_eq!(once, PathBuf::from("/a/b/c"));
    }
}
