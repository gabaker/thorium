//! Unit tests for workload specs, their `Image` conversion, and the `Image` projection

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Debug;
use uuid::Uuid;

use super::{
    BareMetalEnvironment, DroppedImageField, ExecutionSettings, JobDataSettings, K8sEnvironment,
    KvmEnvironment, SchedulingSettings, ToolDependencies, WindowsEnvironment, WorkloadEnvironment,
    WorkloadSpec,
};
use crate::models::images::GenericBan;
use crate::models::{
    ArgStrategy, AutoTag, AutoTagLogic, ChildFilters, Cleanup, Dependencies,
    DependencyPassStrategy, Image, ImageArgs, ImageBan, ImageBanKind, ImageLifetime, ImageRequest,
    ImageScaler, Kvm, OutputCollection, OutputDisplayType, Resources, ResourcesRequest,
    SecurityContext, SpawnLimits, ToolVersion, Volume, VolumeTypes,
};

/// Every scaler an image can use
const SCALERS: [ImageScaler; 5] = [
    ImageScaler::K8s,
    ImageScaler::BareMetal,
    ImageScaler::Windows,
    ImageScaler::Kvm,
    ImageScaler::External,
];

/// Build non-default resources
fn resources() -> Resources {
    Resources {
        cpu: 2000,
        memory: 4096,
        ephemeral_storage: 1024,
        worker_slots: 1,
        nvidia_gpu: 1,
        amd_gpu: 0,
        burstable: crate::models::BurstableResources {
            cpu: 500,
            memory: 512,
        },
    }
}

/// Build a non-default security context
fn security_context() -> SecurityContext {
    SecurityContext {
        user: Some(1000),
        group: Some(1001),
        allow_privilege_escalation: true,
    }
}

/// Build non-default job args
fn args() -> ImageArgs {
    ImageArgs {
        entrypoint: Some(vec!["/bin/tool".into()]),
        command: Some(vec!["--fast".into()]),
        reaction: Some("--reaction".into()),
        repo: None,
        commit: None,
        output: ArgStrategy::Kwarg("--out".into()),
        output_files: ArgStrategy::Append,
    }
}

/// Build non-default dependency settings
fn dependencies() -> Dependencies {
    // pass samples in a directory under a kwarg and download tags
    let mut dependencies = Dependencies::default();
    dependencies.samples.kwarg = Some("--sample".into());
    dependencies.samples.strategy = DependencyPassStrategy::Directory;
    dependencies.tags.enabled = true;
    dependencies
}

/// Build non-default output collection settings
fn output_collection() -> OutputCollection {
    // tag a family from results
    let auto_tag = HashMap::from([(
        "family".to_owned(),
        AutoTag {
            logic: AutoTagLogic::Greater(json!(0.5)),
            key: Some("Family".into()),
        },
    )]);
    // ingest children as a filesystem into one group
    OutputCollection {
        as_filesystem: true,
        groups: vec!["corn".into()],
        auto_tag,
        ..OutputCollection::default()
    }
}

/// Build non-default child filters
fn child_filters() -> ChildFilters {
    ChildFilters {
        mime: BTreeSet::from(["text/plain".to_owned()]),
        file_name: BTreeSet::from(["a.txt".to_owned()]),
        file_extension: BTreeSet::from(["txt".to_owned()]),
        submit_non_matches: true,
    }
}

/// Build a non-default clean up script
fn clean_up() -> Cleanup {
    Cleanup {
        job_id: ArgStrategy::Kwarg("--job".into()),
        results: ArgStrategy::Append,
        result_files_dir: ArgStrategy::None,
        script: "/clean.sh".into(),
    }
}

/// Build non-default kvm settings
fn kvm() -> Kvm {
    Kvm {
        xml: "/golden/vm.xml".into(),
        qcow2: "/golden/vm.qcow2".into(),
    }
}

/// Build a host path volume
fn volume() -> Volume {
    Volume {
        name: "scratch".into(),
        archetype: VolumeTypes::HostPath,
        mount_path: "/scratch".into(),
        sub_path: None,
        read_only: true,
        kustomize: false,
        host_path: Some(crate::models::HostPath {
            path: "/mnt/scratch".into(),
            path_type: None,
        }),
        config_map: None,
        secret: None,
        nfs: None,
    }
}

/// Build an image with every field set to a non-default value
///
/// # Arguments
///
/// * `scaler` - The scaler the image uses
fn full_image(scaler: ImageScaler) -> Image {
    // ban the image so its bans are never carried into a spec
    let ban_id = Uuid::new_v4();
    let ban = ImageBan {
        id: ban_id,
        time_banned: chrono::Utc::now(),
        ban_kind: ImageBanKind::Generic(GenericBan {
            msg: "banned".into(),
        }),
    };
    // set every other field to a non-default value
    Image {
        group: "corn".into(),
        name: "harvester".into(),
        creator: "alice".into(),
        version: Some(ToolVersion::Custom("1.0".into())),
        scaler,
        image: Some("repo/harvester:1.0".into()),
        lifetime: Some(ImageLifetime {
            counter: "jobs".into(),
            amount: 5,
        }),
        timeout: Some(300),
        resources: resources(),
        spawn_limit: SpawnLimits::Basic(3),
        env: HashMap::from([
            ("SECRET".to_owned(), Some("x".to_owned())),
            ("EMPTY".to_owned(), None),
        ]),
        runtime: 42.0,
        volumes: vec![volume()],
        args: args(),
        modifiers: Some("/modifiers".into()),
        description: Some("harvests corn".into()),
        security_context: security_context(),
        used_by: vec!["pipeline".into()],
        collect_logs: true,
        generator: true,
        dependencies: dependencies(),
        display_type: OutputDisplayType::Table,
        output_collection: output_collection(),
        child_filters: child_filters(),
        clean_up: Some(clean_up()),
        kvm: Some(kvm()),
        bans: HashMap::from([(ban_id, ban)]),
        network_policies: HashSet::from(["allow-egress".to_owned()]),
    }
}

/// Build an image that only sets the fields its scaler's environment reads
///
/// Fields an environment never reads, and fields a spec never holds, are left at the values a
/// projection fills in, so the image round-trips through a spec unchanged.
///
/// # Arguments
///
/// * `scaler` - The scaler the image uses
fn environment_image(scaler: ImageScaler) -> Image {
    // start from a fully populated image without the fields no spec holds
    let mut image = full_image(scaler);
    image.creator = String::new();
    image.version = None;
    image.description = None;
    image.used_by = Vec::new();
    image.bans = HashMap::new();
    image.runtime = 600.0;
    // clear the fields this environment never reads
    let defaults = super::SchedulingSettings::default();
    match scaler {
        ImageScaler::K8s => {
            image.clean_up = None;
            image.kvm = None;
        }
        ImageScaler::BareMetal => {
            image.image = None;
            image.env = HashMap::new();
            image.volumes = Vec::new();
            image.network_policies = HashSet::new();
            image.kvm = None;
        }
        ImageScaler::Windows => {
            image.env = HashMap::new();
            image.volumes = Vec::new();
            image.network_policies = HashSet::new();
            image.security_context = SecurityContext::default();
            image.clean_up = None;
            image.kvm = None;
        }
        ImageScaler::Kvm => {
            image.image = None;
            image.env = HashMap::new();
            image.volumes = Vec::new();
            image.network_policies = HashSet::new();
            image.security_context = SecurityContext::default();
            image.clean_up = None;
        }
        ImageScaler::External => {
            image.image = None;
            image.resources = defaults.resources;
            image.spawn_limit = defaults.spawn_limit;
            image.lifetime = defaults.lifetime;
            image.env = HashMap::new();
            image.volumes = Vec::new();
            image.network_policies = HashSet::new();
            image.security_context = SecurityContext::default();
            image.clean_up = None;
            image.kvm = None;
        }
    }
    image
}

/// The scheduling settings of a fully populated image
fn full_scheduling() -> SchedulingSettings {
    SchedulingSettings {
        resources: resources(),
        spawn_limit: SpawnLimits::Basic(3),
        lifetime: Some(ImageLifetime {
            counter: "jobs".into(),
            amount: 5,
        }),
    }
}

/// The execution settings of a fully populated image
fn full_execution() -> ExecutionSettings {
    ExecutionSettings {
        args: args(),
        timeout: Some(300),
        collect_logs: true,
        generator: true,
        modifiers: Some("/modifiers".into()),
    }
}

/// The job data settings of a fully populated image
fn full_data() -> JobDataSettings {
    JobDataSettings {
        dependencies: ToolDependencies::from_image(&dependencies()),
        output_collection: output_collection(),
        child_filters: child_filters(),
        display_type: OutputDisplayType::Table,
    }
}

/// A populated K8s environment
fn full_k8s() -> K8sEnvironment {
    K8sEnvironment {
        image: "repo/harvester:1.0".into(),
        scheduling: full_scheduling(),
        env: BTreeMap::from([
            ("EMPTY".to_owned(), None),
            ("SECRET".to_owned(), Some("x".to_owned())),
        ]),
        volumes: vec![volume()],
        security_context: security_context(),
        network_policies: BTreeSet::from(["allow-egress".to_owned()]),
    }
}

/// Assert that a value survives a serde JSON round trip unchanged
///
/// # Arguments
///
/// * `value` - The value to round trip
fn assert_round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    // serialize the value and read it back
    let serialized = serde_json::to_string(value).unwrap();
    let deserialized: T = serde_json::from_str(&serialized).unwrap();
    // nothing changed on the way
    assert_eq!(&deserialized, value);
}

/// Serialize an image to a comparable JSON value
///
/// # Arguments
///
/// * `image` - The image to serialize
fn image_value(image: &Image) -> Value {
    // images have no `PartialEq`, so they're compared as JSON
    serde_json::to_value(image).unwrap()
}

/// A spec's dependency settings name tools by id: image names aren't carried into a spec, a
/// projection names no images, and a spec naming images in a tool list is refused
#[test]
fn dependency_settings_name_tools() {
    // an image restricting every list to other images
    let mut image = full_image(ImageScaler::K8s);
    image.dependencies.results.images = vec!["plant".into()];
    image.dependencies.children.images = vec!["grow".into()];
    image.dependencies.filesystems.images = vec!["unpack".into()];
    image.dependencies.filesystems.enabled = true;
    // the spec keeps every other setting and names no tools
    let mut spec = WorkloadSpec::from_image(&image).unwrap().spec;
    let deps = &spec.data.dependencies;
    assert_eq!(deps.tools(), Vec::<Uuid>::new());
    assert!(deps.filesystems.enabled);
    assert_eq!(deps.results.location, image.dependencies.results.location);
    // listed tools are each named once, sorted
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    spec.data.dependencies.results.tools = vec![second];
    spec.data.dependencies.children.tools = vec![first];
    spec.data.dependencies.filesystems.tools = vec![second, first];
    assert_eq!(spec.data.dependencies.tools(), vec![first, second]);
    // a projection names no images
    let projected = spec.project_image("corn", "harvester", 600.0);
    assert_eq!(projected.dependencies.results.images, Vec::<String>::new());
    assert_eq!(projected.dependencies.children.images, Vec::<String>::new());
    assert_eq!(
        projected.dependencies.filesystems.images,
        Vec::<String>::new()
    );
    assert!(projected.dependencies.filesystems.enabled);
    // the tool lists serialize as `tools`
    let value = serde_json::to_value(&spec.data.dependencies).unwrap();
    assert_eq!(value["results"]["tools"], json!([second.to_string()]));
    // a list of image names in a tool list's place is refused
    for kind in ["results", "children", "filesystems"] {
        // the same settings parse without the image names
        let mut settings = json!({ "enabled": true });
        if kind == "results" {
            settings = json!({});
        }
        let raw = json!({ kind: settings.clone() });
        assert!(
            serde_json::from_value::<ToolDependencies>(raw).is_ok(),
            "{kind}"
        );
        settings["images"] = json!(["plant"]);
        let raw = json!({ kind: settings });
        assert!(
            serde_json::from_value::<ToolDependencies>(raw).is_err(),
            "{kind} accepted image names"
        );
    }
}

/// Every workload struct and enum round-trips through serde JSON
#[test]
fn serde_round_trip() {
    // the settings shared by every environment
    assert_round_trip(&full_scheduling());
    assert_round_trip(&full_execution());
    assert_round_trip(&full_data());
    // every environment struct
    let k8s = full_k8s();
    let bare_metal = BareMetalEnvironment {
        scheduling: full_scheduling(),
        security_context: security_context(),
        clean_up: Some(clean_up()),
    };
    let windows = WindowsEnvironment {
        image: "repo/win:1.0".into(),
        scheduling: full_scheduling(),
    };
    let kvm_env = KvmEnvironment {
        scheduling: full_scheduling(),
        kvm: Some(kvm()),
    };
    assert_round_trip(&k8s);
    assert_round_trip(&bare_metal);
    assert_round_trip(&windows);
    assert_round_trip(&kvm_env);
    // every environment variant inside a full spec
    for environment in [
        WorkloadEnvironment::K8s(k8s),
        WorkloadEnvironment::BareMetal(bare_metal),
        WorkloadEnvironment::Windows(windows),
        WorkloadEnvironment::Kvm(kvm_env),
        WorkloadEnvironment::External,
    ] {
        assert_round_trip(&environment);
        assert_round_trip(&WorkloadSpec {
            environment,
            execution: full_execution(),
            data: full_data(),
        });
    }
    // the dropped field list
    assert_round_trip(&vec![
        DroppedImageField::Image,
        DroppedImageField::Resources,
        DroppedImageField::SpawnLimit,
        DroppedImageField::Lifetime,
        DroppedImageField::Env(vec!["SECRET".into()]),
        DroppedImageField::Volumes,
        DroppedImageField::NetworkPolicies,
        DroppedImageField::SecurityContext,
        DroppedImageField::CleanUp,
        DroppedImageField::Kvm,
    ]);
    // an env entry with no value is kept as a declared variable
    let value = serde_json::to_value(full_k8s()).unwrap();
    assert_eq!(value["env"], json!({"EMPTY": null, "SECRET": "x"}));
}

/// Converting an image picks the environment of its scaler and keeps every field a reader uses
#[test]
fn conversion_picks_environment() {
    for scaler in SCALERS {
        // convert a fully populated image
        let image = full_image(scaler);
        let spec = WorkloadSpec::from_image(&image).unwrap().spec;
        // the environment matches the scaler
        assert_eq!(spec.environment.scaler(), scaler);
        // execution and job data settings are kept in every environment
        assert_eq!(spec.execution.args, image.args);
        assert_eq!(spec.execution.timeout, image.timeout);
        assert_eq!(spec.execution.generator, image.generator);
        assert_eq!(
            spec.data.dependencies,
            ToolDependencies::from_image(&image.dependencies)
        );
        assert_eq!(spec.data.output_collection, image.output_collection);
        assert_eq!(spec.data.child_filters, image.child_filters);
        assert_eq!(spec.data.display_type, image.display_type);
        // the environment keeps every field it reads
        let scheduling = match &spec.environment {
            WorkloadEnvironment::K8s(env) => {
                assert_eq!(Some(&env.image), image.image.as_ref());
                assert_eq!(env.env.len(), image.env.len());
                for (key, value) in &image.env {
                    assert_eq!(env.env.get(key), Some(value));
                }
                assert_eq!(env.volumes, image.volumes);
                assert_eq!(env.security_context, image.security_context);
                assert_eq!(
                    env.network_policies,
                    image
                        .network_policies
                        .iter()
                        .cloned()
                        .collect::<BTreeSet<_>>()
                );
                Some(&env.scheduling)
            }
            WorkloadEnvironment::BareMetal(env) => {
                assert_eq!(env.security_context, image.security_context);
                assert_eq!(env.clean_up, image.clean_up);
                Some(&env.scheduling)
            }
            WorkloadEnvironment::Windows(env) => {
                assert_eq!(Some(&env.image), image.image.as_ref());
                Some(&env.scheduling)
            }
            WorkloadEnvironment::Kvm(env) => {
                assert_eq!(env.kvm, image.kvm);
                Some(&env.scheduling)
            }
            WorkloadEnvironment::External => None,
        };
        // every scheduled environment keeps the scheduling fields
        if let Some(scheduling) = scheduling {
            assert_eq!(scheduling.resources, image.resources);
            assert_eq!(scheduling.spawn_limit, image.spawn_limit);
            assert_eq!(scheduling.lifetime, image.lifetime);
        }
    }
}

/// A K8s image converts to exactly the K8s spec the conversion rules describe
#[test]
fn conversion_k8s() {
    // convert a fully populated K8s image
    let conversion = WorkloadSpec::from_image(&full_image(ImageScaler::K8s)).unwrap();
    // the spec holds every K8s field and the shared settings
    let expected = WorkloadSpec {
        environment: WorkloadEnvironment::K8s(full_k8s()),
        execution: full_execution(),
        data: full_data(),
    };
    assert_eq!(conversion.spec, expected);
    // a K8s image without an image reference is rejected
    let mut image = full_image(ImageScaler::K8s);
    image.image = None;
    let error = WorkloadSpec::from_image(&image).unwrap_err();
    assert!(error.msg.contains("image"), "{}", error.msg);
    // an empty image reference is rejected too
    image.image = Some(String::new());
    assert!(WorkloadSpec::from_image(&image).is_err());
}

/// A `BareMetal` image converts to exactly the `BareMetal` spec the conversion rules describe
#[test]
fn conversion_bare_metal() {
    // convert a fully populated BareMetal image
    let conversion = WorkloadSpec::from_image(&full_image(ImageScaler::BareMetal)).unwrap();
    // the spec holds the security context, the clean up script, and the shared settings
    let expected = WorkloadSpec {
        environment: WorkloadEnvironment::BareMetal(BareMetalEnvironment {
            scheduling: full_scheduling(),
            security_context: security_context(),
            clean_up: Some(clean_up()),
        }),
        execution: full_execution(),
        data: full_data(),
    };
    assert_eq!(conversion.spec, expected);
    // a BareMetal image needs no image reference
    let mut image = full_image(ImageScaler::BareMetal);
    image.image = None;
    assert_eq!(WorkloadSpec::from_image(&image).unwrap().spec, expected);
}

/// A Windows image converts to exactly the Windows spec the conversion rules describe
#[test]
fn conversion_windows() {
    // convert a fully populated Windows image
    let conversion = WorkloadSpec::from_image(&full_image(ImageScaler::Windows)).unwrap();
    // the spec holds the image reference and the shared settings
    let expected = WorkloadSpec {
        environment: WorkloadEnvironment::Windows(WindowsEnvironment {
            image: "repo/harvester:1.0".into(),
            scheduling: full_scheduling(),
        }),
        execution: full_execution(),
        data: full_data(),
    };
    assert_eq!(conversion.spec, expected);
    // a Windows image without an image reference is rejected
    let mut image = full_image(ImageScaler::Windows);
    image.image = None;
    let error = WorkloadSpec::from_image(&image).unwrap_err();
    assert!(error.msg.contains("image"), "{}", error.msg);
}

/// A Kvm image converts to exactly the Kvm spec the conversion rules describe
#[test]
fn conversion_kvm() {
    // convert a fully populated Kvm image
    let conversion = WorkloadSpec::from_image(&full_image(ImageScaler::Kvm)).unwrap();
    // the spec holds the kvm settings and the shared settings
    let expected = WorkloadSpec {
        environment: WorkloadEnvironment::Kvm(KvmEnvironment {
            scheduling: full_scheduling(),
            kvm: Some(kvm()),
        }),
        execution: full_execution(),
        data: full_data(),
    };
    assert_eq!(conversion.spec, expected);
    // a Kvm image without kvm settings still converts
    let mut image = full_image(ImageScaler::Kvm);
    image.kvm = None;
    let spec = WorkloadSpec::from_image(&image).unwrap().spec;
    assert_eq!(
        spec.environment,
        WorkloadEnvironment::Kvm(KvmEnvironment {
            scheduling: full_scheduling(),
            kvm: None,
        })
    );
}

/// An External image converts to exactly the External spec the conversion rules describe
#[test]
fn conversion_external() {
    // convert a fully populated External image
    let conversion = WorkloadSpec::from_image(&full_image(ImageScaler::External)).unwrap();
    // the spec holds only the shared settings
    let expected = WorkloadSpec {
        environment: WorkloadEnvironment::External,
        execution: full_execution(),
        data: full_data(),
    };
    assert_eq!(conversion.spec, expected);
}

/// Converting drops the fields an environment never reads and lists only the non-default ones
#[test]
fn conversion_drops() {
    // a BareMetal image with env, a volume, and a network policy
    let mut image = environment_image(ImageScaler::BareMetal);
    image.env = HashMap::from([("SECRET".to_owned(), Some("x".to_owned()))]);
    image.volumes = vec![volume()];
    image.network_policies = HashSet::from(["allow-egress".to_owned()]);
    let conversion = WorkloadSpec::from_image(&image).unwrap();
    assert_eq!(
        conversion.dropped,
        vec![
            DroppedImageField::Env(vec!["SECRET".into()]),
            DroppedImageField::Volumes,
            DroppedImageField::NetworkPolicies,
        ]
    );
    // the dropped list names the variable but never holds its value
    let serialized = serde_json::to_string(&conversion.dropped).unwrap();
    assert!(serialized.contains("SECRET"));
    assert!(!serialized.contains("\"x\""));
    // each entry displays as its field name, with the variable names for env
    let names = conversion
        .dropped
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<String>>();
    assert_eq!(names, vec!["env (SECRET)", "volumes", "network_policies"]);
    assert_eq!(DroppedImageField::Env(vec!["SECRET".into()]).field(), "env");
    // the same fields on a K8s image are all kept
    let mut k8s = image.clone();
    k8s.scaler = ImageScaler::K8s;
    k8s.image = Some("repo/harvester:1.0".into());
    k8s.clean_up = None;
    let conversion = WorkloadSpec::from_image(&k8s).unwrap();
    assert!(conversion.dropped.is_empty(), "{:?}", conversion.dropped);
    // a field left at its default isn't listed
    let conversion = WorkloadSpec::from_image(&environment_image(ImageScaler::BareMetal)).unwrap();
    assert!(conversion.dropped.is_empty(), "{:?}", conversion.dropped);
    for scaler in SCALERS {
        let conversion = WorkloadSpec::from_image(&environment_image(scaler)).unwrap();
        assert!(
            conversion.dropped.is_empty(),
            "{scaler}: {:?}",
            conversion.dropped
        );
    }
    // a fully populated image lists exactly what each environment doesn't read
    let expected = [
        (
            ImageScaler::K8s,
            vec![DroppedImageField::CleanUp, DroppedImageField::Kvm],
        ),
        (
            ImageScaler::BareMetal,
            vec![
                DroppedImageField::Image,
                DroppedImageField::Env(vec!["EMPTY".into(), "SECRET".into()]),
                DroppedImageField::Volumes,
                DroppedImageField::NetworkPolicies,
                DroppedImageField::Kvm,
            ],
        ),
        (
            ImageScaler::Windows,
            vec![
                DroppedImageField::Env(vec!["EMPTY".into(), "SECRET".into()]),
                DroppedImageField::Volumes,
                DroppedImageField::NetworkPolicies,
                DroppedImageField::SecurityContext,
                DroppedImageField::CleanUp,
                DroppedImageField::Kvm,
            ],
        ),
        (
            ImageScaler::Kvm,
            vec![
                DroppedImageField::Image,
                DroppedImageField::Env(vec!["EMPTY".into(), "SECRET".into()]),
                DroppedImageField::Volumes,
                DroppedImageField::NetworkPolicies,
                DroppedImageField::SecurityContext,
                DroppedImageField::CleanUp,
            ],
        ),
        (
            ImageScaler::External,
            vec![
                DroppedImageField::Image,
                DroppedImageField::Resources,
                DroppedImageField::SpawnLimit,
                DroppedImageField::Lifetime,
                DroppedImageField::Env(vec!["EMPTY".into(), "SECRET".into()]),
                DroppedImageField::Volumes,
                DroppedImageField::NetworkPolicies,
                DroppedImageField::SecurityContext,
                DroppedImageField::CleanUp,
                DroppedImageField::Kvm,
            ],
        ),
    ];
    for (scaler, dropped) in expected {
        let conversion = WorkloadSpec::from_image(&full_image(scaler)).unwrap();
        assert_eq!(conversion.dropped, dropped, "{scaler}");
    }
}

/// An External image with the resources every image is created with drops no resources
#[test]
fn external_default_resources() {
    // the resources an image gets when its request names none, and zero resources from JSON
    let created = Resources::from(ResourcesRequest::default());
    let zero: Resources = serde_json::from_value(json!({"cpu": 0, "memory": 0})).unwrap();
    for resources in [created, zero, Resources::default()] {
        // an External image with only these resources set
        let mut image = environment_image(ImageScaler::External);
        image.resources = resources;
        let conversion = WorkloadSpec::from_image(&image).unwrap();
        assert!(
            conversion.dropped.is_empty(),
            "{resources:?}: {:?}",
            conversion.dropped
        );
    }
    // any other resources are still listed, whatever their worker slots
    let mut image = environment_image(ImageScaler::External);
    image.resources = Resources::from(ResourcesRequest::default());
    image.resources.cpu = 2000;
    let conversion = WorkloadSpec::from_image(&image).unwrap();
    assert_eq!(conversion.dropped, vec![DroppedImageField::Resources]);
}

/// The projection sets the scaler, group, name, and runtime and matches the equivalent image
#[test]
fn projection_matches_image() {
    for scaler in SCALERS {
        // convert an image holding only what its environment reads
        let image = environment_image(scaler);
        let spec = WorkloadSpec::from_image(&image).unwrap().spec;
        // project it back with the image's group, name, and runtime
        let projected = spec.project_image(&image.group, &image.name, image.runtime);
        assert_eq!(projected.scaler, scaler);
        assert_eq!(projected.group, "corn");
        assert_eq!(projected.name, "harvester");
        assert!((projected.runtime - 600.0).abs() < f64::EPSILON);
        assert_eq!(projected.resources, image.resources);
        assert_eq!(projected.security_context, image.security_context);
        assert_eq!(projected.args, image.args);
        assert_eq!(projected.clean_up, image.clean_up);
        assert_eq!(projected.dependencies, image.dependencies);
        assert_eq!(projected.output_collection, image.output_collection);
        // the projection is the image the spec came from
        assert_eq!(image_value(&projected), image_value(&image), "{scaler}");
        // a projection never carries bans
        assert!(projected.bans.is_empty());
    }
    // the group, name, and runtime come from the arguments
    let spec = WorkloadSpec::from_image(&full_image(ImageScaler::Kvm))
        .unwrap()
        .spec;
    let projected = spec.project_image("B", "winvm", 12.5);
    assert_eq!(projected.group, "B");
    assert_eq!(projected.name, "winvm");
    assert!((projected.runtime - 12.5).abs() < f64::EPSILON);
    assert_eq!(projected.kvm, Some(kvm()));
}

/// The job info of a spec carries its generator flag and scaler
#[test]
fn job_info_from_spec() {
    for scaler in SCALERS {
        // build a generator spec for this environment
        let mut spec = WorkloadSpec::from_image(&full_image(scaler)).unwrap().spec;
        let info = spec.job_info();
        assert!(info.generator);
        assert_eq!(info.scaler, scaler);
        // a spec that isn't a generator gives jobs that aren't
        spec.execution.generator = false;
        assert!(!spec.job_info().generator);
    }
}

/// A job built from a generator spec is a generator
#[cfg(feature = "api")]
#[test]
fn job_from_generator_spec() {
    // build a reaction with one stage
    let reaction = crate::models::Reaction {
        id: Uuid::new_v4(),
        group: "corn".into(),
        creator: "alice".into(),
        pipeline: "harvest".into(),
        status: crate::models::ReactionStatus::Created,
        current_stage: 0,
        current_stage_progress: 0,
        current_stage_length: 1,
        args: HashMap::new(),
        sla: chrono::Utc::now(),
        jobs: Vec::new(),
        tags: Vec::new(),
        parent: None,
        sub_reactions: 0,
        completed_sub_reactions: 0,
        generators: Vec::new(),
        samples: Vec::new(),
        ephemeral: Vec::new(),
        parent_ephemeral: HashMap::new(),
        repos: Vec::new(),
        trigger_depth: None,
        has_cache: false,
        source: crate::models::ReactionSource::Pipeline {
            name: "harvest".into(),
            provenance: crate::models::RunProvenance::default(),
        },
        plan: crate::models::ReactionPlan::default(),
        run_token: None,
        run: None,
    };
    // build the job of a generator spec's stage
    let spec = WorkloadSpec::from_image(&full_image(ImageScaler::External))
        .unwrap()
        .spec;
    let stage = "harvester".to_owned();
    let info = HashMap::from([(&stage, spec.job_info())]);
    let job = crate::models::RawJob::build(&reaction, &stage, chrono::Utc::now(), &info).unwrap();
    assert!(job.generator);
    assert_eq!(job.scaler, ImageScaler::External);
}

/// External is a unit variant and an External workload's execution and data round-trip
#[test]
fn external_environment() {
    // External serializes as its bare name
    assert_eq!(
        serde_json::to_value(WorkloadEnvironment::External).unwrap(),
        json!("External")
    );
    let parsed: WorkloadEnvironment = serde_json::from_value(json!("External")).unwrap();
    assert_eq!(parsed, WorkloadEnvironment::External);
    // External carries no settings
    assert!(serde_json::from_value::<WorkloadEnvironment>(json!({"External": {}})).is_err());
    // an External workload's execution and data round-trip
    let spec = WorkloadSpec {
        environment: WorkloadEnvironment::External,
        execution: full_execution(),
        data: full_data(),
    };
    assert_round_trip(&spec);
    let value = serde_json::to_value(&spec).unwrap();
    assert_eq!(value["environment"], json!("External"));
    // a spec naming only External defaults its execution and data
    let minimal: WorkloadSpec = serde_json::from_value(json!({"environment": "External"})).unwrap();
    assert_eq!(minimal, WorkloadSpec::default());
    // a spec with no environment is refused
    assert!(serde_json::from_value::<WorkloadSpec>(json!({})).is_err());
}

/// Every workload type implements `DigestValue` and `Default`, and a default spec prunes to `{}`
#[cfg(any(feature = "api", feature = "client"))]
#[test]
fn digest_defaults() {
    use crate::models::DigestValue;
    /// Get a type's default digest value, which needs both traits
    ///
    /// # Arguments
    ///
    /// * `_name` - The type's name
    fn default_digest<T: Default + DigestValue>(_name: &str) -> Value {
        // digest the type's default
        T::default().digest_value()
    }
    // every struct's default prunes to an empty object
    assert_eq!(default_digest::<WorkloadSpec>("WorkloadSpec"), json!({}));
    assert_eq!(
        default_digest::<SchedulingSettings>("SchedulingSettings"),
        json!({})
    );
    assert_eq!(
        default_digest::<K8sEnvironment>("K8sEnvironment"),
        json!({})
    );
    assert_eq!(
        default_digest::<BareMetalEnvironment>("BareMetalEnvironment"),
        json!({})
    );
    assert_eq!(
        default_digest::<WindowsEnvironment>("WindowsEnvironment"),
        json!({})
    );
    assert_eq!(
        default_digest::<KvmEnvironment>("KvmEnvironment"),
        json!({})
    );
    assert_eq!(
        default_digest::<ExecutionSettings>("ExecutionSettings"),
        json!({})
    );
    assert_eq!(
        default_digest::<JobDataSettings>("JobDataSettings"),
        json!({})
    );
    // the environment enum defaults to External
    assert_eq!(
        default_digest::<WorkloadEnvironment>("WorkloadEnvironment"),
        json!("External")
    );
    // a populated spec keeps its environment and only its non-default fields
    let spec = WorkloadSpec {
        environment: WorkloadEnvironment::Windows(WindowsEnvironment {
            image: "repo/win:1.0".into(),
            scheduling: SchedulingSettings::default(),
        }),
        execution: ExecutionSettings {
            timeout: Some(30),
            ..ExecutionSettings::default()
        },
        data: JobDataSettings::default(),
    };
    assert_eq!(
        spec.digest_value(),
        json!({
            "environment": {"Windows": {"image": "repo/win:1.0"}},
            "execution": {"timeout": 30},
        })
    );
    // an env entry with no value is kept in the digest
    let k8s = K8sEnvironment {
        env: BTreeMap::from([("FOO".to_owned(), None)]),
        ..K8sEnvironment::default()
    };
    assert_eq!(k8s.digest_value(), json!({"env": {"FOO": null}}));
}

/// Store an image request the way the API does: resolve its resources and security context and
/// copy every other field
///
/// # Arguments
///
/// * `request` - The image request to store
fn stored(request: &ImageRequest) -> Image {
    // the API gives a request without resources the default request and one worker slot
    let resources = Resources::from(request.resources.clone());
    Image {
        group: request.group.clone(),
        name: request.name.clone(),
        creator: "alice".into(),
        version: request.version.clone(),
        scaler: request.scaler,
        image: request.image.clone(),
        lifetime: request.lifetime.clone(),
        timeout: request.timeout,
        resources,
        spawn_limit: request.spawn_limit.clone(),
        env: request.env.clone(),
        runtime: 600.0,
        volumes: request.volumes.clone(),
        args: request.args.clone(),
        modifiers: request.modifiers.clone(),
        description: request.description.clone(),
        security_context: request.security_context.clone().unwrap_or_default(),
        used_by: Vec::new(),
        collect_logs: request.collect_logs,
        generator: request.generator,
        dependencies: request.dependencies.clone(),
        display_type: request.display_type,
        output_collection: request.output_collection.clone(),
        child_filters: request.child_filters.clone(),
        clean_up: request.clean_up.clone(),
        kvm: request.kvm.clone(),
        bans: HashMap::new(),
        network_policies: request.network_policies.clone(),
    }
}

/// A request for each scaler that sets every field to a non-default value converts to the spec
/// and the dropped fields its stored image converts to
#[test]
fn full_requests_convert_like_their_stored_images() {
    for scaler in SCALERS {
        // build the request a fully populated image would have been created from
        let request = ImageRequest::from(full_image(scaler));
        // both conversions agree on the spec and on what is dropped
        let from_request = WorkloadSpec::from_image_request(&request).unwrap();
        let from_stored = WorkloadSpec::from_image(&stored(&request)).unwrap();
        assert_eq!(from_request, from_stored, "{scaler}");
        // every environment drops something from a fully populated request
        assert!(!from_request.dropped.is_empty(), "{scaler}");
    }
}

/// A request that leaves resources and the security context unset gets the defaults the API
/// stores, so it converts like its stored image
#[test]
fn minimal_requests_convert_like_their_stored_images() {
    for scaler in SCALERS {
        // a request with only its group, name, scaler, and container image
        let request = ImageRequest::new("corn", "harvester")
            .scaler(scaler)
            .image("repo/harvester:1.0");
        let from_request = WorkloadSpec::from_image_request(&request).unwrap();
        let from_stored = WorkloadSpec::from_image(&stored(&request)).unwrap();
        assert_eq!(from_request, from_stored, "{scaler}");
    }
    // the default resource request and no security context are what a K8s spec holds
    let request = ImageRequest::new("corn", "harvester").image("repo/harvester:1.0");
    let spec = WorkloadSpec::from_image_request(&request).unwrap().spec;
    let scheduling = spec.environment.scheduling().unwrap();
    assert_eq!(scheduling.resources.cpu, 1000);
    assert_eq!(scheduling.resources.memory, 4096);
    assert_eq!(scheduling.resources.worker_slots, 1);
    let WorkloadEnvironment::K8s(k8s) = spec.environment else {
        panic!("not a K8s environment");
    };
    assert_eq!(k8s.security_context, SecurityContext::default());
}

/// A container environment without an image to run is refused with the same error for a request
/// as for the stored image
#[test]
fn requests_without_an_image_fail_like_their_stored_images() {
    for scaler in [ImageScaler::K8s, ImageScaler::Windows] {
        // neither a missing nor an empty container image can run
        for image in [None, Some(String::new())] {
            let mut request = ImageRequest::new("corn", "harvester").scaler(scaler);
            request.image = image;
            let from_request = WorkloadSpec::from_image_request(&request).unwrap_err();
            let from_stored = WorkloadSpec::from_image(&stored(&request)).unwrap_err();
            assert_eq!(from_request.msg, from_stored.msg);
        }
    }
    // environments that don't run a container image need none
    for scaler in [
        ImageScaler::BareMetal,
        ImageScaler::Kvm,
        ImageScaler::External,
    ] {
        let request = ImageRequest::new("corn", "harvester").scaler(scaler);
        assert!(
            WorkloadSpec::from_image_request(&request).is_ok(),
            "{scaler}"
        );
    }
}

/// The dropped fields of a request name the same fields as its stored image, and env variables
/// by name only
#[test]
fn request_dropped_fields_match_the_stored_image() {
    for scaler in SCALERS {
        let request = ImageRequest::from(full_image(scaler));
        let from_request = WorkloadSpec::from_image_request(&request).unwrap().dropped;
        let from_stored = WorkloadSpec::from_image(&stored(&request)).unwrap().dropped;
        assert_eq!(from_request, from_stored, "{scaler}");
        // no env value appears anywhere in what is reported
        let rendered = from_request
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
            .join(", ");
        assert!(!rendered.contains("\"x\""), "{scaler}: {rendered}");
    }
    // a BareMetal request drops the container image, env, and volumes
    let request = ImageRequest::from(full_image(ImageScaler::BareMetal));
    let dropped = WorkloadSpec::from_image_request(&request).unwrap().dropped;
    assert!(dropped.contains(&DroppedImageField::Image));
    assert!(dropped.contains(&DroppedImageField::Env(vec![
        "EMPTY".to_owned(),
        "SECRET".to_owned()
    ])));
    assert!(dropped.contains(&DroppedImageField::Volumes));
}
