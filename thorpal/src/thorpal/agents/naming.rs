//! Per-invocation display names for sub-agents, drawn from the moons of Jupiter.
//!
//! Every time the main agent delegates to a sub-agent, that invocation is given a
//! unique, human-readable display name like `ReverseEngineer(Europa)`. The name
//! is used as the sub-agent's forum author, its tracing-log identity, and its
//! token-usage ledger key, so two invocations of the same role can be told apart.
//! Names are handed out round-robin from [`MOONS`]; once every moon has been used
//! the list wraps and a `(n)` suffix is appended to keep each name unique.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The named moons of Jupiter, in order of handing them out.
///
/// Only officially *named* moons are listed -- the roman-numeral-only
/// designations (e.g. `Jupiter LI`, `S/2003 J 2`) are intentionally excluded so
/// every display name reads as a proper name.
const MOONS: &[&str] = &[
    "Io",
    "Europa",
    "Ganymede",
    "Callisto",
    "Amalthea",
    "Himalia",
    "Elara",
    "Pasiphae",
    "Sinope",
    "Lysithea",
    "Carme",
    "Ananke",
    "Leda",
    "Thebe",
    "Adrastea",
    "Metis",
    "Callirrhoe",
    "Themisto",
    "Megaclite",
    "Taygete",
    "Chaldene",
    "Harpalyke",
    "Kalyke",
    "Iocaste",
    "Erinome",
    "Isonoe",
    "Praxidike",
    "Autonoe",
    "Thyone",
    "Hermippe",
    "Aitne",
    "Eurydome",
    "Euanthe",
    "Euporie",
    "Orthosie",
    "Sponde",
    "Kale",
    "Pasithee",
    "Hegemone",
    "Mneme",
    "Aoede",
    "Thelxinoe",
    "Arche",
    "Kallichore",
    "Helike",
    "Carpo",
    "Eukelade",
    "Cyllene",
    "Kore",
    "Herse",
    "Dia",
    "Eirene",
    "Philophrosyne",
    "Eupheme",
    "Valetudo",
    "Pandia",
    "Ersa",
];

/// The process-global counter of how many display names have been handed out.
///
/// Shared across every sub-agent invocation so each call takes the next moon,
/// wrapping (with a `(n)` suffix) once the whole list has been used.
static NEXT_MOON: AtomicUsize = AtomicUsize::new(0);

/// Builds the display name for the `n`-th sub-agent invocation.
///
/// This is the pure core of [`next_display_name`], split out so it can be tested
/// without touching the shared [`NEXT_MOON`] counter. The moons are handed out
/// round-robin; after the list is exhausted it wraps and a `(n)` suffix (starting
/// at `(2)`) is appended so names stay unique across passes.
///
/// # Arguments
///
/// * `role` - The human-facing role prefix (e.g. `"ReverseEngineer"`)
/// * `n` - The zero-based sequence number of this invocation
fn display_name_for(role: &str, n: usize) -> String {
    // index into the moon list, and which pass over the list we are on
    let idx = n % MOONS.len();
    let pass = n / MOONS.len();
    // on the first pass the bare moon name is unique; later passes disambiguate
    // with a (n) suffix starting at (2) so the wrapped names never collide
    let moon = if pass == 0 {
        MOONS[idx].to_string()
    } else {
        format!("{} ({})", MOONS[idx], pass + 1)
    };
    // join the role prefix and the moon into the final display name
    format!("{role}({moon})")
}

/// Builds the next unique display name for a sub-agent invocation.
///
/// Called once per delegation from the tool wrapper, so repeated invocations of
/// the same role receive distinct names (`ReverseEngineer(Europa)`,
/// `ReverseEngineer(Io)`, ...).
///
/// # Arguments
///
/// * `role` - The human-facing role prefix (e.g. `"ReverseEngineer"`)
///
/// # Examples
///
/// ```text
/// let name = next_display_name("ReverseEngineer");
/// // e.g. "ReverseEngineer(Europa)"
/// ```
pub fn next_display_name(role: &str) -> String {
    // take the next global slot, then format it into a display name
    let n = NEXT_MOON.fetch_add(1, Ordering::Relaxed);
    display_name_for(role, n)
}

/// The number of display names handed out so far, for session snapshots.
///
/// Persisted so a resumed session can seed the counter past its prior run's
/// names instead of re-minting them (which silently clobbered the prior run's
/// `agents/<name>.json` records and merged distinct invocations' usage keys).
pub fn name_counter() -> usize {
    // read the shared counter without disturbing it
    NEXT_MOON.load(Ordering::Relaxed)
}

/// The lowest counter seed that puts every given display name in the past.
///
/// The snapshot's persisted tally is only as fresh as the last settled turn;
/// a process killed mid-turn minted names past it, visible only as the saved
/// `agents/*.json` record names. Flooring the resume's seed past every such
/// name keeps fresh mints from colliding with (and clobbering) them. Names
/// that do not parse as minted display names contribute nothing.
///
/// # Arguments
///
/// * `names` - The display names the saved records used
pub fn counter_floor<'a>(names: impl Iterator<Item = &'a str>) -> usize {
    // a name minted from slot n forces the counter past n
    names
        .filter_map(minted_index)
        .map(|n| n.saturating_add(1))
        .max()
        .unwrap_or(0)
}

/// Recovers the counter slot a display name was minted from, if parseable
///
/// Inverts [`display_name_for`]: `Role(Moon)` came from the moon's list slot
/// on pass zero, and `Role(Moon (p))` from pass `p - 1`. A side-conversation
/// scope prefix (`btw:name/`) is stripped first. `None` for anything that is
/// not a minted display name.
///
/// # Arguments
///
/// * `name` - The display name to invert
fn minted_index(name: &str) -> Option<usize> {
    // strip any side-conversation scope prefix off the display name
    let name = name.rsplit('/').next().unwrap_or(name);
    // the moon rides inside the role's parens: Role(Moon) or Role(Moon (p))
    if !name.ends_with(')') {
        return None;
    }
    let open = name.find('(')?;
    let inner = name.get(open + 1..name.len() - 1)?;
    // split a pass suffix "Moon (p)" off the bare moon name
    let (moon, pass) = match inner.rfind(" (") {
        Some(split) if inner.ends_with(')') => {
            let digits = inner.get(split + 2..inner.len() - 1)?;
            match digits.parse::<usize>() {
                // display passes start at (2); anything else is not a suffix
                Ok(pass) if pass >= 2 => (&inner[..split], pass - 1),
                _ => (inner, 0),
            }
        }
        _ => (inner, 0),
    };
    // find the moon's slot in the list and add the completed passes
    let idx = MOONS.iter().position(|candidate| *candidate == moon)?;
    Some(pass * MOONS.len() + idx)
}

/// Seeds the display-name counter at least past a resumed session's tally.
///
/// Monotonic (`fetch_max`), so concurrent sessions in one process can only
/// skip names, never collide on them.
///
/// # Arguments
///
/// * `n` - The resumed session's persisted name counter
pub fn seed_counter(n: usize) {
    // only ever move the counter forward
    NEXT_MOON.fetch_max(n, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeding the counter is monotonic: it can only ever skip forward
    #[test]
    fn seed_counter_is_monotonic() {
        // push the shared counter well past any concurrent test's usage
        seed_counter(100_000);
        assert!(name_counter() >= 100_000);
        // seeding backwards never rewinds already-handed-out names
        seed_counter(50);
        assert!(name_counter() >= 100_000);
    }

    /// The first full pass hands out bare, distinct moon names in list order.
    #[test]
    fn first_pass_names_are_suffix_free_and_distinct() {
        // the whole first pass over the list must be suffix-free and unique
        let names: Vec<String> = (0..MOONS.len())
            .map(|n| display_name_for("Tester", n))
            .collect();
        // no first-pass name carries a (n) pass suffix; every name now wraps its
        // moon in parens, so the disambiguator shows up as a space-paren marker
        assert!(
            names.iter().all(|name| !name.contains(" (")),
            "first-pass names should have no pass suffix"
        );
        // every first-pass name is distinct
        let mut deduped = names.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            names.len(),
            "first-pass names must be unique"
        );
    }

    /// Once the list wraps, the same moon reappears with a `(2)` suffix.
    #[test]
    fn wrapping_appends_pass_suffix() {
        // the very next name after a full pass reuses the first moon with (2)
        let wrapped = display_name_for("Role", MOONS.len());
        assert_eq!(wrapped, format!("Role({} (2))", MOONS[0]));
    }

    /// The floor inverts minted names, scope prefixes and wraps included
    #[test]
    fn counter_floor_inverts_minted_names() {
        // every minted slot inverts back to one past itself
        for n in [0, 1, MOONS.len() - 1, MOONS.len(), MOONS.len() * 2 + 3] {
            let name = display_name_for("Prober", n);
            assert_eq!(counter_floor([name.as_str()].into_iter()), n + 1);
        }
        // a scope prefix strips off before the parse
        let scoped = format!("btw:crypto/{}", display_name_for("Swarm", 7));
        assert_eq!(counter_floor([scoped.as_str()].into_iter()), 8);
        // the floor is the max across every record name
        let names = [display_name_for("A", 2), display_name_for("B", 9)];
        assert_eq!(counter_floor(names.iter().map(String::as_str)), 10);
        // unparseable names contribute nothing
        assert_eq!(counter_floor(["main", "Role(NotAMoon)"].into_iter()), 0);
    }
}
