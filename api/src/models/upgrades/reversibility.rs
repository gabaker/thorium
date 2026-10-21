//! How a step is undone on a downgrade, and the fills a lossy reverse writes

#[cfg(feature = "scylla-utils")]
use super::Record;

/// How a step is undone on a downgrade
#[derive(Debug, Clone, Copy)]
pub enum Reversibility {
    /// The revision writes both forms, so the downgrade needs no data pass
    DualWrite {
        /// The contract revision that removes the dual write
        until: &'static str,
    },
    /// A reverse pass derives the old form from the new for every object
    Backfill,
    /// A reverse pass derives what it can and fills the rest; runs only with the loss accepted
    LossyRevert {
        /// A fill for every field the reverse can't derive
        fills: &'static [Fill],
        /// The fields whose values are lost, checked against the snapshot diff
        lost_fields: &'static [&'static str],
        /// The old variant each variant added in this revision maps to; required for every new
        /// variant
        enum_mappings: &'static [EnumMapping],
        /// The grants of every variant of each security-sensitive role field this step maps or
        /// fills, which the fill lint compares
        privilege_orders: &'static [PrivilegeOrder],
        /// What the downgrade loses, for the approval message and release notes
        description: &'static str,
    },
    /// The downgrade is refused; restore a backup instead
    Irreversible {
        /// Why no fill can be valid, checked in review
        justification: &'static str,
    },
}

/// The value a lossy reverse writes for one field it can't derive
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    /// The column or field filled
    pub field: &'static str,
    /// What is written
    pub value: FillValue,
}

/// What a fill writes
#[derive(Debug, Clone, Copy)]
pub enum FillValue {
    /// A constant, in the record's text form (a Redis field value or a CQL literal)
    Constant(&'static str),
    /// A value derived from the new record
    #[cfg(feature = "scylla-utils")]
    Derived(fn(&Record) -> String),
    /// A value the previous revision accepts but never writes, so a re-upgrade recognizes it as
    /// unknown; optional per field
    Sentinel(&'static str),
}

/// The previous revision's variant that a new enum variant maps to on a lossy downgrade; on a
/// security-sensitive field, a variant whose grants are a subset of the new one's
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumMapping {
    /// The field holding the enum
    pub field: &'static str,
    /// The variant added in this revision
    pub new_variant: &'static str,
    /// The previous revision's variant it maps to (no more privileged than `new_variant` by the
    /// field's `PrivilegeOrder`, for a sensitive field)
    pub old_variant: &'static str,
}

/// The grants each variant of a security-sensitive role field carries, so the fill lint can check
/// that a mapping or fill never widens anyone's rights
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegeOrder {
    /// The field holding the role
    pub field: &'static str,
    /// Every variant the field can hold in this revision or the previous one; the lint rejects an
    /// order that leaves one out
    pub variants: &'static [VariantGrants],
}

/// One role variant and its grants in a privilege order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantGrants {
    /// The variant's name, such as `User`
    pub variant: &'static str,
    /// The grants it carries, by name; a variant whose fields carry grants (`Developer`'s scaler
    /// flags) lists every flag, so as a target it counts at its most privileged form
    pub grants: &'static [&'static str],
}
