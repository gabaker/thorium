//! Unit tests for the migration report types and the pure image, pipeline, and provenance
//! conversions

use chrono::{TimeZone, Utc};
use std::collections::{BTreeMap, HashMap, HashSet};
use uuid::Uuid;

use super::{
    DependencyKind, GroupEntity, ImageClass, ImageContext, MIGRATED_FROM_IMAGE_TAG,
    MIGRATED_INSTANCE_NAMESPACE, MIGRATED_TOOL_NAMESPACE, MIGRATION_OPERATOR, MigratedSubmitter,
    MigrationCategory, MigrationNote, MigrationReason, ProvenanceClass, ProvenanceRow, SkippedItem,
    StageTarget, ToolIdMap, classify, classify_image, convert_image, migrated_at, remedy,
    resolve_stages, rewrite_dependencies, up,
};
use crate::models::{
    DroppedImageField, EntityKinds, EntityMetadata, HostPath, Image, ImageScaler, Pipeline,
    ToolInstanceSummary, ToolSpec, ToolVersion, Volume, VolumeTypes, WorkloadEnvironment,
    WorkloadSpec,
};

/// Build a K8s image with nothing but its group, name, creator, and container image set
///
/// # Arguments
///
/// * `group` - The image's group
/// * `name` - The image's name
fn image(group: &str, name: &str) -> Image {
    // start from the projection of an empty spec and make it a K8s image
    let mut image = WorkloadSpec::default().project_image(group, name, 600.0);
    image.scaler = ImageScaler::K8s;
    image.image = Some(format!("repo/{name}:1.0"));
    image.creator = "io".to_owned();
    image
}

/// Build an image with env variables
///
/// # Arguments
///
/// * `name` - The image's name
/// * `scaler` - The image's scaler
/// * `env` - The image's env variables and their values
fn env_image(name: &str, scaler: ImageScaler, env: &[(&str, &str)]) -> Image {
    // set the scaler and the env variables
    let mut image = image("jupiter", name);
    image.scaler = scaler;
    image.env = env
        .iter()
        .map(|(key, value)| ((*key).to_owned(), Some((*value).to_owned())))
        .collect();
    image
}

/// Convert an image whose creator is still in its group, with every image mapped
///
/// # Arguments
///
/// * `image` - The image to convert
fn convert(image: &Image) -> super::ImageMigration {
    // map only this image and convert it
    let map = ToolIdMap::from_images([image]);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    convert_image(image, &ctx).expect("the image converts")
}

/// Render everything a conversion reports, so tests can search it for values
///
/// # Arguments
///
/// * `migration` - The conversion to render
fn reported(migration: &super::ImageMigration) -> String {
    // the report holds the notes, the warnings, the issues, and the dropped fields
    let dropped = migration
        .tool
        .dropped
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>();
    format!(
        "{:?}{:?}{:?}{dropped:?}",
        migration.notes, migration.warnings, migration.issues
    )
}

/// Build a tool in a group, optionally tagged with the image it was migrated from
///
/// # Arguments
///
/// * `group` - The group the tool is seen in
/// * `name` - The tool's name
/// * `migrated_from` - The image named by its tag
/// * `instances` - The ids of its instances, oldest first
fn tool(group: &str, name: &str, migrated_from: Option<&str>, instances: &[Uuid]) -> GroupEntity {
    // give each instance a creation time a minute after the one before
    let start = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let instances = instances
        .iter()
        .enumerate()
        .map(|(index, id)| ToolInstanceSummary {
            id: *id,
            version: ToolVersion::Custom(format!("v{index}")),
            spec_hash: String::new(),
            deprecated: false,
            created: start + chrono::Duration::minutes(i64::try_from(index).unwrap()),
        })
        .collect();
    GroupEntity {
        id: Uuid::new_v4(),
        kind: EntityKinds::Tool,
        group: group.to_owned(),
        name: name.to_owned(),
        migrated_from_image: migrated_from.map(ToOwned::to_owned),
        instances,
    }
}

/// Build a pipeline in a group with some stages and stage tools
///
/// # Arguments
///
/// * `group` - The pipeline's group
/// * `stages` - The pipeline's stages, one per step
/// * `stage_tools` - The stages a tool already backs
fn pipeline(group: &str, stages: &[&str], stage_tools: &[(&str, Uuid)]) -> Pipeline {
    // one stage per step, with the given stage tools
    Pipeline {
        group: group.to_owned(),
        name: "harvest".to_owned(),
        creator: "io".to_owned(),
        order: stages
            .iter()
            .map(|stage| vec![(*stage).to_owned()])
            .collect(),
        sla: 0,
        triggers: HashMap::new(),
        description: None,
        bans: HashMap::new(),
        stage_tools: stage_tools
            .iter()
            .map(|(stage, id)| ((*stage).to_owned(), *id))
            .collect(),
    }
}

/// A migrated spec's dependency settings name the tools migrated from the images its lists
/// named, in the image's group, keeping every other dependency setting
#[test]
fn migrated_specs_name_dependency_tools() {
    // a restricts filesystems to b and c, and c restricts results to b and to gone
    let mut a = image("corn", "a");
    a.dependencies.filesystems.enabled = true;
    a.dependencies.filesystems.images = vec!["b".to_owned(), "c".to_owned()];
    a.dependencies.filesystems.location = "/tmp/prior".to_owned();
    let b = image("corn", "b");
    let mut c = image("corn", "c");
    c.dependencies.results.images = vec!["b".to_owned(), "gone".to_owned()];
    c.dependencies.children.enabled = true;
    let map = ToolIdMap::from_images([&a, &b, &c]);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    let b_id = ToolIdMap::tool_id("corn", "b");
    let c_id = ToolIdMap::tool_id("corn", "c");
    // a's spec names b's and c's tools and keeps its location
    let ToolSpec::Image(spec) = convert_image(&a, &ctx).unwrap().tool.spec else {
        panic!("not an image spec");
    };
    let deps = &spec.data.dependencies;
    assert_eq!(deps.filesystems.tools, vec![b_id, c_id]);
    assert!(deps.filesystems.enabled);
    assert_eq!(deps.filesystems.location, "/tmp/prior");
    assert_eq!(deps.results.tools, Vec::<Uuid>::new());
    // c's spec names only b's tool, with gone dropped, and an unrestricted children list
    let ToolSpec::Image(spec) = convert_image(&c, &ctx).unwrap().tool.spec else {
        panic!("not an image spec");
    };
    let deps = &spec.data.dependencies;
    assert_eq!(deps.results.tools, vec![b_id]);
    assert!(deps.children.enabled);
    assert_eq!(deps.children.tools, Vec::<Uuid>::new());
}

/// Every env name that looks like a secret is named in one warning per image, with no value,
/// and an image with clean names gets no warning
#[test]
fn secret_like_env_names_warn_without_values() {
    // images with secret-like names in every spelling, and one with clean names
    let secret = env_image(
        "callisto",
        ImageScaler::K8s,
        &[
            ("API_KEY", "value-1"),
            ("github_token", "value-2"),
            ("DbPassword", "value-3"),
            ("AWS_SECRET_ACCESS_KEY", "value-4"),
            ("SERVICE_CREDENTIALS", "value-5"),
            ("LOG_LEVEL", "value-6"),
        ],
    );
    let clean = env_image(
        "ganymede",
        ImageScaler::K8s,
        &[("LOG_LEVEL", "info"), ("PATH", "/bin")],
    );
    // the secret-like image gets one warning naming exactly the matching names
    let migration = convert(&secret);
    assert_eq!(migration.warnings.len(), 1, "{:?}", migration.warnings);
    let warning = &migration.warnings[0];
    assert_eq!(warning.item, "callisto");
    for name in [
        "API_KEY",
        "github_token",
        "DbPassword",
        "AWS_SECRET_ACCESS_KEY",
        "SERVICE_CREDENTIALS",
    ] {
        assert!(warning.note.contains(name), "{name} in {}", warning.note);
    }
    assert!(!warning.note.contains("LOG_LEVEL"), "{}", warning.note);
    // no value reaches anything the report holds
    let text = reported(&migration);
    for value in [
        "value-1", "value-2", "value-3", "value-4", "value-5", "value-6",
    ] {
        assert!(!text.contains(value), "{value} in {text}");
    }
    // the image is still migrated, with its env intact in the spec
    let ToolSpec::Image(spec) = &migration.tool.spec else {
        panic!("not an image spec");
    };
    let WorkloadEnvironment::K8s(env) = &spec.environment else {
        panic!("not a K8s spec");
    };
    assert_eq!(env.env.len(), 6);
    // the clean image gets no warning
    assert_eq!(convert(&clean).warnings, Vec::new());
}

/// Each environment's dropped fields are listed per image with env variables by name only, and
/// a K8s image keeps its env values in its spec
#[test]
fn dropped_fields_name_env_variables_only() {
    // a BareMetal image with env and a volume
    let mut bare_metal = env_image(
        "europa",
        ImageScaler::BareMetal,
        &[("API_TOKEN", "s3cr3t"), ("LOG_LEVEL", "debug")],
    );
    bare_metal.image = None;
    bare_metal.volumes = vec![Volume {
        name: "scratch".into(),
        archetype: VolumeTypes::HostPath,
        mount_path: "/scratch".into(),
        sub_path: None,
        read_only: true,
        kustomize: false,
        host_path: Some(HostPath {
            path: "/mnt/scratch".into(),
            path_type: None,
        }),
        config_map: None,
        secret: None,
        nfs: None,
    }];
    // a Windows image with env and network policies
    let mut windows = env_image("io", ImageScaler::Windows, &[("API_TOKEN", "s3cr3t")]);
    windows.network_policies = HashSet::from(["allow-egress".to_owned()]);
    // the BareMetal image lists env by name and the volume
    let migration = convert(&bare_metal);
    assert_eq!(
        migration.tool.dropped,
        vec![
            DroppedImageField::Env(vec!["API_TOKEN".to_owned(), "LOG_LEVEL".to_owned()]),
            DroppedImageField::Volumes,
        ]
    );
    let dropped_note = migration
        .notes
        .iter()
        .find(|note| note.note.contains("dropped fields"))
        .expect("a dropped-field note");
    assert!(dropped_note.note.contains("env (API_TOKEN, LOG_LEVEL)"));
    assert!(dropped_note.note.contains("volumes"));
    // no value reaches the report
    let text = reported(&migration);
    assert!(
        !text.contains("s3cr3t") && !text.contains("debug"),
        "{text}"
    );
    // the BareMetal image still gets its secret-like name warning
    assert_eq!(migration.warnings.len(), 1);
    assert!(migration.warnings[0].note.contains("API_TOKEN"));
    // the Windows image lists env by name and its network policies
    let migration = convert(&windows);
    assert_eq!(
        migration.tool.dropped,
        vec![
            DroppedImageField::Env(vec!["API_TOKEN".to_owned()]),
            DroppedImageField::NetworkPolicies,
        ]
    );
    assert!(!reported(&migration).contains("s3cr3t"));
    // a K8s image with the same env drops nothing and keeps both values
    let k8s = env_image(
        "ganymede",
        ImageScaler::K8s,
        &[("API_TOKEN", "s3cr3t"), ("LOG_LEVEL", "debug")],
    );
    let migration = convert(&k8s);
    assert_eq!(migration.tool.dropped, Vec::new());
    let ToolSpec::Image(spec) = &migration.tool.spec else {
        panic!("not an image spec");
    };
    let WorkloadEnvironment::K8s(env) = &spec.environment else {
        panic!("not a K8s spec");
    };
    assert_eq!(env.env.get("API_TOKEN"), Some(&Some("s3cr3t".to_owned())));
    assert_eq!(env.env.get("LOG_LEVEL"), Some(&Some("debug".to_owned())));
}

/// An image name used in two groups becomes one tool in each, and a tool with the image's name
/// that wasn't migrated from it conflicts only in its own group
#[test]
fn image_names_convert_per_group_and_conflicts_skip() {
    // the same image name in two groups
    let corn = image("corn", "ghidra");
    let oranges = image("oranges", "ghidra");
    let map = ToolIdMap::from_images([&corn, &oranges]);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    let corn_tool = convert_image(&corn, &ctx).unwrap().tool;
    let oranges_tool = convert_image(&oranges, &ctx).unwrap().tool;
    // each becomes a tool in its own group with its own id
    assert_eq!(corn_tool.group, "corn");
    assert_eq!(oranges_tool.group, "oranges");
    assert_eq!(corn_tool.name, "ghidra");
    assert_ne!(corn_tool.id, oranges_tool.id);
    // the tool is tagged with the image it was migrated from
    assert_eq!(
        corn_tool.tags.get(MIGRATED_FROM_IMAGE_TAG),
        Some(&HashSet::from(["ghidra".to_owned()]))
    );
    // an untagged tool named ghidra in corn conflicts only in corn
    let existing = vec![tool("corn", "ghidra", None, &[Uuid::new_v4()])];
    match classify_image("corn", "ghidra", &existing) {
        ImageClass::Conflict(item) => {
            assert_eq!(item.reason.code(), "invalid_value:name");
            assert_eq!(item.group, "corn");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(
        classify_image("oranges", "ghidra", &existing),
        ImageClass::New
    );
    // an entity of another kind tagged with the image doesn't count as migrated
    let mut flag = tool("oranges", "suspicious", Some("ghidra"), &[]);
    flag.kind = EntityKinds::Flag;
    assert_eq!(
        classify_image("oranges", "ghidra", &[flag]),
        ImageClass::New
    );
    // a tool tagged with the image was migrated by an earlier run
    let migrated = tool("oranges", "ghidra", Some("ghidra"), &[Uuid::new_v4()]);
    let migrated_id = migrated.id;
    assert_eq!(
        classify_image("oranges", "ghidra", &[migrated]),
        ImageClass::AlreadyMigrated { tool: migrated_id }
    );
}

/// The creator still in the group publishes the tool, anyone else gives the operator account,
/// each with a note, and versions are kept or labeled as migrated with the last-modified time
#[test]
fn submitter_and_version_choices_are_noted() {
    // an image with a SemVer version whose creator is still in the group
    let mut versioned = image("corn", "strings");
    versioned.creator = "europa".to_owned();
    versioned.version = Some(ToolVersion::from("1.2.3"));
    let map = ToolIdMap::from_images([&versioned]);
    let modified = Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap();
    let in_group = ImageContext {
        creator_in_group: true,
        last_modified: Some(modified),
        map: &map,
    };
    let migration = convert_image(&versioned, &in_group).unwrap();
    assert_eq!(
        migration.tool.submitter,
        MigratedSubmitter::Creator("europa".to_owned())
    );
    assert!(
        migration
            .notes
            .iter()
            .any(|note| note.note.contains("creator `europa`"))
    );
    assert!(matches!(migration.tool.version, ToolVersion::SemVer(_)));
    assert_eq!(migration.tool.version.to_string(), "1.2.3");
    // a creator that was deleted or left the group gives the operator account with a note
    let gone = ImageContext {
        creator_in_group: false,
        ..in_group
    };
    let migration = convert_image(&versioned, &gone).unwrap();
    assert_eq!(migration.tool.submitter, MigratedSubmitter::Operator);
    assert!(
        migration
            .notes
            .iter()
            .any(|note| note.note.contains("operator account") && note.note.contains("europa"))
    );
    // no version gives the migrated label with the last-modified time in the notes
    versioned.version = None;
    let migration = convert_image(&versioned, &in_group).unwrap();
    assert_eq!(
        migration.tool.version,
        ToolVersion::Custom("migrated".to_owned())
    );
    assert!(
        migration
            .notes
            .iter()
            .any(|note| note.note.contains(&modified.to_rfc3339()))
    );
    // the description is the image's
    assert_eq!(migration.tool.description, versioned.description);
}

/// A K8s image without a container image to run is skipped with the conversion's field, and its
/// remedy points at creating the tool and pinning its stages
#[test]
fn unconvertible_images_skip_with_their_field() {
    // a K8s image with no container image
    let mut broken = image("corn", "empty");
    broken.image = None;
    let map = ToolIdMap::from_images([&broken]);
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    let item = convert_image(&broken, &ctx).unwrap_err();
    assert_eq!(item.reason.code(), "missing_field:image");
    assert_eq!(item.group, "corn");
    // the remedy names both commands
    assert!(item.reason.remedy().contains("thorctl tools create"));
    assert!(
        item.reason
            .remedy()
            .contains("thorctl pipelines stage-tool")
    );
}

/// Every reason code renders as `<category>:<field>`, serializes its category in snake case,
/// and has a fixed remedy
#[test]
fn reason_codes_render_and_have_remedies() {
    // each code this migration uses
    let codes = [
        (
            MigrationReason::missing_field("image"),
            "missing_field:image",
        ),
        (MigrationReason::invalid_value("name"), "invalid_value:name"),
        (
            DependencyKind::Results.dropped_reason(),
            "invalid_value:dependencies.results.images.*",
        ),
        (
            DependencyKind::FileSystems.emptied_reason(),
            "invalid_value:dependencies.filesystems.images",
        ),
        (
            MigrationReason::invalid_value("stages.*"),
            "invalid_value:stages.*",
        ),
        (
            MigrationReason::malformed_json("kind_data"),
            "malformed_json:kind_data",
        ),
        (
            MigrationReason::write_failed("reaction"),
            "write_failed:reaction",
        ),
    ];
    // each renders as its code and has a remedy of its own
    let mut remedies = Vec::new();
    for (reason, code) in &codes {
        assert_eq!(reason.code(), *code);
        assert_eq!(reason.to_string(), *code);
        assert_ne!(remedy(reason), "");
        remedies.push(remedy(reason));
    }
    remedies.sort_unstable();
    remedies.dedup();
    assert_eq!(remedies.len(), codes.len());
    // the skipped stage's remedy points at both commands
    let stage = MigrationReason::invalid_value("stages.*");
    assert!(stage.remedy().contains("thorctl tools create"));
    assert!(stage.remedy().contains("thorctl pipelines stage-tool"));
    // it tells the admin to set every stage, including converted ones, and that no rerun is needed
    assert!(stage.remedy().contains("set every stage of the pipeline"));
    assert!(
        stage
            .remedy()
            .contains("the stages that converted included")
    );
    assert!(stage.remedy().contains("no rerun is needed"));
    assert!(!stage.remedy().contains("pin the stage"));
    // the category serializes in snake case
    let value = serde_json::to_value(MigrationReason::new(
        MigrationCategory::WriteFailed,
        "reaction",
    ))
    .unwrap();
    assert_eq!(value["category"], "write_failed");
    // a skipped item and a note keep the parts they were built from
    let item = SkippedItem::new(
        "corn",
        "reaction 1",
        MigrationReason::write_failed("reaction"),
    );
    assert_eq!(item.group, "corn");
    assert_eq!(item.reason.code(), "write_failed:reaction");
    let note = MigrationNote::new("corn", "ghidra", "published as the operator account");
    assert_eq!(note.item, "ghidra");
    // the applied report's types only exist with scylla-utils, so the client-only build skips
    // the conversions into them
    #[cfg(feature = "scylla-utils")]
    {
        // a skipped item becomes the applied report's item with its reason as a code
        let reported = crate::models::schema::SkippedItem::from(&item);
        assert_eq!(reported.group, "corn");
        assert_eq!(reported.item, "reaction 1");
        assert_eq!(reported.reason, "write_failed:reaction");
        // a note keeps its text and carries whether to act on it
        let reported = note.report_note(true);
        assert_eq!(reported.note, "published as the operator account");
        assert!(reported.warning);
    }
}

/// Stages resolve to the first instance of the migrated tool in the pipeline's own group, are
/// left alone when already backed, and are skipped when their image wasn't migrated
#[test]
fn stages_resolve_to_first_instances_in_their_own_group() {
    // a pipeline whose first stage is already backed
    let backed = Uuid::new_v4();
    let pipeline = pipeline("corn", &["plant", "grow", "harvest"], &[("plant", backed)]);
    // grow's tool has a newer second instance, and harvest's image was skipped
    let first = Uuid::new_v4();
    let newer = Uuid::new_v4();
    let grow = tool("corn", "grow", Some("grow"), &[first, newer]);
    let grow_id = grow.id;
    // a same-named tool in another group never resolves
    let other_group = tool("oranges", "harvest", Some("harvest"), &[Uuid::new_v4()]);
    let resolution = resolve_stages(&pipeline, &[grow, other_group], None);
    // only grow gets an entry, pinned to the first instance
    assert_eq!(
        resolution.stage_tools(),
        BTreeMap::from([("grow".to_owned(), first)])
    );
    assert_eq!(
        resolution.stages.get("grow"),
        Some(&StageTarget::Pinned {
            tool: grow_id,
            tool_instance: first
        })
    );
    assert!(!resolution.stages.contains_key("plant"));
    // harvest is skipped with the stage code, naming the stage
    assert_eq!(resolution.skipped.len(), 1);
    assert_eq!(
        resolution.skipped[0].reason.code(),
        "invalid_value:stages.*"
    );
    assert!(resolution.skipped[0].item.contains("harvest"));
    assert!(resolution.changes());
    // the report maps grow to its tool and first instance
    let mappings = resolution.mappings(&pipeline);
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].tool_instance, first);
    // a pipeline whose stages are all backed doesn't change
    let pinned = self::pipeline("corn", &["plant"], &[("plant", backed)]);
    let resolution = resolve_stages(&pinned, &[], None);
    assert!(!resolution.changes());
    assert_eq!(resolution.skipped, Vec::new());
}

/// Before the tools are stored, a stage resolves through the id its image's tool would get
#[test]
fn stages_resolve_through_the_map_before_tools_exist() {
    // a pipeline with one stage whose image will be migrated and one whose image was skipped
    let pipeline = pipeline("corn", &["grow", "harvest"], &[]);
    let grow = image("corn", "grow");
    let map = ToolIdMap::from_images([&grow]);
    let resolution = resolve_stages(&pipeline, &[], Some(&map));
    // grow resolves to its future tool id and is counted as a change, but isn't written
    assert_eq!(
        resolution.stages.get("grow"),
        Some(&StageTarget::Planned {
            tool: ToolIdMap::tool_id("corn", "grow")
        })
    );
    assert!(resolution.changes());
    assert_eq!(resolution.stage_tools(), BTreeMap::new());
    // harvest is skipped
    assert_eq!(resolution.skipped.len(), 1);
    assert_eq!(
        resolution.skipped[0].reason.code(),
        "invalid_value:stages.*"
    );
}

/// Dependency names become the `UUIDv5` ids of the tools migrated from them in the same group,
/// through a cycle; a name that doesn't resolve is dropped with a code that holds no name; and a
/// list that would empty skips its image
#[test]
fn dependency_names_map_to_tool_ids() {
    // a filesystems on b, b children on a (a cycle), c results on b and gone, d filesystems on gone
    let mut a = image("corn", "a");
    a.dependencies.filesystems.images = vec!["b".to_owned()];
    let mut b = image("corn", "b");
    b.dependencies.children.images = vec!["a".to_owned()];
    let mut c = image("corn", "c");
    c.dependencies.results.images = vec!["b".to_owned(), "gone".to_owned()];
    let mut d = image("corn", "d");
    d.dependencies.filesystems.images = vec!["gone".to_owned()];
    let images = vec![a.clone(), b.clone(), c.clone(), d.clone()];
    // the ids are UUIDv5s of the group and name in the fixed namespace
    let mut map = ToolIdMap::from_images(&images);
    let a_id = Uuid::new_v5(&MIGRATED_TOOL_NAMESPACE, b"corn/a");
    let b_id = Uuid::new_v5(&MIGRATED_TOOL_NAMESPACE, b"corn/b");
    assert_eq!(map.get("corn", "a"), Some(a_id));
    assert_eq!(map.get("corn", "b"), Some(b_id));
    assert_eq!(a_id.get_version_num(), 5);
    // settling skips d, whose only list would become empty
    let mut skipped = BTreeMap::new();
    map.settle(&images, &mut skipped);
    assert_eq!(
        skipped.get(&("corn".to_owned(), "d".to_owned())),
        Some(&MigrationReason::invalid_value(
            "dependencies.filesystems.images"
        ))
    );
    assert_eq!(skipped.len(), 1);
    assert_eq!(map.get("corn", "d"), None);
    // a and b name each other's tools
    let rewrite = rewrite_dependencies("corn", &a.dependencies, &map).unwrap();
    assert_eq!(rewrite.tools.filesystems, vec![b_id]);
    let rewrite = rewrite_dependencies("corn", &b.dependencies, &map).unwrap();
    assert_eq!(rewrite.tools.children, vec![a_id]);
    // c drops gone with a note and an issue, and is migrated
    let ctx = ImageContext {
        creator_in_group: true,
        last_modified: None,
        map: &map,
    };
    let migration = convert_image(&c, &ctx).unwrap();
    assert_eq!(migration.tool.dependency_tools.results, vec![b_id]);
    assert_eq!(migration.issues.len(), 1);
    let code = migration.issues[0].reason.code();
    assert_eq!(code, "invalid_value:dependencies.results.images.*");
    assert!(!code.contains("gone"));
    assert!(
        migration
            .notes
            .iter()
            .any(|note| note.note.contains("gone"))
    );
    // d itself is skipped with its code, which holds no name
    let item = convert_image(&d, &ctx).unwrap_err();
    assert_eq!(
        item.reason.code(),
        "invalid_value:dependencies.filesystems.images"
    );
    assert!(!item.reason.code().contains("gone"));
    // a rerun computes the same map and the same skips
    let mut rerun = ToolIdMap::from_images(&images);
    let mut rerun_skipped = BTreeMap::new();
    rerun.settle(&images, &mut rerun_skipped);
    assert_eq!(rerun, map);
    assert_eq!(rerun_skipped, skipped);
    // an image in another group naming b doesn't resolve to this group's tool
    let mut other = image("oranges", "e");
    other.dependencies.results.images = vec!["b".to_owned()];
    assert_eq!(
        rewrite_dependencies("oranges", &other.dependencies, &map),
        Err(MigrationReason::invalid_value(
            "dependencies.results.images"
        ))
    );
}

/// Skipping an image removes it from the map, so an image that names only it is skipped next
#[test]
fn settling_follows_chains_of_skipped_images() {
    // x is skipped for another reason, y names only x, and z names only y
    let x = image("corn", "x");
    let mut y = image("corn", "y");
    y.dependencies.results.images = vec!["x".to_owned()];
    let mut z = image("corn", "z");
    z.dependencies.children.images = vec!["y".to_owned()];
    let images = vec![z, y, x];
    let mut map = ToolIdMap::from_images(&images);
    let mut skipped = BTreeMap::from([(
        ("corn".to_owned(), "x".to_owned()),
        MigrationReason::missing_field("image"),
    )]);
    map.settle(&images, &mut skipped);
    // every image in the chain is skipped with its own list's code
    assert_eq!(skipped.len(), 3);
    assert_eq!(
        skipped[&("corn".to_owned(), "z".to_owned())],
        MigrationReason::invalid_value("dependencies.children.images")
    );
    assert_eq!(map.len(), 0);
}

/// Build a provenance row
///
/// # Arguments
///
/// * `kind` - The row's kind
/// * `kind_data` - The row's kind data
fn row(kind: &str, kind_data: &str) -> ProvenanceRow {
    // key each row by its kind
    ProvenanceRow {
        key: format!("{kind}-row"),
        kind: kind.to_owned(),
        kind_data: kind_data.to_owned(),
    }
}

/// Re-serialize a stored row the way the API reads it
///
/// # Arguments
///
/// * `kind_data` - The stored kind data
fn shim(kind_data: &str) -> String {
    // read through the compat shim and write the typed value back
    let metadata = serde_json::from_str::<EntityMetadata>(kind_data).unwrap();
    serde_json::to_string(&metadata).unwrap()
}

/// The provenance rewrite classifies and rewrites every provenance kind, leaves other kinds and
/// broken rows alone, is idempotent, and matches the API's own reading of each row
#[test]
fn provenance_rewrite_properties() {
    // stored rows of each provenance kind with names, ids, and a name that parses as a UUID
    let id = Uuid::new_v4();
    let migratable = vec![
        row(
            "FileSystem",
            &format!(r#"{{"FileSystem":{{"sha256":"abc","tools":["binwalk","{id}"]}}}}"#),
        ),
        row(
            "DecompiledFunction",
            r#"{"DecompiledFunction":{"address":16,"tools":["ghidra"],"content":"x"}}"#,
        ),
        row(
            "WindowsProcessTree",
            r#"{"WindowsProcessTree":{"tools":["volatility"],"legacy_tools":["strings"]}}"#,
        ),
    ];
    // rows with nothing to move and rows of other kinds
    let current = vec![
        row(
            "FileSystem",
            &format!(r#"{{"FileSystem":{{"sha256":"abc","tools":["{id}"]}}}}"#),
        ),
        row("FileSystem", r#"{"FileSystem":{"sha256":"abc"}}"#),
        row(
            "WindowsProcessTree",
            r#"{"WindowsProcessTree":{"tools":[],"legacy_tools":["volatility"]}}"#,
        ),
        row("Other", r#""Other""#),
        row("Flag", "not even json"),
    ];
    // broken provenance rows
    let broken = vec![
        row("FileSystem", "{not json"),
        row("DecompiledFunction", r#"{"FileSystem":{"sha256":"abc"}}"#),
    ];
    // migratable rows move every name to legacy_tools and keep ids in tools
    for stored in &migratable {
        assert_eq!(
            classify(stored),
            ProvenanceClass::Migratable { layout: 1 },
            "{stored:?}"
        );
        let once = up(stored.clone()).unwrap();
        // the rewrite equals the API's reading of the row, and changes nothing else
        assert_eq!(once.kind_data, shim(&stored.kind_data));
        assert_eq!(once.key, stored.key);
        assert_eq!(once.kind, stored.kind);
        // a second rewrite changes nothing and the result is current
        assert_eq!(up(once.clone()).unwrap(), once);
        assert_eq!(classify(&once), ProvenanceClass::Current);
        // no stored tool string is a name any more
        let value = serde_json::from_str::<serde_json::Value>(&once.kind_data).unwrap();
        let tools = value[&once.kind]["tools"].as_array().unwrap();
        assert!(
            tools
                .iter()
                .all(|tool| Uuid::parse_str(tool.as_str().unwrap()).is_ok())
        );
    }
    // the names land in legacy_tools, after any already there
    let filesystem = up(migratable[0].clone()).unwrap();
    let value = serde_json::from_str::<serde_json::Value>(&filesystem.kind_data).unwrap();
    assert_eq!(
        value["FileSystem"]["tools"],
        serde_json::json!([id.to_string()])
    );
    assert_eq!(
        value["FileSystem"]["legacy_tools"],
        serde_json::json!(["binwalk"])
    );
    let tree = up(migratable[2].clone()).unwrap();
    let value = serde_json::from_str::<serde_json::Value>(&tree.kind_data).unwrap();
    assert_eq!(
        value["WindowsProcessTree"]["legacy_tools"],
        serde_json::json!(["strings", "volatility"])
    );
    // current rows stay current and rewrite to their own reading
    for stored in &current {
        assert_eq!(classify(stored), ProvenanceClass::Current, "{stored:?}");
        let once = up(stored.clone()).unwrap();
        assert_eq!(up(once.clone()).unwrap(), once);
    }
    // rows of other kinds come back exactly as stored
    assert_eq!(up(current[4].clone()).unwrap(), current[4]);
    // broken provenance rows are unrecognized with the serde code and refused by the rewrite
    for stored in &broken {
        let reason = MigrationReason::malformed_json("kind_data");
        assert_eq!(
            classify(stored),
            ProvenanceClass::Unrecognized {
                reason: reason.clone()
            },
            "{stored:?}"
        );
        assert_eq!(up(stored.clone()), Err(reason));
    }
}

/// The first instance id of a migrated image is a `UUIDv5` of its group and name in the instance
/// namespace, differs from the tool's id, and is stable per image
#[test]
fn first_instance_ids_are_deterministic_and_apart_from_tool_ids() {
    // the id is the UUIDv5 of the group and name in the instance namespace
    let id = ToolIdMap::first_instance_id("corn", "grow");
    assert_eq!(id, Uuid::new_v5(&MIGRATED_INSTANCE_NAMESPACE, b"corn/grow"));
    assert_eq!(id.get_version_num(), 5);
    // it never equals the tool's own id, and another image gets another id
    assert_ne!(id, ToolIdMap::tool_id("corn", "grow"));
    assert_ne!(id, ToolIdMap::first_instance_id("corn", "till"));
    assert_ne!(id, ToolIdMap::first_instance_id("wheat", "grow"));
}

/// A tool published as its creator carries the creator's username, and the operator account's
/// username is fixed so every other tool carries the same one
#[test]
fn submitters_name_the_user_a_tool_is_published_as() {
    // the creator publishes as themselves
    assert_eq!(MigratedSubmitter::Creator("io".to_owned()).username(), "io");
    // anyone else gets the operator account
    assert_eq!(MigratedSubmitter::Operator.username(), MIGRATION_OPERATOR);
}

/// Every migrated tool is created at one fixed time in the migration's year, so a repeated
/// publish keys the same rows
#[test]
fn the_migration_time_is_fixed() {
    // the time is the one the constant documents
    assert_eq!(
        migrated_at(),
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    );
    // asking twice gives the same instant
    assert_eq!(migrated_at(), migrated_at());
}
