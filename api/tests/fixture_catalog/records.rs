//! The fixture objects of each matrix family in each layout, and the helpers that register,
//! seed, and verify them in a scenario

use thorium::models::upgrades::{Family, Record};
use thorium::test_utilities::upgrade_harness::{
    Check, FamilyFixture, FixtureSeed, FixtureVerifier, Layout, Scenario, Variation,
};

use super::catalog::MATRIX;
use super::transforms::{
    ACL, FILES, FILES_NEW, FILES_OLD, ITEMS, OWNERS, PEOPLE, RANKS, TAGS, TASKS, TOKENS,
};

/// Build a Redis hash record of a family
///
/// # Arguments
///
/// * `family` - The family's name, which the key starts with
/// * `index` - The object's number
/// * `pairs` - The fields it holds
fn hash(family: &str, index: usize, pairs: &[(&str, String)]) -> Record {
    // the key is the family and the number
    Record::Redis {
        key: format!("{family}:{index}"),
        fields: pairs
            .iter()
            .map(|(field, value)| ((*field).to_owned(), value.clone()))
            .collect(),
    }
}

/// Read the fields of a record, or none for a record that isn't a Redis hash
///
/// # Arguments
///
/// * `record` - The record
pub fn fields(record: &Record) -> Option<&std::collections::BTreeMap<String, String>> {
    // only Redis hashes hold fields
    match record {
        Record::Redis { fields, .. } => Some(fields),
        _ => None,
    }
}

/// The label of an item
///
/// # Arguments
///
/// * `index` - The item's number
pub fn label(index: usize) -> String {
    // mixed case, so the lowercased key differs from it
    format!("Item-{index}")
}

/// The full name of a person: every third person has one name only
///
/// # Arguments
///
/// * `index` - The person's number
pub fn full_name(index: usize) -> String {
    // a single name has no space to split at
    if index % 3 == 0 {
        format!("Cher{index}")
    } else {
        format!("First{index} Last{index}")
    }
}

/// Build an item in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The item's number
/// * `_variation` - Unused: the items declare no optional fields
fn item(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // each layout adds a field, and layout 4 drops `label`
    let label = label(index);
    let mut pairs = Vec::new();
    if layout.0 <= 3 {
        pairs.push(("label", label.clone()));
    }
    if layout.0 >= 2 {
        pairs.push(("display_name", label.clone()));
    }
    if layout.0 >= 3 {
        pairs.push(("display_key", label.to_lowercase()));
    }
    hash("matrix_items", index, &pairs)
}

/// Find the layout an item is in
///
/// # Arguments
///
/// * `record` - The item
fn item_layout(record: &Record) -> Option<Layout> {
    // the newest field decides, and layout 4 has no label
    let fields = fields(record)?;
    match (
        fields.contains_key("display_key"),
        fields.contains_key("label"),
        fields.contains_key("display_name"),
    ) {
        (true, false, _) => Some(Layout(4)),
        (true, true, _) => Some(Layout(3)),
        (false, true, true) => Some(Layout(2)),
        (false, true, false) => Some(Layout(1)),
        _ => None,
    }
}

/// Build a person in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The person's number
/// * `_variation` - Unused: people declare no optional fields
fn person(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 1 has one field, layout 2 two, layout 3 one again under a new name
    let full = full_name(index);
    let (first, last) = full.split_once(' ').unwrap_or((full.as_str(), ""));
    match layout.0 {
        1 => hash("matrix_people", index, &[("full_name", full.clone())]),
        2 => hash(
            "matrix_people",
            index,
            &[("first", first.to_owned()), ("last", last.to_owned())],
        ),
        _ => hash("matrix_people", index, &[("name", full.clone())]),
    }
}

/// Find the layout a person is in
///
/// # Arguments
///
/// * `record` - The person
fn person_layout(record: &Record) -> Option<Layout> {
    // the newest field decides
    let fields = fields(record)?;
    if fields.contains_key("name") {
        Some(Layout(3))
    } else if fields.contains_key("first") {
        Some(Layout(2))
    } else if fields.contains_key("full_name") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build a task in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The task's number
/// * `_variation` - Unused: tasks declare no optional fields
fn task(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 written by the new code holds the text only
    let number = (index * 7 % 10).to_string();
    if layout.0 == 1 {
        hash("matrix_tasks", index, &[("priority", number)])
    } else {
        hash("matrix_tasks", index, &[("priority_text", number)])
    }
}

/// Find the layout a task is in
///
/// # Arguments
///
/// * `record` - The task
fn task_layout(record: &Record) -> Option<Layout> {
    // the text is layout 2, and the number alone layout 1
    let fields = fields(record)?;
    if fields.contains_key("priority_text") {
        Some(Layout(2))
    } else if fields.contains_key("priority") {
        Some(Layout(1))
    } else {
        None
    }
}

/// The owner of an owners record
///
/// # Arguments
///
/// * `index` - The record's number
pub fn owner(index: usize) -> String {
    // capitalized, so the bucket differs from the first letter
    format!("Owner{index}")
}

/// Build an owners record in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The record's number
/// * `_variation` - Unused: owners declare no optional fields
fn owners(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 adds the bucket
    let owner = owner(index);
    let mut pairs = vec![("owner", owner)];
    if layout.0 >= 2 {
        pairs.push(("bucket", "o".to_owned()));
    }
    hash("matrix_owners", index, &pairs)
}

/// Find the layout an owners record is in
///
/// # Arguments
///
/// * `record` - The record
fn owners_layout(record: &Record) -> Option<Layout> {
    // the bucket decides
    let fields = fields(record)?;
    if fields.contains_key("bucket") {
        Some(Layout(2))
    } else if fields.contains_key("owner") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build a tag in a layout: from layout 3 on, every fourth tag holds the new `archived` status
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The tag's number
/// * `_variation` - Unused: tags declare no optional fields
fn tag(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // the new status exists only in layout 3
    let text = format!(" Tag-{index} ");
    let status = if layout.0 >= 3 && index % 4 == 3 {
        "archived"
    } else if index % 2 == 0 {
        "active"
    } else {
        "inactive"
    };
    let mut pairs = vec![("tag", text.clone()), ("status", status.to_owned())];
    if layout.0 >= 2 {
        pairs.push(("normalized", text.trim().to_lowercase()));
    }
    if layout.0 >= 3 {
        pairs.push(("retention", "keep".to_owned()));
    }
    hash("matrix_tags", index, &pairs)
}

/// Find the layout a tag is in
///
/// # Arguments
///
/// * `record` - The tag
fn tag_layout(record: &Record) -> Option<Layout> {
    // the newest field decides
    let fields = fields(record)?;
    if fields.contains_key("retention") {
        Some(Layout(3))
    } else if fields.contains_key("normalized") {
        Some(Layout(2))
    } else if fields.contains_key("tag") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build an access record in a layout: even records are public
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The record's number
/// * `_variation` - Unused: access records declare no optional fields
fn acl(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 replaces the flag with who is allowed
    let public = index % 2 == 0;
    if layout.0 == 1 {
        hash("matrix_acl", index, &[("public", public.to_string())])
    } else {
        let allowed = if public { "everyone" } else { "owner" };
        hash("matrix_acl", index, &[("allowed", allowed.to_owned())])
    }
}

/// Find the layout an access record is in
///
/// # Arguments
///
/// * `record` - The record
fn acl_layout(record: &Record) -> Option<Layout> {
    // the new field decides
    let fields = fields(record)?;
    if fields.contains_key("allowed") {
        Some(Layout(2))
    } else if fields.contains_key("public") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build a token in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The token's number
/// * `_variation` - Unused: tokens declare no optional fields
fn token(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 adds the derived identifier
    let token = format!("tok{index}");
    if layout.0 == 1 {
        hash("matrix_tokens", index, &[("token", token)])
    } else {
        hash(
            "matrix_tokens",
            index,
            &[("ident", format!("id-{token}")), ("token", token)],
        )
    }
}

/// Find the layout a token is in
///
/// # Arguments
///
/// * `record` - The token
fn token_layout(record: &Record) -> Option<Layout> {
    // the identifier decides
    let fields = fields(record)?;
    if fields.contains_key("ident") {
        Some(Layout(2))
    } else if fields.contains_key("token") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build a ranks record in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The record's number
/// * `_variation` - Unused: ranks declare no optional fields
fn rank(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 adds the reference to a tag
    let entry = format!(" Rank-{index} ");
    if layout.0 == 1 {
        hash("matrix_ranks", index, &[("entry", entry)])
    } else {
        hash(
            "matrix_ranks",
            index,
            &[("tag_ref", entry.trim().to_lowercase()), ("entry", entry)],
        )
    }
}

/// Find the layout a ranks record is in
///
/// # Arguments
///
/// * `record` - The record
fn rank_layout(record: &Record) -> Option<Layout> {
    // the reference decides
    let fields = fields(record)?;
    if fields.contains_key("tag_ref") {
        Some(Layout(2))
    } else if fields.contains_key("entry") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Build a file in a layout
///
/// # Arguments
///
/// * `layout` - The layout
/// * `index` - The file's number
/// * `_variation` - Unused: files declare no optional fields
fn file(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 2 files sit under the new prefix
    let prefix = if layout.0 == 1 { FILES_OLD } else { FILES_NEW };
    Record::S3 {
        key: format!("{prefix}{index}"),
    }
}

/// Find the layout a file is in
///
/// # Arguments
///
/// * `record` - The file
fn file_layout(record: &Record) -> Option<Layout> {
    // the key's prefix decides
    match record {
        Record::S3 { key } if key.starts_with(FILES_NEW) => Some(Layout(2)),
        Record::S3 { key } if key.starts_with(FILES_OLD) => Some(Layout(1)),
        _ => None,
    }
}

/// Builds the object with a number in a layout
type Generate = fn(Layout, usize, &Variation) -> Record;

/// Finds the layout an object is in
type LayoutOf = fn(&Record) -> Option<Layout>;

/// Every family of the matrix
#[must_use]
pub fn all_families() -> [Family; 9] {
    // in the order the catalog introduces them
    [
        ITEMS, PEOPLE, TASKS, OWNERS, FILES, TAGS, RANKS, ACL, TOKENS,
    ]
}

/// How a matrix family's objects are built and read
///
/// # Arguments
///
/// * `family` - The family
fn fixture(family: &Family) -> FamilyFixture {
    // each family has its own layouts
    let (generate, layout_of): (Generate, LayoutOf) = match *family {
        f if f == ITEMS => (item, item_layout),
        f if f == PEOPLE => (person, person_layout),
        f if f == TASKS => (task, task_layout),
        f if f == OWNERS => (owners, owners_layout),
        f if f == TAGS => (tag, tag_layout),
        f if f == RANKS => (rank, rank_layout),
        f if f == FILES => (file, file_layout),
        f if f == ACL => (acl, acl_layout),
        _ => (token, token_layout),
    };
    FamilyFixture {
        generate,
        layout_of,
        corrupt: None,
    }
}

/// Register the fixture of every matrix family in a scenario
///
/// # Arguments
///
/// * `scenario` - The scenario
#[must_use]
pub fn register(scenario: Scenario) -> Scenario {
    // one registration per family
    all_families().iter().fold(scenario, |scenario, family| {
        scenario.family(*family, fixture(family))
    })
}

/// Start a scenario on the pattern matrix with every family registered
///
/// # Arguments
///
/// * `name` - The scenario's name
#[must_use]
pub fn scenario<T: Into<String>>(name: T) -> Scenario {
    // plan from the matrix catalog
    register(Scenario::new(name).catalog(MATRIX))
}

/// Seed objects of a family in a layout
///
/// # Arguments
///
/// * `family` - The family
/// * `layout` - The layout the objects are written in
/// * `count` - How many to write
#[must_use]
pub fn seed(family: Family, layout: u32, count: usize) -> FixtureSeed {
    // no optional-field variants
    FixtureSeed {
        family,
        layout: Layout(layout),
        count,
        variants: None,
    }
}

/// Expect every object of a family in a layout
///
/// # Arguments
///
/// * `family` - The family
/// * `layout` - The layout every object must be in
#[must_use]
pub fn in_layout(family: Family, layout: u32) -> Check {
    // a verifier with nothing else expected
    Check::Fixtures(FixtureVerifier::items_in(family, Layout(layout)))
}
