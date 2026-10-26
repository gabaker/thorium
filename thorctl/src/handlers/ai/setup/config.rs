//! Loading, merging, and atomically writing the config the setup wizard edits
//!
//! The wizard drafts its answers into a [`SetupPlan`] and nothing touches the
//! config file until the review step confirms; [`apply`] then merges the plan
//! over the loaded config (or a fresh one on a first-run box), preserving every
//! field the wizard does not own, and [`write_atomic`] lands the result with a
//! temp-file-plus-rename swap so a crash mid-write can never truncate the only
//! copy of a user's token.

use std::path::{Path, PathBuf};

use thorium::client::conf::{AiProviderKind, ResolvedAiProvider};
use thorium::{CtlConf, Error, Keys};

/// The wizard's drafted answers, applied to the config at the review step
#[derive(Debug, Clone, Default)]
pub struct SetupPlan {
    /// The Thorium keys a successful in-wizard login produced
    pub keys: Option<Keys>,
    /// The provider settings the credentials form drafted
    pub provider: Option<ResolvedAiProvider>,
    /// The Ghidra install the user chose or downloaded
    pub ghidra_install: Option<PathBuf>,
    /// The Ghidra projects directory the user chose
    pub ghidra_projects: Option<PathBuf>,
}

/// Placeholder keys written when a fresh-box user skips the login step
///
/// They keep the config loadable for the AI commands while making it obvious
/// (an empty api url) that `thorctl login` still has to run for anything that
/// talks to Thorium itself.
fn placeholder_keys() -> Keys {
    Keys {
        api: String::new(),
        username: None,
        password: None,
        token: None,
    }
}

/// Load the config the wizard starts from, never failing the wizard itself
///
/// A missing file is the fresh-box path (the login step turns on); a file that
/// exists but will not parse is reported so the welcome and review pages can
/// warn that saving will overwrite it. Note `CtlConf::from_path` overlays
/// `THORCTL__*` env vars, so overlaid values would persist on save -- the same
/// behavior `thorctl config` already has.
///
/// # Arguments
///
/// * `path` - The config path the wizard reads and later writes
pub fn load_state(path: &Path) -> (Option<CtlConf>, Option<String>) {
    match CtlConf::from_path(path) {
        // a parsed config seeds the wizard's prefills
        Ok(conf) => (Some(conf), None),
        // a missing file is simply the fresh-box path, not an error
        Err(_) if !path.exists() => (None, None),
        // an unparseable file is surfaced so saving over it is a loud choice
        Err(error) => (None, Some(error.to_string())),
    }
}

/// Merge the wizard's plan over the loaded config into the config to write
///
/// Everything the wizard does not own passes through untouched: the client
/// settings, git keys, editor, every other `ai` field (workflow, templates,
/// script/memory dirs, yara rules), and the untouched provider blocks. Legacy
/// flat provider keys migrate first, exactly like `thorctl config` does before
/// a write.
///
/// # Arguments
///
/// * `plan` - The wizard's drafted answers
/// * `existing` - The config loaded at startup, when one parsed
/// * `default_projects` - The shared default Ghidra projects dir, so defaults
///   are never pinned into the file
pub fn apply(
    plan: &SetupPlan,
    existing: Option<CtlConf>,
    default_projects: &Path,
) -> Result<CtlConf, Error> {
    // start from the loaded config, or a fresh one on a first-run box
    let mut conf = match existing {
        Some(conf) => conf,
        None => CtlConf::new(plan.keys.clone().unwrap_or_else(placeholder_keys)),
    };
    // a successful in-wizard login replaces the keys either way
    if let Some(keys) = &plan.keys {
        conf.keys = keys.clone();
    }
    // take the ai block (or start an empty one) and modernize legacy keys
    // before merging, mirroring `thorctl config`'s pre-write migration
    let mut ai = conf.ai.take().unwrap_or_default();
    ai.migrate_legacy();
    // migrate_legacy leaves the flat trio in place when a litellm block
    // already exists; resolve() never reads the flat keys again in that
    // case, so drop them rather than persist a stale (possibly rotated-away)
    // secret back to disk
    if ai.litellm.is_some() {
        ai.endpoint = None;
        ai.api_key = None;
        ai.model = None;
    }
    // land the drafted provider settings in their block and select them
    if let Some(provider) = &plan.provider {
        match provider.clone() {
            ResolvedAiProvider::OpenAi(mut settings) => {
                // keep the old block's pricing overrides across a re-setup
                if settings.pricing.is_none() {
                    settings.pricing = ai.openai.as_ref().and_then(|old| old.pricing);
                }
                ai.openai = Some(settings);
                ai.provider = Some(AiProviderKind::OpenAi);
            }
            ResolvedAiProvider::LiteLlm(mut settings) => {
                // keep the old block's pricing overrides across a re-setup
                if settings.pricing.is_none() {
                    settings.pricing = ai.litellm.as_ref().and_then(|old| old.pricing);
                }
                ai.litellm = Some(settings);
                ai.provider = Some(AiProviderKind::LiteLlm);
            }
            ResolvedAiProvider::Anthropic(mut settings) => {
                // keep the old block's pricing overrides across a re-setup
                if settings.pricing.is_none() {
                    settings.pricing = ai.anthropic.as_ref().and_then(|old| old.pricing);
                }
                ai.anthropic = Some(settings);
                ai.provider = Some(AiProviderKind::Anthropic);
            }
            ResolvedAiProvider::Bedrock(mut settings) => {
                // keep the old block's pricing overrides across a re-setup
                if settings.pricing.is_none() {
                    settings.pricing = ai.bedrock.as_ref().and_then(|old| old.pricing);
                }
                ai.bedrock = Some(settings);
                ai.provider = Some(AiProviderKind::Bedrock);
            }
        }
    }
    // land the chosen Ghidra install; a skipped step leaves the old value
    if let Some(install) = &plan.ghidra_install {
        ai.ghidra_install = Some(install.clone());
    }
    // pin the projects dir only when it was already pinned or diverges from
    // the shared default, so the default never gets frozen into the file
    if let Some(projects) = &plan.ghidra_projects {
        if ai.ghidra_projects.is_some() || projects.as_path() != default_projects {
            ai.ghidra_projects = Some(projects.clone());
        }
    }
    // refuse to write a provider selection that cannot resolve; a block-less
    // ai section (a ghidra-only setup) is fine and skips the check
    let has_provider = ai.provider.is_some()
        || ai.openai.is_some()
        || ai.litellm.is_some()
        || ai.anthropic.is_some()
        || ai.bedrock.is_some();
    if has_provider {
        ai.resolve()?;
    }
    // hand the merged config back for review/write
    conf.ai = Some(ai);
    Ok(conf)
}

/// Atomically write the merged config, tightening permissions on the way
///
/// This deliberately diverges from the `File::create` precedent in login and
/// `thorctl config`: the file may hold the user's only copy of a fresh token,
/// so the yaml lands in a same-directory temp file (mode 0600 on unix) that is
/// renamed over the target only once fully written and synced.
///
/// # Arguments
///
/// * `path` - The config path to write
/// * `conf` - The merged config to serialize
pub fn write_atomic(path: &Path, conf: &CtlConf) -> Result<(), Error> {
    // write through a symlinked config (dotfile managers) by resolving the
    // real target first, so the rename replaces the file itself rather than
    // the link and the old secrets never linger at a stranded link target
    let resolved = resolve_destination(path);
    let path = resolved.as_path();
    // make sure the config's directory exists (fresh boxes have no ~/.thorium)
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)?;
    // the temp file sits beside the target so the rename stays on one filesystem
    let name = path.file_name().map_or_else(
        || "config.yml".to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    let temp = parent.join(format!(".{name}.tmp"));
    // create the temp file owner-read/write only; the yaml carries secrets
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&temp)?;
    // serialize the whole config into the temp file
    serde_norway::to_writer(&file, conf)?;
    // force the bytes down before the rename makes the file the real config
    file.sync_all()?;
    // the atomic swap: a crash before this line leaves the old config intact
    std::fs::rename(&temp, path)?;
    Ok(())
}

/// The real filesystem destination a config write should land at
///
/// A symlinked config file resolves to its target; a config that does not
/// exist yet resolves its parent (which may itself be a symlinked
/// `~/.thorium`) and keeps the file name. When nothing resolves, the path is
/// used as given.
///
/// # Arguments
///
/// * `path` - The config path as the user named it
fn resolve_destination(path: &Path) -> PathBuf {
    // an existing file (or link chain) resolves directly
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    // a missing file cannot canonicalize; resolve the directory instead and
    // re-join the file name so a symlinked parent still lands correctly
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && !parent.as_os_str().is_empty()
        && let Ok(parent) = std::fs::canonicalize(parent)
    {
        return parent.join(name);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use thorium::client::conf::{AISettings, AiPricing, AnthropicSettings, LiteLlmSettings};

    /// A config with keys, an ai block carrying a litellm provider, and the
    /// ThorPal paths the wizard must never clobber
    fn seeded_conf() -> CtlConf {
        // start from a plain tokened config
        let mut conf = CtlConf::new(Keys::new_token("http://thorium.example", "token"));
        // seed an ai block with a provider and unrelated ThorPal settings
        let mut ai = AISettings::default();
        ai.litellm = Some(LiteLlmSettings {
            endpoint: "http://gateway:4000".to_string(),
            api_key: Some("old-key".to_string()),
            model: "alias".to_string(),
            pricing: None,
        });
        ai.workflow = Some(PathBuf::from("/workflows/custom.yaml"));
        ai.scripts_dir = Some(PathBuf::from("/scripts"));
        ai.ghidra_projects = Some(PathBuf::from("/data/projects"));
        conf.ai = Some(ai);
        conf
    }

    /// The anthropic settings a credentials form would draft
    fn drafted_anthropic() -> ResolvedAiProvider {
        ResolvedAiProvider::Anthropic(AnthropicSettings {
            endpoint: None,
            api_key: "sk-new".to_string(),
            model: "claude-sonnet-5-5".to_string(),
            pricing: None,
        })
    }

    /// Switching providers preserves every field the wizard does not own
    #[test]
    fn apply_preserves_unrelated_fields() {
        // draft a provider switch plus a ghidra install over the seeded config
        let plan = SetupPlan {
            keys: None,
            provider: Some(drafted_anthropic()),
            ghidra_install: Some(PathBuf::from("/opt/ghidra")),
            ghidra_projects: None,
        };
        let merged = apply(&plan, Some(seeded_conf()), Path::new("/default")).unwrap();
        // the keys and the untouched ai fields all passed through
        assert_eq!(merged.keys.api, "http://thorium.example");
        let ai = merged.ai.as_ref().unwrap();
        assert_eq!(ai.workflow, Some(PathBuf::from("/workflows/custom.yaml")));
        assert_eq!(ai.scripts_dir, Some(PathBuf::from("/scripts")));
        assert_eq!(ai.ghidra_projects, Some(PathBuf::from("/data/projects")));
        // the old provider block survives alongside the new selection
        assert!(ai.litellm.is_some());
        assert_eq!(ai.provider, Some(AiProviderKind::Anthropic));
        assert_eq!(ai.anthropic.as_ref().unwrap().api_key, "sk-new");
        assert_eq!(ai.ghidra_install, Some(PathBuf::from("/opt/ghidra")));
        // the explicit selector keeps the two-block config resolvable
        assert!(ai.resolve().is_ok());
    }

    /// Legacy flat keys migrate into a litellm block before the merge
    #[test]
    fn apply_migrates_legacy_flat_keys() {
        // a pre-provider-blocks config carries the flat trio
        let mut conf = CtlConf::new(Keys::new_token("http://thorium.example", "token"));
        let mut ai = AISettings::default();
        ai.endpoint = Some("http://old-gateway".to_string());
        ai.api_key = Some("legacy-key".to_string());
        ai.model = Some("legacy-model".to_string());
        conf.ai = Some(ai);
        // a ghidra-only plan still triggers the pre-write migration
        let plan = SetupPlan {
            ghidra_install: Some(PathBuf::from("/opt/ghidra")),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, Some(conf), Path::new("/default")).unwrap();
        let ai = merged.ai.as_ref().unwrap();
        // the flat keys moved into a selected litellm block
        assert!(ai.endpoint.is_none() && ai.api_key.is_none() && ai.model.is_none());
        assert_eq!(ai.provider, Some(AiProviderKind::LiteLlm));
        assert_eq!(ai.litellm.as_ref().unwrap().model, "legacy-model");
    }

    /// Re-drafting the same provider keeps its old pricing overrides
    #[test]
    fn apply_keeps_pricing_across_a_re_setup() {
        // the existing litellm block carries pricing the form never edits
        let mut conf = seeded_conf();
        conf.ai.as_mut().unwrap().litellm.as_mut().unwrap().pricing = Some(AiPricing {
            input: Some(3.0),
            output: Some(15.0),
            cache_read: None,
            cache_write: None,
        });
        // the form re-drafts the litellm block with a fresh key
        let plan = SetupPlan {
            provider: Some(ResolvedAiProvider::LiteLlm(LiteLlmSettings {
                endpoint: "http://gateway:4000".to_string(),
                api_key: Some("new-key".to_string()),
                model: "alias".to_string(),
                pricing: None,
            })),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, Some(conf), Path::new("/default")).unwrap();
        let litellm = merged.ai.as_ref().unwrap().litellm.as_ref().unwrap();
        // the new key landed and the old pricing survived
        assert_eq!(litellm.api_key.as_deref(), Some("new-key"));
        assert_eq!(litellm.pricing.unwrap().input, Some(3.0));
    }

    /// A fresh box with a skipped login still writes a loadable config
    #[test]
    fn apply_fresh_box_with_skipped_login_uses_placeholder_keys() {
        // no existing config and no login keys, just a drafted provider
        let plan = SetupPlan {
            provider: Some(drafted_anthropic()),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, None, Path::new("/default")).unwrap();
        // the placeholder keys make the thorium side obviously unconfigured
        assert_eq!(merged.keys.api, "");
        assert!(merged.keys.token.is_none());
        // while the ai side resolves to the drafted provider
        assert!(merged.ai.as_ref().unwrap().resolve().is_ok());
    }

    /// The shared projects default is never pinned into the file
    #[test]
    fn apply_skips_pinning_the_default_projects_dir() {
        // a fresh config with the projects box left on the shared default
        let plan = SetupPlan {
            provider: Some(drafted_anthropic()),
            ghidra_projects: Some(PathBuf::from("/home/user/ghidra-projects")),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, None, Path::new("/home/user/ghidra-projects")).unwrap();
        // the default stayed implicit
        assert!(merged.ai.as_ref().unwrap().ghidra_projects.is_none());
        // while a diverging choice gets pinned
        let plan = SetupPlan {
            provider: Some(drafted_anthropic()),
            ghidra_projects: Some(PathBuf::from("/data/projects")),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, None, Path::new("/home/user/ghidra-projects")).unwrap();
        assert_eq!(
            merged.ai.as_ref().unwrap().ghidra_projects,
            Some(PathBuf::from("/data/projects"))
        );
    }

    /// The atomic write round-trips through the real loader with tight perms
    #[test]
    fn write_atomic_round_trips_and_tightens_permissions() {
        // merge a fresh config and land it in a temp home
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yml");
        let plan = SetupPlan {
            keys: Some(Keys::new_token("http://thorium.example", "token")),
            provider: Some(drafted_anthropic()),
            ghidra_install: Some(PathBuf::from("/opt/ghidra")),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, None, Path::new("/default")).unwrap();
        write_atomic(&path, &merged).unwrap();
        // the real loader reads back exactly what the wizard wrote
        let reloaded = CtlConf::from_path(&path).unwrap();
        assert_eq!(reloaded.keys.api, "http://thorium.example");
        let ai = reloaded.ai.as_ref().unwrap();
        assert_eq!(ai.provider, Some(AiProviderKind::Anthropic));
        assert_eq!(ai.ghidra_install, Some(PathBuf::from("/opt/ghidra")));
        // no temp file lingers after the swap
        assert!(!dir.path().join("nested").join(".config.yml.tmp").exists());
        // the secrets file is owner-only on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the config is owner-read/write only");
        }
    }

    /// A selector naming a missing block is refused before anything writes
    #[test]
    fn apply_refuses_an_unresolvable_provider_selection() {
        // force an impossible state: a selector with no matching block
        let mut conf = CtlConf::new(Keys::new_token("http://thorium.example", "token"));
        let mut ai = AISettings::default();
        ai.provider = Some(AiProviderKind::Bedrock);
        conf.ai = Some(ai);
        // a ghidra-only plan cannot fix the selection, so apply refuses
        let plan = SetupPlan {
            ghidra_install: Some(PathBuf::from("/opt/ghidra")),
            ..SetupPlan::default()
        };
        assert!(apply(&plan, Some(conf), Path::new("/default")).is_err());
    }

    /// Legacy flat keys shadowed by a litellm block are dropped, not persisted
    #[test]
    fn apply_drops_shadowed_legacy_flat_keys() {
        // a partially-upgraded config carries BOTH the flat trio and a litellm
        // block; migrate_legacy leaves the flat trio alone in that state
        let mut conf = seeded_conf();
        let ai = conf.ai.as_mut().unwrap();
        ai.endpoint = Some("http://old-gateway".to_string());
        ai.api_key = Some("rotated-away-key".to_string());
        ai.model = Some("old-model".to_string());
        // the user re-drafts the litellm block to rotate the leaked key
        let plan = SetupPlan {
            provider: Some(ResolvedAiProvider::LiteLlm(LiteLlmSettings {
                endpoint: "http://gateway:4000".to_string(),
                api_key: Some("fresh-key".to_string()),
                model: "alias".to_string(),
                pricing: None,
            })),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, Some(conf), Path::new("/default")).unwrap();
        let ai = merged.ai.as_ref().unwrap();
        // the stale flat secret is gone and only the fresh block remains
        assert!(ai.endpoint.is_none() && ai.api_key.is_none() && ai.model.is_none());
        assert_eq!(
            ai.litellm.as_ref().unwrap().api_key.as_deref(),
            Some("fresh-key")
        );
    }

    /// A symlinked config is written through, never replaced by a plain file
    #[test]
    #[cfg(unix)]
    fn write_atomic_writes_through_a_symlinked_config() {
        // a dotfile-manager layout: the real file lives in a store dir and
        // ~/.thorium/config.yml is a symlink to it
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        std::fs::create_dir_all(&store).unwrap();
        let real = store.join("thorctl-config.yml");
        std::fs::write(&real, "keys:\n  api: http://old\n").unwrap();
        let link = dir.path().join("config.yml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // save a merged config through the symlink
        let plan = SetupPlan {
            keys: Some(Keys::new_token("http://thorium.example", "token")),
            provider: Some(drafted_anthropic()),
            ..SetupPlan::default()
        };
        let merged = apply(&plan, None, Path::new("/default")).unwrap();
        write_atomic(&link, &merged).unwrap();
        // the link survived and the real file carries the new content
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let reloaded = CtlConf::from_path(&real).unwrap();
        assert_eq!(reloaded.keys.api, "http://thorium.example");
    }
}
