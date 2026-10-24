//! Tests of the users migration's writes: one transaction per user holding the two fields the
//! conversion changes

use super::users_theme::write_commands;
use crate::models::upgrades::user_hash;

/// The commands of a write, upper cased so they can be searched for
///
/// # Arguments
///
/// * `pipe` - The pipeline to render
fn packed(pipe: &redis::Pipeline) -> String {
    // the wire form holds the command names and arguments as text
    String::from_utf8_lossy(&pipe.get_packed_pipeline()).to_uppercase()
}

/// A converted user is written in one transaction that sets `settings` and removes the flat
/// `theme`, and a user reverted to layout 1 sets the flat field and drops an emptied `settings`
#[test]
fn a_user_is_written_in_one_transaction() {
    // up: settings set, flat theme removed
    let converted = user_hash("alice", &[("settings", r#"{"theme":"Dark"}"#)]);
    let up = packed(&write_commands(&converted, "ns:user_data:alice"));
    assert!(up.contains("MULTI") && up.contains("EXEC"), "{up}");
    assert!(up.contains("HSET") && up.contains("SETTINGS"), "{up}");
    assert!(up.contains("HDEL") && up.contains("THEME"), "{up}");
    assert!(up.contains("NS:USER_DATA:ALICE"), "{up}");
    // down: flat theme set, settings removed
    let reverted = user_hash("alice", &[("theme", "\"Dark\"")]);
    let down = packed(&write_commands(&reverted, "ns:user_data:alice"));
    assert!(down.contains("MULTI") && down.contains("EXEC"), "{down}");
    assert!(down.contains("HSET") && down.contains("\"DARK\""), "{down}");
    assert!(down.contains("HDEL") && down.contains("SETTINGS"), "{down}");
}
