//! Utility functions for resolving entity targets given as ids or names

use chrono::SecondsFormat;
use thorium::models::{Entity, EntityKinds};
use thorium::{Error, Thorium};
use uuid::Uuid;

/// Build the error message for a name shared by several entities
///
/// Matches are listed in the order the server returned them (oldest created
/// first) so the user can pick the right id to re-run with.
///
/// # Arguments
///
/// * `noun` - The user facing noun for the kind of entity being resolved
/// * `target` - The ambiguous name the user gave
/// * `matches` - The entities sharing this name in the server's order
fn ambiguous_name_error(noun: &str, target: &str, matches: &[Entity]) -> String {
    // start with a summary line telling the user to re-run with an id
    let mut msg = format!("Multiple {noun}s are named '{target}'; re-run with one of these ids:");
    // list each match's id, creation time, and groups on its own line
    for entity in matches {
        msg.push_str(&format!(
            "\n  {}  created {}  groups [{}]",
            entity.id,
            entity.created.to_rfc3339_opts(SecondsFormat::Secs, true),
            entity.groups.join(",")
        ));
    }
    msg
}

/// Parse a target as an entity id in the canonical hyphenated uuid form
///
/// `Uuid::parse_str` also accepts the simple 32-hex, braced, and urn forms,
/// but a 32-hex string is a legal (and common, hash style) entity name, so
/// only the canonical hyphenated form Thorium prints is treated as an id;
/// everything else resolves as a name.
///
/// # Arguments
///
/// * `target` - The target to parse as an entity id
pub fn parse_id_target(target: &str) -> Option<Uuid> {
    // only the 36 char hyphenated form is unambiguously an id
    if target.len() != 36 {
        return None;
    }
    // parse the canonical form into an id
    Uuid::try_parse(target).ok()
}

/// Resolve an entity target given as either an id or an exact name
///
/// Targets in the canonical hyphenated uuid form are always treated as ids,
/// so a name that is itself a hyphenated uuid must be addressed by its id
/// (other uuid spellings like bare 32-hex resolve as names). Name targets
/// resolve server side by exact name and must match exactly one entity the
/// user can see; ambiguous names error with the candidate ids.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the entity to resolve
/// * `kinds` - The entity kinds this target is allowed to resolve to
/// * `noun` - The user facing noun for error messages (e.g. "rule")
pub async fn resolve_entity(
    thorium: &Thorium,
    target: &str,
    kinds: &[EntityKinds],
    noun: &str,
) -> Result<Entity, Error> {
    // targets in canonical hyphenated uuid form are addressed by id, full stop
    if let Some(id) = parse_id_target(target) {
        // get this entity with a friendlier message when it doesn't exist
        let entity = match thorium.entities.get(id).await {
            Ok(entity) => entity,
            Err(error) => {
                return Err(match error.status().map(|status| status.as_u16()) {
                    Some(404) => Error::new(format!("No {noun} found with id {id}")),
                    _ => error,
                });
            }
        };
        // make sure this entity is one of the kinds we are resolving
        if !kinds.contains(&entity.kind) {
            return Err(Error::new(format!(
                "Entity {id} is a {} entity, not a {noun}",
                entity.kind
            )));
        }
        return Ok(entity);
    }
    // this target is a name so look it up server side by exact name
    let mut matches = match thorium.entities.get_by_name(target, kinds).await {
        Ok(matches) => matches,
        Err(error) => {
            return Err(match error.status().map(|status| status.as_u16()) {
                // surface the server's 400 reason verbatim (e.g. the shared name row guard)
                Some(400) => Error::new(
                    super::api_error_text(&error)
                        .unwrap_or_else(|| format!("Failed to resolve {noun} '{target}'")),
                ),
                _ => error,
            });
        }
    };
    // this name must match exactly one entity we can see
    match matches.len() {
        // nothing we can see carries this name
        0 => Err(Error::new(format!("No {noun} named '{target}' was found"))),
        // exactly one entity carries this name
        1 => Ok(matches.remove(0)),
        // several entities share this name so the user must pick an id
        _ => Err(Error::new(ambiguous_name_error(noun, target, &matches))),
    }
}

/// Resolve an entity target to its id, passing ids through without a fetch
///
/// Create/update style commands hand the server many targets it validates
/// again anyway, so an id target costs no extra round trip here (matching the
/// old raw-uuid behavior); only name targets resolve server side.
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `target` - The id or name of the entity to resolve
/// * `kinds` - The entity kinds this target is allowed to resolve to
/// * `noun` - The user facing noun for error messages (e.g. "rule")
pub async fn resolve_entity_id(
    thorium: &Thorium,
    target: &str,
    kinds: &[EntityKinds],
    noun: &str,
) -> Result<Uuid, Error> {
    // ids pass through untouched; only names need a resolution round trip
    match parse_id_target(target) {
        Some(id) => Ok(id),
        None => Ok(resolve_entity(thorium, target, kinds, noun).await?.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use thorium::models::EntityMetadata;

    /// Build a bare entity for formatting tests
    ///
    /// # Arguments
    ///
    /// * `id` - The id for this test entity
    /// * `created` - The creation timestamp for this test entity
    /// * `groups` - The groups for this test entity
    fn entity(id: &str, created: i64, groups: &[&str]) -> Entity {
        Entity {
            id: Uuid::parse_str(id).expect("test entity id was not a uuid"),
            name: "demo_rule".to_owned(),
            kind: EntityKinds::YaraRule,
            metadata: EntityMetadata::Other,
            description: None,
            submitter: "mcarson".to_owned(),
            groups: groups.iter().map(ToString::to_string).collect(),
            tags: HashMap::default(),
            image: None,
            created: DateTime::<Utc>::from_timestamp(created, 0)
                .expect("timestamp was out of range"),
        }
    }

    /// The ambiguity message must list each match's id, created, and groups
    #[test]
    fn ambiguous_name_error_lists_matches() {
        // build two matches sharing a name in the server's created order
        let matches = vec![
            entity(
                "bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a",
                1_760_000_000,
                &["demolab", "corn"],
            ),
            entity(
                "0a632e3a-3c90-45a8-9b2f-6f6dc44f8af0",
                1_760_000_060,
                &["beans"],
            ),
        ];
        // build the ambiguity message for these matches
        let msg = ambiguous_name_error("rule", "demo_rule", &matches);
        // the message must open with the re-run summary line
        let mut lines = msg.lines();
        assert_eq!(
            lines.next(),
            Some("Multiple rules are named 'demo_rule'; re-run with one of these ids:")
        );
        // each match must land on its own line in the server's order
        assert_eq!(
            lines.next(),
            Some(
                "  bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a  created 2025-10-09T08:53:20Z  \
                groups [demolab,corn]"
            )
        );
        assert_eq!(
            lines.next(),
            Some(
                "  0a632e3a-3c90-45a8-9b2f-6f6dc44f8af0  created 2025-10-09T08:54:20Z  \
                groups [beans]"
            )
        );
        // nothing may follow the final match line
        assert_eq!(lines.next(), None);
    }

    /// Only the canonical hyphenated uuid form may be treated as an id
    #[test]
    fn parse_id_target_requires_hyphenated_form() {
        // the canonical hyphenated form parses as an id
        assert!(parse_id_target("bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a").is_some());
        // a bare 32-hex string is a legal hash style entity name, not an id
        assert!(parse_id_target("deadbeefdeadbeefdeadbeefdeadbeef").is_none());
        // braced and urn uuid spellings resolve as names too
        assert!(parse_id_target("{bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a}").is_none());
        assert!(parse_id_target("urn:uuid:bf5b4ea2-7a43-4a69-8c41-1f3b2c6e5d8a").is_none());
        // ordinary rule names are never ids
        assert!(parse_id_target("demo_keylogger_strings").is_none());
    }
}
