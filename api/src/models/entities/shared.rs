//! Contains models shared between multiple entity kinds

use strum::EnumString;

/// A critical sector that an entity is associated with
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    EnumString,
    strum::Display,
    Hash,
)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum CriticalSector {
    Chemical,
    CommercialFacilities,
    Communications,
    CriticalManufacturing,
    Dams,
    DefenseIndustrialBase,
    EmergencyServices,
    Energy,
    FinancialServices,
    FoodAgriculture,
    GovernmentServicesFacilities,
    HealthcarePublicHealth,
    InformationTechnology,
    NuclearReactorsMaterialsWaste,
    TransportSystems,
    WaterWasteWater,
}

/// Split stored tool references into tool instance ids and legacy tool names
///
/// Entities store the tool instances that produced them as UUID strings, and rows stored before
/// tool instances existed hold tool names instead, so every stored string that parses as a UUID
/// is a tool instance and every other string is a legacy tool name.
///
/// # Arguments
///
/// * `tools` - The stored tool strings
/// * `legacy_tools` - The stored legacy tool names
pub(crate) fn split_tool_refs(
    tools: Vec<String>,
    mut legacy_tools: Vec<String>,
) -> (Vec<uuid::Uuid>, Vec<String>) {
    // keep the tool instance ids in their stored order
    let mut ids = Vec::with_capacity(tools.len());
    // sort each stored tool string into ids or legacy names
    for tool in tools {
        // a string that parses as a UUID is a tool instance id
        match uuid::Uuid::parse_str(&tool) {
            Ok(id) => ids.push(id),
            Err(_) => legacy_tools.push(tool),
        }
    }
    (ids, legacy_tools)
}

/// Add tool instance ids to the ones an entity records, skipping any it already records
///
/// Each tool instance is recorded once, in the order it was first added, however often a form
/// or a concurrent writer sends it.
///
/// # Arguments
///
/// * `tools` - The tool instance ids the entity records
/// * `add` - The tool instance ids to add
#[cfg(feature = "api")]
pub(crate) fn add_tool_ids(tools: &mut Vec<uuid::Uuid>, add: Vec<uuid::Uuid>) {
    // add each id that isn't recorded yet
    for id in add {
        // skip ids that are already recorded
        if !tools.contains(&id) {
            tools.push(id);
        }
    }
}

/// Tests for the helpers shared between entity kinds
#[cfg(all(test, feature = "api"))]
mod tests {
    use uuid::Uuid;

    /// Adding tool instance ids skips ids already recorded or repeated and keeps the first order
    #[test]
    fn add_tool_ids_skips_duplicates() {
        // two recorded ids and a new one
        let (first, second, third) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut tools = vec![first, second];
        // add a recorded id, the new id twice, and another recorded id
        super::add_tool_ids(&mut tools, vec![second, third, third, first]);
        // each id appears once in the order it was first added
        assert_eq!(tools, vec![first, second, third]);
        // a repeated list added to nothing keeps its first order
        let mut empty = Vec::new();
        super::add_tool_ids(&mut empty, vec![third, first, third]);
        assert_eq!(empty, vec![third, first]);
    }
}
