//! What a restore keeps of the target instead of taking it from the backup: the users the Thorium
//! operator provisions and logs in as, and the nodes the operator registers
//!
//! A backup of another deployment holds that deployment's operator users, whose passwords are in
//! its own Secrets and whose hashes were made with its own secret key, and the nodes of its own
//! Kubernetes clusters. Restoring them would lock the target's operator and components out and leave
//! the target's scaler without its clusters, so a restore keeps the target's where the target has
//! them, and takes the backup's only into a target that has none.

use scylla::client::session::Session;
use thorium::models::{Backup, User};

use crate::Error;

/// The users the Thorium operator provisions and logs in as: `thorium-operator`, which it creates
/// and logs in as first, and `thorium` and `thorium-kaboom`, which it creates with that user's
/// token and whose passwords it writes into the components' keys; each one's password is in the
/// deployment's `<user>-pass` Secret
pub const OPERATOR_USERS: [&str; 3] = ["thorium-operator", "thorium", "thorium-kaboom"];

/// The table of the nodes each deployment's operator registers for its own clusters
pub const NODES_TABLE: &str = "nodes";

/// Whether a username is one of the operator's users
///
/// # Arguments
///
/// * `username` - The username to check
#[must_use]
pub fn is_operator_user(username: &str) -> bool {
    // only the names the operator provisions
    OPERATOR_USERS.contains(&username)
}

/// The operator users a target holds, which a restore keeps in place of the backup's
#[derive(Debug, Clone, Default)]
pub struct KeptUsers {
    /// The target's records of the operator users it holds, password hashes and tokens included
    pub users: Vec<User>,
    /// Why the target's operator users couldn't be read, when they couldn't, so none is kept
    pub unread: Option<String>,
}

impl KeptUsers {
    /// Takes the operator users out of a target's Redis state
    ///
    /// # Arguments
    ///
    /// * `target` - The target's Redis state, as the API backs it up
    #[must_use]
    pub fn of(target: Backup) -> Self {
        // only the operator's users, in the order the target lists them
        let users = target
            .users
            .into_iter()
            .filter(|user| is_operator_user(&user.username))
            .collect();
        KeptUsers {
            users,
            unread: None,
        }
    }

    /// Puts the target's operator users into a backup in place of the backup's, returning the
    /// names of the users kept
    ///
    /// A user the backup holds too is restored from the backup with the target's credentials: its
    /// password hash, token, and token expiration, which the target's operator and components log
    /// in with. A user the backup doesn't hold is kept whole, in no group, since the backup's groups
    /// list their members. A backup's operator user the target doesn't hold is restored as it is.
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup about to be restored
    pub fn keep(&self, backup: &mut Backup) -> Vec<String> {
        // replace or add each operator user the target holds, in the order the operator creates them
        let mut kept = Vec::with_capacity(self.users.len());
        let ordered = OPERATOR_USERS
            .iter()
            .filter_map(|name| self.users.iter().find(|user| user.username == *name));
        for target in ordered {
            // the backup's record of the same user, if it holds one
            let restored = backup
                .users
                .iter_mut()
                .find(|user| user.username == target.username);
            match restored {
                // the backup's user with the target's credentials
                Some(user) => {
                    user.password.clone_from(&target.password);
                    user.token.clone_from(&target.token);
                    user.token_expiration = target.token_expiration;
                }
                // a user the backup doesn't hold is in none of its groups
                None => {
                    let mut user = target.clone();
                    user.groups = Vec::new();
                    user.actual_groups = Vec::new();
                    backup.users.push(user);
                }
            }
            kept.push(target.username.clone());
        }
        kept
    }
}

/// Whether the target's nodes table holds a row, in which case a restore keeps the target's nodes
///
/// A keyspace or nodes table that doesn't exist, as when a restore recreates a dropped keyspace,
/// holds no nodes.
///
/// # Arguments
///
/// * `scylla` - A session to the target's Scylla
/// * `keyspace` - The target's keyspace
pub async fn holds_nodes(scylla: &Session, keyspace: &str) -> Result<bool, Error> {
    // a table the schema doesn't list holds nothing, and reading it would fail
    let listed = scylla
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, NODES_TABLE),
        )
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to list {NODES_TABLE}: {error}")))?
        .rows_num();
    if listed == 0 {
        return Ok(false);
    }
    // one row is enough
    let query = format!("SELECT * FROM {keyspace}.{NODES_TABLE} LIMIT 1");
    let rows = scylla.query_unpaged(query, &[]).await?;
    let found = rows
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to read {NODES_TABLE}: {error}")))?
        .rows_num();
    Ok(found > 0)
}

#[cfg(test)]
mod tests {
    use super::{KeptUsers, OPERATOR_USERS, is_operator_user};
    use thorium::models::{Backup, User};

    /// Builds a user with a password hash, a token, groups, and an icon
    ///
    /// # Arguments
    ///
    /// * `name` - The username
    /// * `secret` - What the password hash and token are made of
    /// * `groups` - The groups the user is in
    fn user(name: &str, secret: &str, groups: &[&str]) -> User {
        // a user record as the API backs it up
        serde_json::from_value(serde_json::json!({
            "username": name,
            "password": format!("hash-{secret}"),
            "email": format!("{name}@{secret}.invalid"),
            "role": "Admin",
            "groups": groups,
            "actual_groups": groups,
            "token": format!("token-{secret}"),
            "token_expiration": "2030-01-01T00:00:00Z",
            "unix": null,
            "settings": serde_json::to_value(thorium::models::UserSettings::default())
                .expect("settings serialize"),
            "verified": true,
            "verification_token": null,
            "verification_sent": null,
            "aliases": {},
            "image": format!("icons/{name}-{secret}"),
        }))
        .expect("a valid user")
    }

    /// Builds a backup holding only users
    ///
    /// # Arguments
    ///
    /// * `users` - The users it holds
    fn backup(users: Vec<User>) -> Backup {
        // an otherwise empty Redis state
        let mut backup: Backup = serde_json::from_value(serde_json::json!({
            "settings": serde_json::to_value(thorium::models::SystemSettings::default())
                .expect("settings serialize"),
            "users": [],
            "groups": [],
            "images": [],
            "pipelines": [],
        }))
        .expect("a valid backup");
        backup.users = users;
        backup
    }

    /// The operator's users are the three it provisions and no other
    #[test]
    fn only_the_provisioned_users_are_operator_users() {
        // the three names and nothing like them
        for name in OPERATOR_USERS {
            assert!(is_operator_user(name), "{name}");
        }
        for name in ["thorium-admin", "alice", "Thorium", "thorium-operator2"] {
            assert!(!is_operator_user(name), "{name}");
        }
    }

    /// A restore keeps the target's credentials of the operator users it holds, with the rest of
    /// the backup's record, restores the backup's operator user the target lacks as it is, and
    /// leaves every other user as the backup holds it
    #[test]
    fn the_targets_operator_users_replace_the_backups() {
        // the target holds two operator users and a human, which isn't kept
        let target = backup(vec![
            user("thorium-operator", "b", &[]),
            user("thorium", "b", &["b-only"]),
            user("alice", "b", &[]),
        ]);
        let kept = KeptUsers::of(target);
        assert_eq!(kept.users.len(), 2);
        // the backup holds all three operator users and a human
        let mut restored = backup(vec![
            user("thorium-operator", "a", &["ops"]),
            user("thorium", "a", &["ops", "main"]),
            user("thorium-kaboom", "a", &["ops"]),
            user("alice", "a", &["main"]),
        ]);
        let names = kept.keep(&mut restored);
        assert_eq!(names, ["thorium-operator", "thorium"]);
        let find = |name: &str| {
            restored
                .users
                .iter()
                .filter(|user| user.username == name)
                .collect::<Vec<_>>()
        };
        // each kept user appears once, with the target's credentials and the backup's account
        for (name, groups) in [
            ("thorium-operator", vec!["ops"]),
            ("thorium", vec!["ops", "main"]),
        ] {
            let found = find(name);
            assert_eq!(found.len(), 1, "{name}");
            assert_eq!(found[0].password.as_deref(), Some("hash-b"), "{name}");
            assert_eq!(found[0].token, "token-b", "{name}");
            assert_eq!(found[0].email, format!("{name}@a.invalid"));
            assert_eq!(found[0].groups, groups, "{name}");
            assert_eq!(found[0].image, Some(format!("icons/{name}-a")), "{name}");
        }
        // the operator user the target lacks and the human come from the backup
        assert_eq!(find("thorium-kaboom")[0].token, "token-a");
        assert_eq!(find("alice")[0].token, "token-a");
        assert_eq!(restored.users.len(), 4);
    }

    /// An operator user the backup doesn't hold is kept whole, in no group
    #[test]
    fn a_user_the_backup_lacks_is_kept_in_no_group() {
        // the target's user is in a group the backup doesn't have
        let kept = KeptUsers::of(backup(vec![user("thorium-kaboom", "b", &["b-only"])]));
        let mut restored = backup(Vec::new());
        assert_eq!(kept.keep(&mut restored), ["thorium-kaboom"]);
        // it keeps its credentials and icon, in no group
        let user = &restored.users[0];
        assert_eq!(user.token, "token-b");
        assert!(user.groups.is_empty() && user.actual_groups.is_empty());
        assert_eq!(user.image.as_deref(), Some("icons/thorium-kaboom-b"));
    }
}
