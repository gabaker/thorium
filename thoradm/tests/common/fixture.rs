//! A fixture that touches every kind of resource Thorium stores
//!
//! The seed writes through the API wherever it can, so every table, Redis key family, and bucket
//! the API's normal operation creates exists afterwards: users (one sent a verification email) and
//! scoped tokens, groups and their role sets (one allowing no repos), images with bans and network
//! policies, a banned pipeline, notifications, samples with tags, comments, and attachments,
//! results with files and entity blobs, repos, entities and associations, a profile icon and an
//! entity image, nodes and workers, reactions with stage logs, and changed system settings. The
//! runtime state of jobs (a sleeping generator, a finished job's time to complete, a reaction's
//! cache) comes from claiming and handling jobs, and the families only LDAP and OAuth logins write
//! (metagroups, aliases, login contexts, registration sessions, and link tokens) come from the
//! API's own writers, since the test cluster has neither.

use super::harness::Harness;
use super::names::Names;
use super::seed;
use openidconnect::{CsrfToken, Nonce, PkceCodeVerifier};
use thorium::Thorium;
use thorium::client::{ClientSettings, ResultsClient, Users};
use thorium::models::backends::db;
use thorium::models::backends::oauth::OAuthRegistrationSession;
use thorium::models::{
    AssociationKind, AssociationRequest, AssociationTarget, Buffer, CommentRequest, EntityKinds,
    EntityRequest, GroupAllowedUpdate, GroupUpdate, HostPathWhitelistUpdate, ImageBan,
    ImageBanKind, ImageBanUpdate, ImageNetworkPolicyUpdate, ImageUpdate, ImageVersion,
    NotificationLevel, NotificationParams, NotificationRequest, OnDiskFile, OriginRequest,
    OutputDisplayType, OutputRequest, PipelineBan, PipelineBanKind, PipelineBanUpdate,
    PipelineUpdate, ReactionCache, Resources, SampleRequest, ScopedTokenRequest,
    SystemSettingsUpdate, SystemSettingsUpdateParams, User, UserCreate, UserRole,
};
use thorium::test_utilities::generators;

/// What the broad seed created, for tests that need to find it again
#[derive(Debug, Clone)]
pub struct Broad {
    /// The main group everything was created in
    pub group: String,
    /// The second group, holding a user and shared data
    pub second_group: String,
    /// The sha256 of the uploaded sample
    pub sample: String,
    /// The name of the image with bans and network policies
    pub image: String,
}

/// Seeds one of every kind of resource through the API
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
///
/// # Panics
///
/// Panics if the API refuses any of the seed's requests.
pub async fn broad(harness: &Harness) -> Broad {
    // name this seed's groups
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("minerva");
    let second_group = names.group("mars");
    seed::group(&client, &group).await;
    seed::group(&client, &second_group).await;
    // a group that allows no repos
    let closed_group = names.group("juno");
    seed::group(&client, &closed_group).await;
    let closed = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_repos(),
        ..GroupUpdate::default()
    };
    client.groups.update(&closed_group, &closed).await.unwrap();
    // seed each area of Thorium's data
    people(harness, &client, &names, &group).await;
    let image = images(&client, &group).await;
    jobs(&client, &names, &group).await;
    let sample = files(harness, &client, &names, &group, &second_group).await;
    entities(&client, &names, &group, &sample).await;
    graphics(harness, &names, &group).await;
    runtime(&client, &names, &group).await;
    Box::pin(logins(harness, &client, &names, &group)).await;
    // change system settings from their defaults
    let settings_update = SystemSettingsUpdate::default()
        .reserved_cpu("1500m")
        .host_path_whitelist(HostPathWhitelistUpdate::default().add_path("/mnt/fixture"));
    client
        .system
        .update_settings(
            &settings_update,
            &SystemSettingsUpdateParams { scan: false },
        )
        .await
        .unwrap();
    Broad {
        group,
        second_group,
        sample,
        image,
    }
}

/// What the clean seed created, for tests that need to find it again
#[derive(Debug, Clone)]
pub struct Clean {
    /// The main group everything was created in
    pub group: String,
    /// The sha256 of the uploaded sample
    pub sample: String,
}

/// Seeds samples with tags, a comment and attachment, a result with files and an entity blob, a
/// repo, entities, and an association through the API, and no Redis state beyond users and groups
/// (no bans, scoped tokens, network policies, or changed settings)
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
///
/// # Panics
///
/// Panics if the API refuses any of the seed's requests.
pub async fn clean(harness: &Harness) -> Clean {
    // two groups the sample is shared by
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("ceres");
    let second_group = names.group("diana");
    seed::group(&client, &group).await;
    seed::group(&client, &second_group).await;
    // the files, results, repos, entities, and associations
    let sample = files(harness, &client, &names, &group, &second_group).await;
    entities(&client, &names, &group, &sample).await;
    Clean { group, sample }
}

/// Creates an analyst, a plain user, and a scoped token
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The group the scoped token is scoped to
async fn people(harness: &Harness, client: &Thorium, names: &Names, group: &str) {
    // create an analyst and a plain user, which fill the user maps and the analyst set
    let settings = ClientSettings::default();
    let key = Some(harness.conf.thorium.secret_key.as_str());
    for (moon, role) in [("himalia", UserRole::Analyst), ("europa", UserRole::User)] {
        let user = UserCreate::new(
            names.user(moon),
            "fixture-password",
            format!("{moon}@example.invalid"),
        )
        .skip_verification()
        .role(role);
        Users::create(&client.host, user, key, &settings)
            .await
            .unwrap();
    }
    // give the admin a scoped token
    client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(names.user("callisto")).group(group))
        .await
        .unwrap();
}

/// Creates network policies and an image that uses them, carries a ban, and has a notification
///
/// # Arguments
///
/// * `client` - An admin client
/// * `group` - The group to create them in
async fn images(client: &Thorium, group: &str) -> String {
    // create network policies and an image
    let policies = generators::network_policies(&[group.to_owned()], 2, client)
        .await
        .unwrap();
    let image = generators::images(group, 1, false, client)
        .await
        .unwrap()
        .remove(0)
        .name;
    // ban the image and give it the policies
    let ban = ImageBan::new(ImageBanKind::generic("fixture ban"));
    let update = ImageUpdate::default()
        .bans(ImageBanUpdate::default().add_ban(ban))
        .network_policies(
            ImageNetworkPolicyUpdate::default()
                .add_policies(policies.into_iter().map(|policy| policy.name)),
        );
    client.images.update(group, &image, &update).await.unwrap();
    // notify about the image
    let notice = NotificationRequest::new("fixture notice", NotificationLevel::Warn);
    client
        .images
        .create_notification(group, &image, &notice, &NotificationParams::default())
        .await
        .unwrap();
    image
}

/// Creates pipelines, reactions, sub reactions, stage logs, a node, and a worker
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The group to create them in
async fn jobs(client: &Thorium, names: &Names, group: &str) {
    // create a pipeline and a reaction for it, which queue jobs
    let (reactions, created) = generators::reactions(group, 1, Some("fixture"), client)
        .await
        .unwrap();
    let pipeline = &reactions[0].pipeline;
    // ban the pipeline
    let ban = PipelineBan::new(PipelineBanKind::generic("fixture ban"));
    let update = PipelineUpdate::default().bans(PipelineBanUpdate::default().add_ban(ban));
    client
        .pipelines
        .update(group, pipeline, &update)
        .await
        .unwrap();
    // notify about the pipeline
    let notice = NotificationRequest::new("fixture notice", NotificationLevel::Error);
    client
        .pipelines
        .create_notification(group, pipeline, &notice, &NotificationParams::default())
        .await
        .unwrap();
    // give the reaction a sub reaction and stage logs
    let parent = created.created[0];
    generators::sub_reactions(group, 1, &parent, client)
        .await
        .unwrap();
    let stage = client.pipelines.get(group, pipeline).await.unwrap().order[0][0].clone();
    client
        .reactions
        .add_stage_logs(group, &parent, &stage, &generators::stage_logs())
        .await
        .unwrap();
    // register a node and a worker on it
    let cluster = names.group("vulcan");
    generators::node(&cluster, "forge", Resources::default(), client)
        .await
        .unwrap();
    let worker = names.user("thebe");
    generators::worker(&cluster, "forge", &worker, group, pipeline, &stage, client)
        .await
        .unwrap();
}

/// Claims and handles jobs of a generator pipeline: one reaction, created with a cache, finishes
/// its first stage and one sleeps as a generator
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The group to create them in
async fn runtime(client: &Thorium, names: &Names, group: &str) {
    // a generator pipeline with two reactions, the first with a cache
    let request = generators::gen_generator_pipe(group, client).await.unwrap();
    let pipeline = client.pipelines.get(group, &request.name).await.unwrap();
    let stage = pipeline.order[0][0].clone();
    let cache = ReactionCache {
        generic: std::collections::HashMap::from([("fixture".to_owned(), "cache".to_owned())]),
        files: Vec::new(),
    };
    let cached = generators::gen_reaction(group, &pipeline, None).cache(cache);
    client.reactions.create(&cached).await.unwrap();
    let sleeper = generators::gen_reaction(group, &pipeline, None);
    client.reactions.create(&sleeper).await.unwrap();
    // a worker claims each reaction's first job
    let cluster = names.group("janus");
    let worker = names.user("pasiphae");
    generators::worker(
        &cluster,
        "gate",
        &worker,
        group,
        &pipeline.name,
        &stage,
        client,
    )
    .await
    .unwrap();
    let jobs = client
        .jobs
        .claim(group, &pipeline.name, &stage, &cluster, "gate", &worker, 2)
        .await
        .unwrap();
    assert_eq!(jobs.len(), 2, "claimed {jobs:?}");
    // one finishes its stage, which records how long it took, and one sleeps as a generator
    client
        .jobs
        .proceed(&jobs[0], &generators::stage_logs(), 2)
        .await
        .unwrap();
    client
        .jobs
        .sleep(&jobs[1].id, "fixture-checkpoint")
        .await
        .unwrap();
}

/// Writes what LDAP and OAuth logins leave in Redis through the API's own writers: a metagroup,
/// an alias, a login context, a registration session, and a link token
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The group to give a metagroup
async fn logins(harness: &Harness, client: &Thorium, names: &Names, group: &str) {
    // the API's shared state for this namespace
    let shared = Box::pin(thorium::utils::Shared::new(harness.conf.clone())).await;
    // a metagroup on the main group, which an LDAP sync would fill
    let mut found = client.groups.get(group).await.unwrap();
    found.users.metagroups.insert(names.group("saturn"));
    let none = std::collections::HashSet::new();
    db::groups::update(&found, &none, &none, &shared)
        .await
        .unwrap();
    // an OAuth alias of a user, saved with the user as an OAuth registration does, then the
    // steps of an OAuth login and an account link
    let provider = "fixture-provider";
    let mut user = User::force_get(&names.user("europa"), &shared)
        .await
        .unwrap();
    user.aliases
        .insert(provider.to_owned(), names.user("sinope"));
    db::users::save(&user, &shared).await.unwrap();
    // a verification email sent to another user, as an email signup leaves it
    db::users::set_verification_token(&names.user("himalia"), &names.user("thyone"), &shared)
        .await
        .unwrap();
    db::oauth::store_context(
        provider,
        &CsrfToken::new(names.user("leda")),
        Nonce::new(names.user("carme")),
        PkceCodeVerifier::new(format!("fixture-verifier-{}", names.suffix)),
        &shared,
    )
    .await
    .unwrap();
    let alias = names.user("ananke");
    let session = OAuthRegistrationSession::new(&alias, format!("{alias}@example.invalid"));
    db::oauth::save_registration_session(provider, &session, &shared)
        .await
        .unwrap();
    db::oauth::save_link_token(
        provider,
        &user.username,
        &names.user("lysithea"),
        &names.user("elara"),
        &shared,
    )
    .await
    .unwrap();
}

/// Uploads a sample shared by two groups, with tags, a comment and attachment, a result with
/// files, and a repo; returns the sample's sha256
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The main group
/// * `second_group` - The second group the sample is shared with
async fn files(
    harness: &Harness,
    client: &Thorium,
    names: &Names,
    group: &str,
    second_group: &str,
) -> String {
    // upload a sample shared by both groups, with tags and an origin
    let data = Buffer::new(b"a sample every resource can point at".to_vec()).name("ceres.bin");
    let origin = OriginRequest::downloaded(
        "https://example.invalid/ceres.bin",
        Some("example".to_owned()),
    );
    let request = SampleRequest::new_buffer(data, vec![group, second_group])
        .description("fixture sample")
        .tag("Planet", "Jupiter")
        .tag("moon", "Europa")
        .origin(origin);
    let sample = client.files.create(request).await.unwrap().sha256;
    // comment on it with an attachment
    let scratch = harness.scratch(&format!("broad-{}", names.suffix));
    let attachment = scratch.join("notes.txt");
    std::fs::write(&attachment, b"an attached note").unwrap();
    let comment = CommentRequest::new(&sample, "fixture comment")
        .file(OnDiskFile::new(&attachment).trim_prefix(&scratch));
    client.files.comment(comment).await.unwrap();
    // add a result with files
    let result = OutputRequest::new(
        sample.clone(),
        "fixture-tool",
        "fixture result",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new("result file").name("out.txt"))
    .buffer(Buffer::new("nested result file").name("nested/out.txt"))
    .entities(
        EntityKinds::Device,
        1,
        Buffer::new(r#"[{"name":"fixture device"}]"#),
    )
    .tool_version(ImageVersion::SemVer(semver::Version::new(1, 2, 3)));
    client.files.create_result(result).await.unwrap();
    // create a repo
    generators::repos(group, 1, client).await.unwrap();
    sample
}

/// Uploads the harness admin's profile icon and creates a device with an image, which the API
/// stores in the graphics bucket
///
/// # Arguments
///
/// * `harness` - The harness naming the cluster
/// * `names` - This seed's names
/// * `group` - The group to create the device in
async fn graphics(harness: &Harness, names: &Names, group: &str) {
    // an icon and an entity image
    seed::icon(&harness.api_url, &harness.token).await;
    seed::entity_image(
        &harness.api_url,
        &harness.token,
        group,
        &names.user("leda"),
        b"\x89PNG\r\n\x1a\nfixture device image",
    )
    .await;
}

/// Creates a vendor, a device it makes, and a flag on the sample
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - This seed's names
/// * `group` - The group to create them in
/// * `sample` - The sample the flag points at
async fn entities(client: &Thorium, names: &Names, group: &str, sample: &str) {
    // create a vendor and a device it makes
    let groups = vec![group.to_owned()];
    let vendor_meta = generators::gen_vendor_meta();
    let vendor = EntityRequest::new(names.user("amalthea"), vendor_meta, groups.clone());
    let vendor = client.entities.create(vendor).await.unwrap().id;
    let device_meta = generators::gen_device_meta(vec![vendor]);
    let device = EntityRequest::new(names.user("adrastea"), device_meta, groups.clone());
    client.entities.create(device).await.unwrap();
    // flag the sample with an association from a flag entity
    let flag_name = names.user("metis");
    let flag = EntityRequest::new(&flag_name, generators::gen_flag_meta(), groups.clone());
    let flag = client.entities.create(flag).await.unwrap();
    let source = AssociationTarget::Entity {
        id: flag.id,
        name: flag_name,
    };
    let association = AssociationRequest::new(AssociationKind::FlagFor, source)
        .target(AssociationTarget::File(sample.to_owned()))
        .groups(groups);
    client.associations.create(&association).await.unwrap();
}

/// What the value-class fixture holds: every required cell and every case the stores hold
#[derive(Debug)]
pub struct ValueClasses {
    /// The required cells and the observed cases
    pub matrix: super::classes::CoverageMatrix,
    /// Rule violations found while observing: opaque schema nodes, unlisted JSON columns,
    /// inconsistent derived columns, list entries naming missing columns, unmapped hash fields
    pub problems: Vec<String>,
    /// The matrix size of the run
    pub size: super::classes::MatrixSize,
}

/// Returns how long a near-cell-limit value is: just under half the test Scylla's commit-log
/// segment, the largest mutation it accepts
///
/// # Arguments
///
/// * `scylla` - The Scylla session
async fn near_limit_len(scylla: &scylla::client::session::Session) -> usize {
    // read the segment size, defaulting to Scylla's default of 32 MiB
    let segment_mb = scylla
        .query_unpaged(
            "SELECT value FROM system.config WHERE name = 'commitlog_segment_size_in_mb'",
            &[],
        )
        .await
        .ok()
        .and_then(|result| result.into_rows_result().ok())
        .and_then(|rows| rows.maybe_first_row::<(String,)>().ok().flatten())
        .and_then(|(value,)| value.trim_matches('"').parse::<usize>().ok())
        .unwrap_or(32);
    // leave room for the rest of the row
    segment_mb * 1_048_576 / 2 - 262_144
}

/// Derives every cell the backed-up columns, `Backup` fields, and object layouts need
///
/// # Arguments
///
/// * `tables` - The backed-up tables' live columns
/// * `schema` - The `Backup` schema
/// * `size` - The matrix size of the run
/// * `problems` - Where opaque schema nodes and list entries for tables that aren't backed up
///   are collected
#[must_use]
pub fn requirements(
    tables: &[super::columns::Table],
    schema: &super::schema::BackupSchema,
    size: super::classes::MatrixSize,
    problems: &mut Vec<String>,
) -> super::classes::CoverageMatrix {
    // columns, fields, and layouts each add their cells
    let lists = super::enums::all();
    let mut matrix = super::classes::CoverageMatrix::default();
    for table in tables {
        super::columns::require(table, &lists, size, &mut matrix);
        // a near-limit entry can't name a column the live schema uses as a key
        problems.extend(super::columns::key_sized_entries(
            table,
            super::classes::NEAR_LIMIT_COLUMNS,
        ));
    }
    let mut opaque = Vec::new();
    super::schema::require(&schema.tree(), "", &mut matrix, &mut opaque);
    problems.extend(
        opaque
            .into_iter()
            .map(|path| format!("the Backup schema has an opaque node at {path}")),
    );
    super::objects::require(&mut matrix);
    // every table the column lists name must be a backed-up table
    for (table, column) in super::columns::listed_columns() {
        if !tables.iter().any(|candidate| candidate.name == table) {
            problems.push(format!(
                "a list names {table}.{column}, but {table} isn't backed up"
            ));
        }
    }
    matrix
}

/// Reads the live columns of every backed-up table
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
pub async fn backed_up_tables(harness: &Harness) -> Vec<super::columns::Table> {
    // one table per backed-up registry entry
    let scylla = harness.scylla().await;
    let mut tables = Vec::new();
    for entry in thoradm::backup::coverage::Registry::backed_up(
        thoradm::backup::coverage::Store::ScyllaTable,
    ) {
        tables.push(
            super::columns::live_table(&scylla, &harness.conf.thorium.namespace, entry.name).await,
        );
    }
    tables
}

/// Counts what the namespace's tables, Redis records, and buckets hold
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `tables` - The backed-up tables' live columns
/// * `matrix` - The matrix to count observations in
/// * `problems` - Where rule violations are collected
pub async fn observe_stores(
    harness: &Harness,
    tables: &[super::columns::Table],
    matrix: &mut super::classes::CoverageMatrix,
    problems: &mut Vec<String>,
) {
    // classify every row of every backed-up table
    let lists = super::enums::all();
    let size = super::classes::MatrixSize::from_env();
    let planner = super::columns::Planner::new(&harness.conf, &lists, size, 0);
    let scylla = harness.scylla().await;
    for table in tables {
        let rows =
            super::columns::read_rows(&scylla, &harness.conf.thorium.namespace, &table.name).await;
        super::columns::observe(table, &rows, &lists, &planner, matrix, problems);
    }
    // classify every object of every backed-up layout
    super::objects::observe(&harness.s3(), &harness.conf, matrix).await;
}

/// Which cases of the value-class fixture to seed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixtureCases {
    /// Whether to seed the cases that exercise sizes: large and near-limit values, collections of
    /// many elements, and objects at the part size or uploaded in parts
    pub sizes: bool,
    /// Whether to leave unset, each in a row of its own, the columns whose format-1 archive field
    /// isn't optional, which a format-1 backup skips
    pub format_1_unset: bool,
}

impl FixtureCases {
    /// Every case
    pub const ALL: FixtureCases = FixtureCases {
        sizes: true,
        format_1_unset: true,
    };

    /// Returns whether a required cell is seeded
    ///
    /// # Arguments
    ///
    /// * `location` - The cell's location
    /// * `class` - The cell's class
    fn keeps(
        self,
        location: &super::classes::Location,
        class: &super::classes::ValueClass,
    ) -> bool {
        use super::classes::{FORMAT_1_REQUIRED, Location, ValueClass};
        // size cases go unless sizes are asked for
        if !self.sizes
            && matches!(
                class,
                ValueClass::Large | ValueClass::NearCellLimit | ValueClass::Many
            )
        {
            return false;
        }
        // an unset column format 1 can't hold goes unless asked for
        match location {
            Location::Column { table, column } if *class == ValueClass::Null => {
                self.format_1_unset
                    || !FORMAT_1_REQUIRED.contains(&(table.as_str(), column.as_str()))
            }
            _ => true,
        }
    }
}

/// Seeds the value-class fixture and returns its coverage matrix
///
/// Columns, fields, and layouts come from the live schema, the `Backup` schema, and the registry;
/// rows, records, and objects are written to hold every class each needs; and the matrix counts
/// what the stores hold afterwards, not what the builder meant to write.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
///
/// # Panics
///
/// Panics if a row, record, or object can't be written.
pub async fn value_classes(harness: &Harness) -> ValueClasses {
    // every case
    Box::pin(value_classes_with(harness, FixtureCases::ALL)).await
}

/// Seeds some cases of the value-class fixture and returns its coverage matrix
///
/// The matrix holds only the cells of the cases seeded.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `cases` - Which cases to seed
///
/// # Panics
///
/// Panics if a row, record, or object can't be written.
pub async fn value_classes_with(harness: &Harness, cases: FixtureCases) -> ValueClasses {
    // derive the required cells of the cases asked for
    let size = super::classes::MatrixSize::from_env();
    let tables = backed_up_tables(harness).await;
    let schema = super::schema::BackupSchema::load();
    let mut problems = Vec::new();
    let mut matrix = requirements(&tables, &schema, size, &mut problems);
    matrix
        .needs
        .retain(|(location, class), _| cases.keeps(location, class));
    // plan and write rows and the objects they name
    let lists = super::enums::all();
    let scylla = harness.scylla().await;
    let near_limit = near_limit_len(&scylla).await;
    let mut planner = super::columns::Planner::new(&harness.conf, &lists, size, near_limit);
    let mut plans = Vec::new();
    for table in &tables {
        plans.extend(planner.plan(table, &matrix));
    }
    plans.extend(super::objects::plan(&mut planner, &tables, cases.sizes));
    super::columns::write_rows(&scylla, &harness.conf.thorium.namespace, &plans).await;
    super::objects::write_objects(&harness.s3(), &harness.conf, &plans).await;
    super::objects::write_user_icon(&harness.s3(), &harness.conf).await;
    // seed and observe the Redis records
    problems.extend(
        Box::pin(super::records::seed_and_observe(
            harness,
            &schema,
            &mut matrix,
            cases.sizes,
        ))
        .await,
    );
    problems.extend(super::records::unmapped_fields(harness, &schema.tree()).await);
    // observe the tables and buckets
    observe_stores(harness, &tables, &mut matrix, &mut problems).await;
    ValueClasses {
        matrix,
        problems,
        size,
    }
}
