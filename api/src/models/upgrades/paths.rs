//! The table of valid upgrade paths the admin docs show, rendered from the catalog's floors

use std::collections::BTreeSet;
use std::fmt::Write;

use super::{Catalog, CatalogEntry, SupportFloor};

/// The header of the upgrade-paths table
const HEADER: &str = "\
| Thorium version | Revision | Oldest revision it upgrades from | From an older revision |
|-----------------|----------|----------------------------------|------------------------|
";

impl Catalog {
    /// Render the table of valid upgrade paths between support floors as Markdown
    ///
    /// The table has one row per Thorium version at the newest release revision released with
    /// it, naming the oldest revision that version upgrades from and the chain of intermediate
    /// releases a deployment below it installs first, and a row for pre-Helm deployments of any
    /// version. The admin upgrade page holds it between its `upgrade-paths` markers; regenerate
    /// it there with `THORIUM_BLESS=1 cargo test -p thorium-api --lib models::upgrades::paths`.
    ///
    /// # Arguments
    ///
    /// * `pre_helm` - The revision `convert-to-helm.sh` records for a converted deployment
    #[must_use]
    pub fn upgrade_paths(&self, pre_helm: &str) -> String {
        // the release revisions this catalog holds, newest first
        let mut releases = self.releases().collect::<Vec<_>>();
        releases.sort_by(|left, right| right.id.cmp(left.id));
        let newest = releases.first().map_or("", |edge| edge.id);
        // without a floor in effect a release plans from the catalog's oldest revision
        let oldest = releases.last().map_or(pre_helm, |edge| edge.id);
        // add one row per Thorium version, at the newest revision released with it
        let mut table = HEADER.to_owned();
        let mut versions = BTreeSet::new();
        for edge in releases
            .iter()
            .filter(|edge| versions.insert(edge.released_with))
        {
            // the oldest revision this version upgrades from is its newest floor's minimum
            let floors = self.floors_at(edge.id, newest);
            let minimum = floors.last().map_or(oldest, |floor| floor.minimum);
            // writing to a string can't fail
            let _ = writeln!(
                table,
                "| {} | `{}` | `{minimum}` | {} |",
                edge.released_with,
                edge.id,
                chain(floors)
            );
        }
        // a pre-Helm deployment converts with the release whose catalog plans from its baseline
        let version = releases.first().map_or("", |edge| edge.released_with);
        let floors = self.floors_at(newest, newest);
        let convert = match floors.iter().position(|floor| pre_helm < floor.minimum) {
            Some(index) => {
                // convert with the oldest intermediate above the baseline, then install the rest
                let mut installs = floors[index..]
                    .iter()
                    .map(|floor| floor.intermediate)
                    .collect::<Vec<_>>();
                installs.push(version);
                format!(
                    "Convert with the `convert-to-helm.sh` of Thorium {}, then install {}, \
                     letting each finish upgrading",
                    installs[0],
                    in_turn(&installs[1..])
                )
            }
            // a baseline at or above every floor converts with this release
            None => format!(
                "Convert with the `convert-to-helm.sh` of Thorium {version}, then upgrade from \
                 `{pre_helm}`"
            ),
        };
        // writing to a string can't fail
        let _ = writeln!(
            table,
            "| Pre-Helm, any version | `{pre_helm}` once converted | — | {convert} |"
        );
        table
    }

    /// Iterate over the release edges of this catalog: forward edges that aren't pre-releases
    fn releases(&self) -> impl Iterator<Item = &'static CatalogEntry> {
        // skip feature branches' edges and edges that lead down
        self.edges
            .iter()
            .filter(|edge| !edge.prerelease && !edge.is_downward())
    }

    /// Get the floors in effect for the release that shipped a revision, oldest first
    ///
    /// A floor is in effect from the release that raised it: the newest release revision released
    /// with its `since_version`. A last floor without such a revision was raised by the newest
    /// release, and an older one by a release already squashed, so it is in effect for every
    /// release the catalog holds.
    ///
    /// # Arguments
    ///
    /// * `revision` - The release revision to get the floors of
    /// * `newest` - The newest release revision in this catalog
    fn floors_at(&self, revision: &str, newest: &str) -> &'static [SupportFloor] {
        // count the floors, oldest first, raised at or before the revision
        let count = self
            .floors
            .iter()
            .enumerate()
            .take_while(|(index, floor)| {
                // find the release that raised this floor
                let raised_by = self
                    .releases()
                    .filter(|edge| edge.released_with == floor.since_version)
                    .map(|edge| edge.id)
                    .max();
                match raised_by {
                    Some(raised_by) => raised_by <= revision,
                    None if index + 1 == self.floors.len() => newest <= revision,
                    None => true,
                }
            })
            .count();
        &self.floors[..count]
    }
}

/// Describe what a deployment below each floor in effect installs first, the newest range first
///
/// # Arguments
///
/// * `floors` - The floors in effect, oldest first
fn chain(floors: &[SupportFloor]) -> String {
    // a release without a floor upgrades from every revision it knows
    if floors.is_empty() {
        return "—".to_owned();
    }
    // a revision below a floor installs that floor's intermediate and every newer one, in order
    (0..floors.len())
        .rev()
        .map(|index| {
            // name the range of revisions below this floor and above the one before it
            let range = match index.checked_sub(1) {
                Some(previous) => format!(
                    "From `{}` to before `{}`",
                    floors[previous].minimum, floors[index].minimum
                ),
                None => format!("Below `{}`", floors[index].minimum),
            };
            // install this floor's intermediate and every newer one, one after another
            let installs = floors[index..]
                .iter()
                .map(|floor| floor.intermediate)
                .collect::<Vec<_>>();
            match installs.as_slice() {
                [only] => {
                    format!("{range}: install Thorium {only} first and let it finish upgrading")
                }
                _ => format!(
                    "{range}: install {}, letting each finish upgrading",
                    in_turn(&installs)
                ),
            }
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

/// List Thorium releases to install one after another
///
/// # Arguments
///
/// * `versions` - The releases' versions, in install order
fn in_turn(versions: &[&str]) -> String {
    // name each release, joined in install order
    versions
        .iter()
        .map(|version| format!("Thorium {version}"))
        .collect::<Vec<_>>()
        .join(", then ")
}

#[cfg(test)]
mod tests {
    use super::super::{Catalog, SupportFloor, fixtures};

    /// The admin upgrade page holding the table, relative to the api crate
    const PAGE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/src/admins/deploy/upgrades.md"
    );

    /// The line before the table on the admin upgrade page
    const BEGIN: &str =
        "<!-- upgrade-paths:begin (generated from the catalog's support floors) -->\n";

    /// The line after the table on the admin upgrade page
    const END: &str = "<!-- upgrade-paths:end -->\n";

    /// Get the revision the pre-Helm conversion script records
    fn pre_helm_revision() -> String {
        // read the script's baseline the same way it is set
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deploy/charts/scripts/convert-to-helm.sh"
        );
        let script = std::fs::read_to_string(path).expect("read convert-to-helm.sh");
        script
            .lines()
            .find_map(|line| line.strip_prefix("BASELINE_REVISION="))
            .expect("BASELINE_REVISION is set in the script")
            .trim_matches('"')
            .to_owned()
    }

    /// Get the text between the markers of a page, with the byte range it spans
    ///
    /// # Arguments
    ///
    /// * `page` - The page's text
    fn block(page: &str) -> (std::ops::Range<usize>, &str) {
        // the table starts after the begin marker and ends before the end marker
        let start = page.find(BEGIN).expect("the page has the begin marker") + BEGIN.len();
        let end = start
            + page[start..]
                .find(END)
                .expect("the page has the end marker after the begin marker");
        (start..end, &page[start..end])
    }

    /// The upgrade paths between the page's markers equal the table rendered from the production
    /// catalog
    ///
    /// After changing the catalog's floors or releases, regenerate the table with
    /// `THORIUM_BLESS=1 cargo test -p thorium-api --lib models::upgrades::paths`.
    #[test]
    fn docs_table_matches_the_catalog() {
        // render the production table for deployments converted by this release's script
        let rendered = Catalog::PRODUCTION.upgrade_paths(&pre_helm_revision());
        let page = std::fs::read_to_string(PAGE).expect("read the upgrade page");
        let (range, current) = block(&page);
        // rewrite the block on request instead of comparing
        if std::env::var("THORIUM_BLESS").is_ok_and(|value| value == "1") {
            let mut blessed = page.clone();
            blessed.replace_range(range, &rendered);
            std::fs::write(PAGE, blessed).expect("write the upgrade page");
            return;
        }
        assert_eq!(
            current, rendered,
            "the upgrade paths in {PAGE} differ from the catalog; rerun with THORIUM_BLESS=1 to \
             regenerate them"
        );
    }

    /// Raising a floor without regenerating the docs leaves the page's table different from the
    /// rendered one
    #[test]
    fn raised_floor_differs_from_the_docs() {
        // a production catalog whose floor was raised to its newest revision
        let raised = Catalog {
            floors: &[SupportFloor {
                minimum: "2026-10-v02",
                intermediate: "1.8.0",
                since_version: "1.8.1",
            }],
            ..Catalog::PRODUCTION
        };
        let rendered = raised.upgrade_paths(&pre_helm_revision());
        // the new floor and its intermediate release appear in the rendered table
        assert!(rendered.contains("Thorium 1.8.0"), "{rendered}");
        // and the page, rendered from the catalog as it is, no longer matches
        let page = std::fs::read_to_string(PAGE).expect("read the upgrade page");
        assert_ne!(block(&page).1, rendered);
    }

    /// Without floors the table states the one path from the baseline and the pre-Helm row
    #[test]
    fn table_without_floors_starts_at_the_baseline() {
        // a catalog with one release edge after its baseline and no floor
        let rendered = fixtures::ONE_EDGE.upgrade_paths(fixtures::BASE);
        assert_eq!(
            rendered,
            "\
| Thorium version | Revision | Oldest revision it upgrades from | From an older revision |
|-----------------|----------|----------------------------------|------------------------|
| 0.0.0 | `2026-10-v02` | `2026-10-v01` | — |
| Pre-Helm, any version | `2026-10-v01` once converted | — | Convert with the `convert-to-helm.sh` of Thorium 0.0.0, then upgrade from `2026-10-v01` |
"
        );
    }

    /// With floors each version names its oldest revision and the chain of intermediate releases
    /// below it, and the pre-Helm row names the release whose script to convert with
    #[test]
    fn table_with_floors_names_each_chain() {
        // a catalog whose floor was raised twice
        let rendered = fixtures::FLOORED.upgrade_paths(fixtures::BASE);
        assert_eq!(
            rendered,
            "\
| Thorium version | Revision | Oldest revision it upgrades from | From an older revision |
|-----------------|----------|----------------------------------|------------------------|
| 2.0.0 | `2028-01-v01` | `2027-01-v01` | From `2026-01-v01` to before `2027-01-v01`: install Thorium 1.9.0 first and let it finish upgrading<br>Below `2026-01-v01`: install Thorium 1.5.0, then Thorium 1.9.0, letting each finish upgrading |
| 0.0.0 | `2027-10-v01` | `2026-01-v01` | Below `2026-01-v01`: install Thorium 1.5.0 first and let it finish upgrading |
| Pre-Helm, any version | `2026-10-v01` once converted | — | Convert with the `convert-to-helm.sh` of Thorium 1.9.0, then install Thorium 2.0.0, letting each finish upgrading |
"
        );
        // a pre-Helm baseline below both floors converts with the oldest intermediate release
        let oldest = fixtures::FLOORED.upgrade_paths("2025-06-v01");
        assert!(
            oldest.contains(
                "| Convert with the `convert-to-helm.sh` of Thorium 1.5.0, then install Thorium \
                 1.9.0, then Thorium 2.0.0, letting each finish upgrading |"
            ),
            "{oldest}"
        );
    }
}
