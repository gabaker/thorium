//! Utility functions relating to images

use thorium::{Thorium, models::UserRole};

/// Get all groups the user is a part of or all groups if the user is an admin
///
/// # Arguments
///
/// * `thorium` - The Thorium client
#[allow(clippy::module_name_repetitions)]
pub async fn get_all_groups(thorium: &Thorium) -> Result<Vec<String>, thorium::Error> {
    // retrieve the user's info
    let user = match thorium.users.info().await {
        Ok(user) => user,
        Err(err) => {
            return Err(thorium::Error::new(format!(
                "Unable to retrieve the user's groups: {}",
                err.msg().unwrap_or("an unknown error occurred".to_string())
            )));
        }
    };
    match user.role {
        UserRole::Admin => {
            // if the user is an admin, get all groups
            let mut groups = Vec::new();
            let mut cursor = thorium.groups.list();
            loop {
                cursor.next().await.map_err(|err| {
                    thorium::Error::new(format!(
                        "Unable to retrieve the user's groups: {}",
                        err.msg().unwrap_or("an unknown error occurred".to_string())
                    ))
                })?;
                groups.append(&mut cursor.names);
                if cursor.exhausted {
                    break;
                }
            }
            Ok(groups)
        }
        _ => Ok(user.groups),
    }
}
