//! Handles config commands

use std::{collections::HashSet, path::PathBuf};

use thorium::{CtlConf, Error, client::conf::GitSettings};

use crate::args::{
    Args,
    config::{Config, ConfigOpts},
};

/// Update the ThorPal settings in the `ai` block of the Thorctl configuration
///
/// # Arguments
///
/// * `config` - The config to modify
/// * `opts` - The optional updates to the Thorctl config
fn update_ai_config(config: &mut CtlConf, opts: &ConfigOpts) -> Result<(), Error> {
    // skip the ai block entirely if no ai settings were passed
    let any_set = opts.ai_provider.is_some()
        || opts.ghidra_install.is_some()
        || opts.clear_ghidra_install
        || opts.ghidra_projects.is_some()
        || opts.ai_workflow.is_some()
        || opts.clear_ai_workflow
        || opts.report_template.is_some()
        || opts.clear_report_template;
    if !any_set {
        return Ok(());
    }
    // the ThorPal settings live in the ai block, so it must already exist
    let Some(ai) = config.ai.as_mut() else {
        return Err(Error::new(
            "No `ai` config block set: add an `ai` block with a provider section \
            such as `openai`, `litellm`, `anthropic`, or `bedrock` to your Thorctl \
            config first",
        ));
    };
    // switch the active AI provider, validating that its block exists
    if let Some(provider) = opts.ai_provider {
        ai.provider = Some(provider);
        // refuse to write a selector that points at a missing block
        ai.resolve()?;
    }
    // set or clear the Ghidra install
    if opts.clear_ghidra_install {
        ai.ghidra_install = None;
    } else if let Some(ghidra_install) = &opts.ghidra_install {
        ai.ghidra_install = Some(ghidra_install.clone());
    }
    // set the Ghidra projects directory
    if let Some(ghidra_projects) = &opts.ghidra_projects {
        ai.ghidra_projects = Some(ghidra_projects.clone());
    }
    // set or clear the workflow
    if opts.clear_ai_workflow {
        ai.workflow = None;
    } else if let Some(workflow) = &opts.ai_workflow {
        ai.workflow = Some(workflow.clone());
    }
    // set or clear the report template
    if opts.clear_report_template {
        ai.report_template = None;
    } else if let Some(report_template) = &opts.report_template {
        ai.report_template = Some(report_template.clone());
    }
    Ok(())
}

/// Update the Thorctl configuration, returning the modified [`CtlConf`]
///
/// # Arguments
///
/// * `config` - The config to modify
/// * `cmd` - The optional updates to the Thorctl config
fn update_config(mut config: CtlConf, opts: &ConfigOpts) -> Result<CtlConf, Error> {
    // set opts if they were set
    if let Some(keys) = &opts.git_ssh_keys {
        config.git = Some(GitSettings::new(keys));
    }
    if let Some(invalid_certs) = opts.invalid_certs {
        config.client.invalid_certs = invalid_certs;
    }
    if let Some(invalid_hostnames) = opts.invalid_hostnames {
        config.client.invalid_hostnames = invalid_hostnames;
    }
    if opts.clear_certificate_authorities {
        config.client.certificate_authorities.clear();
    } else {
        let mut cert_set: HashSet<PathBuf> =
            HashSet::from_iter(config.client.certificate_authorities.clone());
        // append any new certificate authorities
        cert_set.extend(opts.certificate_authorities.clone());
        // remove certificate authorities
        for cert in &opts.remove_certificate_authorities {
            cert_set.remove(cert);
        }
        config.client.certificate_authorities = cert_set.into_iter().collect();
    }
    if let Some(timeout) = opts.timeout {
        config.client.timeout = timeout;
    }
    if let Some(disable_proxy) = opts.disable_proxy {
        config.client.disable_proxy = disable_proxy;
    }
    if opts.clear_proxy {
        config.client.proxy = None;
    } else if let Some(proxy) = &opts.proxy {
        config.client.proxy = Some(proxy.clone());
    }
    if opts.clear_no_proxy {
        config.client.no_proxy = None;
    } else if let Some(no_proxy) = &opts.no_proxy {
        config.client.no_proxy = Some(no_proxy.join(","));
    }
    if let Some(skip_insecure_warning) = opts.skip_insecure_warning {
        config.skip_insecure_warning = Some(skip_insecure_warning);
    }
    if let Some(skip_update) = opts.skip_update {
        config.skip_update = Some(skip_update);
    }
    if let Some(default_editor) = &opts.default_editor {
        config.default_editor.clone_from(default_editor);
    }
    // this config is about to be written back out, so migrate any legacy flat
    // ai keys into a nested `litellm` block first (loading never rewrites them)
    if let Some(ai) = config.ai.as_mut() {
        ai.migrate_legacy();
    }
    // update the ThorPal settings in the ai block
    update_ai_config(&mut config, opts)?;
    Ok(config)
}

/// Modify the Thorctl configuration file given by `--config`
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `cmd` - The config command that was run
pub fn config(args: &Args, cmd: &Config) -> Result<(), Error> {
    // deserialize the Thorctl configuration file
    let Ok(thorctl_conf) = CtlConf::from_path(&args.config) else {
        return Err(Error::new(format!(
            "Missing or invalid config file at '{}': first login \
            using 'thorctl login' to generate a valid base configuration file",
            args.config.to_string_lossy()
        )));
    };
    // update the Thorctl config
    let new_conf = update_config(thorctl_conf, &cmd.config_opts)?;
    // write the new configuration file
    let conf_file = std::fs::File::create(&args.config)?;
    serde_norway::to_writer(conf_file, &new_conf)?;
    Ok(())
}
