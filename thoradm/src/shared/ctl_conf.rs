//! Loading the Thorctl config thoradm uses to reach the Thorium API

use std::path::Path;
use thorium::CtlConf;

use crate::Error;

/// Load the Thorctl config thoradm reaches the Thorium API with
///
/// A missing or unreadable config fails as a usage error that names the path, says what the
/// config is for, and how to create one or point thoradm at another, instead of a bare I/O error
/// from the client.
///
/// # Arguments
///
/// * `path` - The path to the Thorctl config (`--ctl-conf`)
pub fn load(path: &Path) -> Result<CtlConf, Error> {
    // a config that exists and parses is used as is
    if path.is_file() {
        return Ok(CtlConf::from_path(path)?);
    }
    // explain what is missing and how to fix it
    Err(Error::Usage(format!(
        "no Thorctl config at '{}'. thoradm reaches the Thorium API with a Thorctl config for \
         an admin user (the API holds the Redis data a backup or restore reads). Run \
         `thorctl login <thorium url>` as an admin on this host to create ~/.thorium/config.yml, \
         or pass `--ctl-conf <path>` before the subcommand (for example \
         `thoradm --cluster-conf thorium.yml --ctl-conf <path> backup new ...`). Under sudo, ~ \
         is root's home, so pass the full path to your config",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::load;
    use crate::Error;

    /// A missing Thorctl config fails as a usage error naming the path and both fixes
    #[test]
    fn missing_config_names_the_path_and_the_fixes() {
        // point at a path under a directory that doesn't exist, so no config is there
        let path = std::env::temp_dir()
            .join(format!("thoradm-no-ctl-conf-{}", uuid::Uuid::new_v4()))
            .join("config.yml");
        // loading it must fail with a usage error
        let Err(Error::Usage(message)) = load(&path) else {
            panic!("a missing config must fail as a usage error");
        };
        // the message names the path and both ways to fix it
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("thorctl login"), "{message}");
        assert!(message.contains("--ctl-conf"), "{message}");
    }
}
