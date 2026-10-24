//! The loss a lossy reverse step's check estimates before anything is approved
//!
//! The catalog names the fields a step loses. The check adds how many objects hold a value to fill
//! and whether today's `thoradm backup` covers each field, so the approval message, the status, and
//! the confirmation code's loss report all say what is at stake.

use thorium::models::schema::PassCounts;
use thorium::models::upgrades::{Family, StepLoss};

use crate::backup::coverage::{Coverage, Registry, Store};

/// Whether today's backup carries a lost field
///
/// A field written `table.column` names a column of a Scylla table; a bare field is a column of the
/// table the forward step's transform rewrites, or a field of its Redis family or S3 prefix.
///
/// # Arguments
///
/// * `family` - The family the forward step rewrites, when it declares one
/// * `field` - The lost field
fn backed_up(family: Option<Family>, field: &str) -> bool {
    // a qualified field names its own table
    if let Some((table, column)) = field.split_once('.') {
        return Registry::column_backed_up(table, column);
    }
    // otherwise the family says where the field lives
    match family {
        Some(Family::ScyllaTable(table)) => Registry::column_backed_up(table, field),
        Some(Family::Redis(name)) => {
            let found = Registry::classify(Store::Redis, name)
                .or_else(|| Registry::classify(Store::Redis, &format!("{name}:x")));
            found.is_some_and(|entry| matches!(entry.coverage, Coverage::BackedUp(_)))
        }
        Some(Family::S3Prefix { bucket, .. }) => Registry::classify(Store::S3, bucket)
            .is_some_and(|entry| matches!(entry.coverage, Coverage::BackedUp(_))),
        // nothing says where the field lives, so no backup is known to carry it
        None => false,
    }
}

/// Estimate what a migration's step loses, when the step reverses a lossy one
///
/// Every object the pass would migrate holds a value of each lost field to fill, so each field's
/// object count is the pass's migratable count.
///
/// # Arguments
///
/// * `migration` - The id of the migration checked
/// * `counts` - What the check found
pub(super) fn estimate(migration: &str, counts: &PassCounts) -> Option<StepLoss> {
    // the step the migration performs must reverse a lossy step
    let catalog = super::catalog();
    let step = super::step_for(migration)?;
    let mut loss = catalog.planned_loss(step.id)?;
    // the forward step's transform says which family the fields are in
    let family = step
        .reverses
        .and_then(|forward| catalog.step(forward))
        .and_then(|(_, forward)| forward.transform)
        .map(|transform| transform.family);
    for field in &mut loss.fields {
        field.objects = counts.migratable;
        field.backed_up = backed_up(family, &field.field);
    }
    Some(loss)
}
