//! Readable, per-test-unique names for test users and groups
//!
//! Users are moons of Jupiter and groups are Roman gods, each with a short suffix shared by one
//! test, so a failure message or a stored row says who did what.

use rand::Rng;

/// Moons of Jupiter, used for test users
pub const MOONS: &[&str] = &[
    "io", "europa", "ganymede", "callisto", "amalthea", "himalia", "elara", "pasiphae", "sinope",
    "lysithea", "carme", "ananke", "leda", "thebe", "adrastea", "metis",
];

/// Roman gods, used for test groups
pub const GODS: &[&str] = &[
    "jupiter", "juno", "minerva", "mars", "venus", "apollo", "diana", "vulcan", "ceres", "mercury",
    "neptune", "vesta", "bacchus", "janus", "pluto", "saturn",
];

/// The names one test uses, sharing one suffix
#[derive(Debug, Clone)]
pub struct Names {
    /// The suffix every name of this test ends with
    pub suffix: String,
}

impl Names {
    /// Picks a fresh suffix for one test
    #[must_use]
    pub fn new() -> Self {
        // four random lowercase letters and digits
        let mut rng = rand::rng();
        let suffix = (0..4)
            .map(|_| char::from(b"abcdefghijklmnopqrstuvwxyz0123456789"[rng.random_range(0..36)]))
            .collect();
        Names { suffix }
    }

    /// Returns a group name for this test
    ///
    /// # Arguments
    ///
    /// * `god` - The god the group is named after
    #[must_use]
    pub fn group(&self, god: &str) -> String {
        // join the god and the test's suffix
        format!("{god}-{}", self.suffix)
    }

    /// Returns a user name for this test
    ///
    /// # Arguments
    ///
    /// * `moon` - The moon the user is named after
    #[must_use]
    pub fn user(&self, moon: &str) -> String {
        // join the moon and the test's suffix
        format!("{moon}-{}", self.suffix)
    }
}
