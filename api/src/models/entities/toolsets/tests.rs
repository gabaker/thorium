//! Tests for the toolset models

use chrono::Utc;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, Hash};
use uuid::Uuid;

use super::{
    SelectorMatches, ToolSetEntity, ToolSetEntityRequest, ToolSetExclusion, ToolSetExclusionReason,
    ToolSetInclusion, ToolSetInstanceEntity, ToolSetInstanceEntityRequest, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetMember, ToolSetSelector, ToolSetUpdateFields,
};
use crate::models::{EntityMetadataRequest, EntityMetadataUpdate};

/// Serialize a value to JSON and back, checking it serializes the same both times
///
/// # Arguments
///
/// * `value` - The value to round trip
fn round_trip_json<T: Serialize + DeserializeOwned>(value: &T) {
    // compare the serialized values since not every type is comparable
    let first = serde_json::to_value(value).unwrap();
    let back: T = serde_json::from_value(first.clone()).unwrap();
    assert_eq!(serde_json::to_value(&back).unwrap(), first);
}

/// Hash a value with the standard library's hasher
///
/// # Arguments
///
/// * `value` - The value to hash
fn std_hash<T: Hash>(value: &T) -> u64 {
    // a hasher with fixed keys, so equal values hash equally
    std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default()
        .hash_one(value)
}

/// Build a member of a toolset instance
///
/// # Arguments
///
/// * `tool` - The member's tool
/// * `spec_hash` - The member's spec hash
/// * `inclusion` - How the member was included
fn member(tool: Uuid, spec_hash: &str, inclusion: ToolSetInclusion) -> ToolSetMember {
    ToolSetMember {
        tool,
        tool_instance: Uuid::new_v4(),
        name: format!("tool-{}", &tool.to_string()[..8]),
        groups: vec!["minerva".into()],
        version: "1.0.0".into(),
        spec_hash: spec_hash.into(),
        function_name: format!("call_{}", &tool.simple().to_string()[..8]),
        inclusion,
    }
}

/// A toolset instance with two members, one exclusion, and every field set
fn instance() -> ToolSetInstanceEntity {
    // two members included two ways
    let members = vec![
        member(Uuid::new_v4(), "v1:aa", ToolSetInclusion::ExplicitLatest),
        member(Uuid::new_v4(), "v1:bb", ToolSetInclusion::Selector),
    ];
    let mut instance = ToolSetInstanceEntity {
        tool_set: Uuid::new_v4(),
        tool_set_groups: vec!["minerva".into(), "mars".into()],
        revision: 2,
        previous: Some(Uuid::new_v4()),
        status: ToolSetInstanceStatus::Ready,
        warnings: vec!["rename_symbol was left out".into()],
        trigger: ToolSetInstanceTrigger::Manual,
        triggered_by: "europa".into(),
        read_only: true,
        selector: Some(ToolSetSelector::new("toolset", "ghidra")),
        members,
        excluded: vec![ToolSetExclusion {
            tool: Uuid::new_v4(),
            tool_instance: Some(Uuid::new_v4()),
            name: "rename_symbol".into(),
            inclusion: ToolSetInclusion::Selector,
            reason: ToolSetExclusionReason::NotTaggedInAllGroups,
            missing_groups: vec!["mars".into()],
        }],
        digest: String::new(),
    };
    instance.digest = instance.compute_digest().unwrap();
    instance
}

/// Every toolset struct and enum round trips through serde JSON
#[test]
fn serde_round_trips() {
    // the root with every field set
    let root = ToolSetEntity {
        read_only: true,
        selector: Some(ToolSetSelector::new("toolset", "ghidra")),
        auto_instance: false,
        tools: Vec::new(),
        modified: Utc::now(),
        latest_instance: Some(Uuid::new_v4()),
        highest_revision: 7,
    };
    round_trip_json(&root);
    // the instance and its parts
    let full = instance();
    round_trip_json(&full);
    let back: ToolSetInstanceEntity =
        serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
    assert_eq!(back, full);
    round_trip_json(&full.members[0]);
    round_trip_json(&full.excluded[0]);
    round_trip_json(&ToolSetSelector::new("a", "b"));
    for status in [
        ToolSetInstanceStatus::Ready,
        ToolSetInstanceStatus::Invalid {
            errors: vec!["no latest".into()],
        },
    ] {
        round_trip_json(&status);
    }
    for trigger in [
        ToolSetInstanceTrigger::ToolSetUpdated,
        ToolSetInstanceTrigger::ToolUpdated {
            tool: Uuid::new_v4(),
            tool_instance: Uuid::new_v4(),
        },
        ToolSetInstanceTrigger::Manual,
    ] {
        round_trip_json(&trigger);
    }
    // the requests
    let req = ToolSetEntityRequest::default()
        .tool(Uuid::new_v4())
        .selector(ToolSetSelector::new("toolset", "ghidra"))
        .read_only(true)
        .auto_instance(false);
    round_trip_json(&req);
    round_trip_json(&ToolSetInstanceEntityRequest::new(&full).unwrap());
    // the update variant
    let update = EntityMetadataUpdate::ToolSet {
        read_only: Some(false),
        selector: Some(ToolSetSelector::new("k", "v")),
        clear_selector: None,
        auto_instance: Some(true),
        add_tools: vec![Uuid::new_v4()],
        remove_tools: vec![Uuid::new_v4()],
        extend_target_groups: true,
    };
    round_trip_json(&update);
}

/// Missing root fields take their defaults and an unset selector isn't written
#[test]
fn root_defaults() {
    // a root with only its modified time
    let raw = json!({"modified": "2026-10-16T00:00:00Z"});
    let root: ToolSetEntity = serde_json::from_value(raw).unwrap();
    assert!(root.auto_instance);
    assert!(!root.read_only);
    assert!(root.selector.is_none());
    assert!(root.tools.is_empty());
    assert!(root.latest_instance.is_none());
    // a root stored before it recorded its highest revision has none
    assert_eq!(root.highest_revision, 0);
    // an unset selector and latest instance are left out of the JSON
    let value = serde_json::to_value(&root).unwrap();
    assert!(value.get("selector").is_none(), "{value}");
    assert!(value.get("latest_instance").is_none(), "{value}");
}

/// The requests hash, so they fit in an entity metadata request
#[test]
fn requests_hash() {
    // equal requests hash equally
    let tool = Uuid::new_v4();
    let left = ToolSetEntityRequest::default().tool(tool);
    let right = ToolSetEntityRequest::default().tool(tool);
    assert_eq!(std_hash(&left), std_hash(&right));
    assert_eq!(
        std_hash(&ToolSetSelector::new("a", "b")),
        std_hash(&ToolSetSelector::new("a", "b"))
    );
    let instance = ToolSetInstanceEntityRequest {
        instance: "{}".into(),
    };
    assert_eq!(std_hash(&instance), std_hash(&instance.clone()));
    // both fit in an entity metadata request, which hashes
    let toolset = EntityMetadataRequest::ToolSet(left);
    let submitted = EntityMetadataRequest::ToolSetInstance(instance);
    let _ = (std_hash(&toolset), std_hash(&submitted));
}

/// The enums serialize in their externally tagged form
#[test]
fn enum_forms() {
    // unit variants are their names
    assert_eq!(
        serde_json::to_value(ToolSetInclusion::ExplicitLatest).unwrap(),
        json!("ExplicitLatest")
    );
    assert_eq!(
        serde_json::to_value(ToolSetInclusion::ExplicitPinned).unwrap(),
        json!("ExplicitPinned")
    );
    assert_eq!(
        serde_json::to_value(ToolSetInclusion::Selector).unwrap(),
        json!("Selector")
    );
    for (reason, name) in [
        (ToolSetExclusionReason::NotReadOnly, "NotReadOnly"),
        (ToolSetExclusionReason::NotAgentic, "NotAgentic"),
        (ToolSetExclusionReason::NoLatest, "NoLatest"),
        (
            ToolSetExclusionReason::NotTaggedInAllGroups,
            "NotTaggedInAllGroups",
        ),
    ] {
        assert_eq!(serde_json::to_value(reason).unwrap(), json!(name));
    }
    // data variants wrap their payload in their name
    assert_eq!(
        serde_json::to_value(ToolSetInstanceStatus::Ready).unwrap(),
        json!("Ready")
    );
    assert_eq!(
        serde_json::to_value(ToolSetInstanceStatus::Invalid {
            errors: vec!["x".into()]
        })
        .unwrap(),
        json!({"Invalid": {"errors": ["x"]}})
    );
    let (tool, tool_instance) = (Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(
        serde_json::to_value(ToolSetInstanceTrigger::ToolUpdated {
            tool,
            tool_instance
        })
        .unwrap(),
        json!({"ToolUpdated": {"tool": tool, "tool_instance": tool_instance}})
    );
    assert_eq!(
        serde_json::to_value(ToolSetInstanceTrigger::ToolSetUpdated).unwrap(),
        json!("ToolSetUpdated")
    );
    assert_eq!(
        serde_json::to_value(ToolSetInstanceTrigger::Manual).unwrap(),
        json!("Manual")
    );
}

/// Members order by tool then tool instance, exclusions sort by tool, and sorted lists
/// serialize identically however they were read
#[test]
fn member_order() {
    // two members of the same tool order by their instance
    let tool = Uuid::from_u128(5);
    let mut first = member(tool, "v1:aa", ToolSetInclusion::Selector);
    first.tool_instance = Uuid::from_u128(1);
    let mut second = member(tool, "v1:aa", ToolSetInclusion::ExplicitLatest);
    second.tool_instance = Uuid::from_u128(2);
    assert!(first < second);
    // a lower tool orders first whatever its instance
    let mut lower = member(Uuid::from_u128(4), "v1:zz", ToolSetInclusion::Selector);
    lower.tool_instance = Uuid::from_u128(9);
    assert!(lower < first);
    // the same members read in two orders serialize to identical bytes once sorted
    let mut one = instance();
    let mut two = one.clone();
    two.members.reverse();
    let extra = ToolSetExclusion {
        tool: Uuid::from_u128(1),
        tool_instance: None,
        name: "image_tool".into(),
        inclusion: ToolSetInclusion::Selector,
        reason: ToolSetExclusionReason::NotAgentic,
        missing_groups: Vec::new(),
    };
    one.excluded.push(extra.clone());
    two.excluded.insert(0, extra);
    two.excluded.reverse();
    one.sort_lists();
    two.sort_lists();
    assert_eq!(
        serde_json::to_vec(&one).unwrap(),
        serde_json::to_vec(&two).unwrap()
    );
    assert!(one.members.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(
        one.excluded
            .windows(2)
            .all(|pair| pair[0].tool <= pair[1].tool)
    );
    // order never changes the digest
    assert_eq!(one.compute_digest().unwrap(), two.compute_digest().unwrap());
}

/// `triggered_by` and `missing_groups` round trip, default empty, and leave the digest alone
#[test]
fn attribution_and_missing_groups() {
    // both fields round trip
    let full = instance();
    let value = serde_json::to_value(&full).unwrap();
    assert_eq!(value["triggered_by"], json!("europa"));
    assert_eq!(value["excluded"][0]["missing_groups"], json!(["mars"]));
    assert_eq!(
        value["excluded"][0]["reason"],
        json!("NotTaggedInAllGroups")
    );
    // JSON without them deserializes empty
    let mut stripped = value.clone();
    stripped.as_object_mut().unwrap().remove("triggered_by");
    stripped["excluded"][0]
        .as_object_mut()
        .unwrap()
        .remove("missing_groups");
    let back: ToolSetInstanceEntity = serde_json::from_value(stripped).unwrap();
    assert_eq!(back.triggered_by, "");
    assert_eq!(back.excluded[0].missing_groups, Vec::<String>::new());
    // an exclusion without missing groups doesn't write them
    let exclusion = serde_json::to_value(&back.excluded[0]).unwrap();
    assert!(exclusion.get("missing_groups").is_none(), "{exclusion}");
    // neither field changes the digest
    assert_eq!(back.compute_digest().unwrap(), full.digest);
}

/// The digest covers the settings and each member's spec hash and inclusion, and nothing else
#[test]
fn digest_inputs() {
    // the digest is a v1 hash
    let base = instance();
    assert!(base.digest.starts_with("v1:") && base.digest.len() == 67);
    // these changes keep the digest
    let mut same = base.clone();
    same.tool_set = Uuid::new_v4();
    same.tool_set_groups = vec!["vulcan".into()];
    same.revision = 9;
    same.previous = None;
    same.status = ToolSetInstanceStatus::Invalid {
        errors: vec!["x".into()],
    };
    same.warnings.clear();
    same.trigger = ToolSetInstanceTrigger::ToolSetUpdated;
    same.excluded.clear();
    for member in &mut same.members {
        member.tool = Uuid::new_v4();
        member.tool_instance = Uuid::new_v4();
        member.name = "renamed".into();
        member.groups = vec!["vulcan".into()];
        member.version = "9.9.9".into();
    }
    assert_eq!(same.compute_digest().unwrap(), base.digest);
    // each of these changes it
    let mut read_only = base.clone();
    read_only.read_only = false;
    let mut selector = base.clone();
    selector.selector = Some(ToolSetSelector::new("toolset", "radare"));
    let mut cleared = base.clone();
    cleared.selector = None;
    let mut spec = base.clone();
    spec.members[0].spec_hash = "v1:cc".into();
    let mut inclusion = base.clone();
    inclusion.members[0].inclusion = ToolSetInclusion::ExplicitPinned;
    let mut fewer = base.clone();
    fewer.members.pop();
    for changed in [read_only, selector, cleared, spec, inclusion, fewer] {
        assert_ne!(changed.compute_digest().unwrap(), base.digest);
    }
}

/// A selector is two non-empty parts, parsed from `key=value`
#[test]
fn selector_parsing() {
    // the first equals sign splits the pair
    let parsed: ToolSetSelector = "toolset=ghidra=11".parse().unwrap();
    assert_eq!(parsed, ToolSetSelector::new("toolset", "ghidra=11"));
    assert_eq!(parsed.to_string(), "toolset=ghidra=11");
    // a missing equals sign or an empty part is refused
    for bad in ["toolset", "=ghidra", "toolset=", ""] {
        assert!(bad.parse::<ToolSetSelector>().is_err(), "{bad}");
    }
}

/// The selector matches only tools tagged in every one of the toolset's groups
#[test]
fn selector_matches_every_group() {
    // a toolset in minerva and mars
    let groups = vec!["minerva".to_owned(), "mars".to_owned()];
    let (both, only_minerva, elsewhere) =
        (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    // the groups each tool carries the tag in
    let mut tagged: BTreeMap<Uuid, BTreeSet<String>> = BTreeMap::new();
    tagged.insert(both, BTreeSet::from(["minerva".into(), "mars".into()]));
    tagged.insert(only_minerva, BTreeSet::from(["minerva".into()]));
    tagged.insert(elsewhere, BTreeSet::from(["vulcan".into()]));
    // the tool tagged in both matches and the one tagged in minerva only is excluded
    let SelectorMatches { matched, partial } = ToolSetSelector::partition(&groups, &tagged);
    assert_eq!(matched, vec![both]);
    assert_eq!(partial, vec![(only_minerva, vec!["mars".to_owned()])]);
    // its exclusion reads "not tagged in `mars`"
    let exclusion = ToolSetExclusion {
        tool: only_minerva,
        tool_instance: None,
        name: "decompile".into(),
        inclusion: ToolSetInclusion::Selector,
        reason: ToolSetExclusionReason::NotTaggedInAllGroups,
        missing_groups: partial[0].1.clone(),
    };
    assert_eq!(exclusion.describe(), "not tagged in `mars`");
}

/// The request and update send the documented form fields
#[test]
fn form_fields() {
    // a create request with every field
    let (tool, pinned) = (Uuid::new_v4(), Uuid::new_v4());
    let req = ToolSetEntityRequest::default()
        .tool(tool)
        .tool(pinned)
        .selector(ToolSetSelector::new("toolset", "ghidra"))
        .read_only(true)
        .auto_instance(false)
        .extend_target_groups();
    let fields = req.form_fields();
    assert!(fields.contains(&("kind", "ToolSet".to_owned())));
    assert!(fields.contains(&("metadata[toolset_tools][]", tool.to_string())));
    assert!(fields.contains(&("metadata[toolset_tools][]", pinned.to_string())));
    assert!(fields.contains(&("metadata[toolset_selector_key]", "toolset".to_owned())));
    assert!(fields.contains(&("metadata[toolset_selector_value]", "ghidra".to_owned())));
    assert!(fields.contains(&("metadata[toolset_read_only]", "true".to_owned())));
    assert!(fields.contains(&("metadata[toolset_auto_instance]", "false".to_owned())));
    assert!(fields.contains(&("metadata[extend_target_groups]", "true".to_owned())));
    // an empty request sends only its kind
    assert_eq!(
        ToolSetEntityRequest::default().form_fields(),
        vec![("kind", "ToolSet".to_owned())]
    );
    // a submitted instance is one JSON field
    let full = instance();
    let fields = ToolSetInstanceEntityRequest::new(&full)
        .unwrap()
        .form_fields();
    assert_eq!(fields[0], ("kind", "ToolSetInstance".to_owned()));
    let (name, raw) = &fields[1];
    assert_eq!(*name, "metadata[toolset_instance]");
    assert_eq!(
        serde_json::from_str::<ToolSetInstanceEntity>(raw).unwrap(),
        full
    );
    // an update sends only what it sets
    let (add, remove) = (Uuid::new_v4(), Uuid::new_v4());
    let selector = ToolSetSelector::new("k", "v");
    let update = ToolSetUpdateFields {
        read_only: Some(&false),
        selector: Some(&selector),
        clear_selector: Some(&true),
        auto_instance: None,
        add_tools: &[add],
        remove_tools: &[remove],
        extend_target_groups: false,
    };
    let fields = update.form_fields();
    assert_eq!(
        fields,
        vec![
            ("metadata[toolset_read_only]", "false".to_owned()),
            ("metadata[toolset_selector_key]", "k".to_owned()),
            ("metadata[toolset_selector_value]", "v".to_owned()),
            ("metadata[clear_toolset_selector]", "true".to_owned()),
            ("metadata[add_toolset_tools][]", add.to_string()),
            ("metadata[remove_toolset_tools][]", remove.to_string()),
        ]
    );
}
