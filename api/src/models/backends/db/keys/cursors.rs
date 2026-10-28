//! Build the keys to cursors in redis

use uuid::Uuid;

use crate::models::backends::db::cursors::CursorKind;
use crate::utils::Shared;

/// The layout version of every cursor's saved state, so a cursor saved in an older layout is
/// never read and its id is reported as not found
pub const LAYOUT_VERSION: u32 = 2;

/// Build the key to this cursors data
///
/// The key nests the cursor id under its layout version and the creating user's username, so
/// cursors are scoped to the user that created them and a cursor saved in an older layout is
/// never found
///
/// # Arguments
///
/// * `kind` - The kind of cursor this key is for
/// * `user` - The username of the user this cursor is scoped to
/// * `id` - The id of the cursor this key is for
/// * `shared` - Shared Thorium objects
pub fn data(kind: CursorKind, user: &str, id: &Uuid, shared: &Shared) -> String {
    // scope this cursor to its creating user by nesting the id under the username
    format!(
        "{ns}:cursors:{kind}:v{LAYOUT_VERSION}:{user}:{id}",
        kind = kind.as_str(),
        ns = shared.config.thorium.namespace
    )
}
