//! Docker-style adjective-animal name generation for auto-named sessions.
//!
//! A session the operator did not name with `--name` gets a random
//! `<adjective>-<animal>` directory name (e.g. `brave-otter`) so it is easy to
//! reference and resume. [`crate::thorpal::store::SessionStore::create`] claims
//! the name by creating its directory and retries on a collision, appending a
//! random numeric suffix once the plain combinations keep colliding.

use rand::Rng;

/// The adjectives an auto-generated session name can open with.
const ADJECTIVES: &[&str] = &[
    "agile",
    "amber",
    "ancient",
    "bold",
    "brave",
    "bright",
    "calm",
    "clever",
    "cosmic",
    "crafty",
    "crimson",
    "curious",
    "daring",
    "dusty",
    "eager",
    "electric",
    "elegant",
    "fearless",
    "fierce",
    "frosty",
    "gentle",
    "gilded",
    "golden",
    "happy",
    "hidden",
    "humble",
    "iron",
    "jolly",
    "keen",
    "lively",
    "lucky",
    "lunar",
    "mellow",
    "mighty",
    "nimble",
    "noble",
    "patient",
    "proud",
    "quiet",
    "rapid",
    "restless",
    "rustic",
    "shadowy",
    "silent",
    "silver",
    "sly",
    "solar",
    "stealthy",
    "stormy",
    "swift",
    "vivid",
    "wandering",
    "wild",
    "wise",
    "zealous",
];

/// The animals an auto-generated session name can end with.
const ANIMALS: &[&str] = &[
    "badger", "bat", "bear", "beaver", "bison", "cheetah", "cobra", "condor", "coyote", "crane",
    "crow", "dingo", "dolphin", "eagle", "falcon", "ferret", "fox", "gecko", "hawk", "heron",
    "hound", "ibex", "jackal", "jaguar", "koala", "lemur", "lynx", "mantis", "marmot", "mole",
    "moose", "narwhal", "ocelot", "octopus", "orca", "osprey", "otter", "owl", "panda", "panther",
    "penguin", "puma", "python", "rabbit", "raccoon", "raven", "salmon", "shark", "sparrow",
    "stoat", "tiger", "toucan", "viper", "walrus", "wolf", "wombat",
];

/// Generates a random docker-style `<adjective>-<animal>` session name
pub(super) fn random_name() -> String {
    // draw a fresh adjective and animal from the thread-local generator
    let mut rng = rand::rng();
    let adjective = ADJECTIVES[rng.random_range(0..ADJECTIVES.len())];
    let animal = ANIMALS[rng.random_range(0..ANIMALS.len())];
    format!("{adjective}-{animal}")
}

/// Generates a random two-digit suffix used to break stubborn name collisions
pub(super) fn random_suffix() -> String {
    // two digits keeps the suffix short while adding 100 variants per name
    format!("{:02}", rand::rng().random_range(0..100u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generated names are always two lowercase words joined by a hyphen.
    #[test]
    fn random_names_are_well_formed() {
        // sample a batch of names so a bad list entry cannot hide
        for _ in 0..100 {
            let name = random_name();
            // the name splits into exactly an adjective and an animal
            let parts: Vec<&str> = name.split('-').collect();
            assert_eq!(parts.len(), 2, "name {name:?} should be adjective-animal");
            // both halves stay lowercase ascii so the name is file and shell safe
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "name {name:?} should be lowercase ascii"
            );
        }
    }

    /// Collision suffixes are always exactly two digits.
    #[test]
    fn random_suffixes_are_two_digits() {
        // sample a batch of suffixes to cover the range
        for _ in 0..100 {
            let suffix = random_suffix();
            assert_eq!(suffix.len(), 2);
            assert!(suffix.chars().all(|c| c.is_ascii_digit()));
        }
    }
}
