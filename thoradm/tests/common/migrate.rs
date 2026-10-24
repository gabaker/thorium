//! The harness for the migration runner's tests: a keyspace of its own holding the two state
//! tables and the fixture tables, seeded rows, and the backends a run reads and writes through

use scylla::client::session::Session;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use thoradm::migrate::fixture;
use thoradm::migrate::registry::MigrationDef;
use thorium::Conf;
use thorium::models::schema::{
    MigrationRunStatus, PassSpec, PassStore, RunBackends, RunContext, RunLimits, RunMode,
    SchemaObject, WriterLease, apply_objects,
};
use tokio_util::sync::CancellationToken;

/// The test config every run starts from; the environment overrides point it at the test cluster
pub const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// A migration test's world: its own keyspace, with the state tables and the fixture tables
pub struct World {
    /// The keyspace, which is also the namespace
    pub ns: String,
    /// The config with the namespace set
    pub conf: Conf,
    /// The clients a run reads and writes through
    pub backends: RunBackends,
    /// The prepared statements on the pass rows
    pub store: PassStore,
}

impl World {
    /// Build a world with a keyspace of its own, empty fixture tables, and the state tables
    ///
    /// # Arguments
    ///
    /// * `suffix` - What makes the test's keyspace unique (lowercase, short)
    pub async fn new(suffix: &str) -> World {
        // only the state tables and the fixture tables
        World::build(suffix, false).await
    }

    /// Build a world holding the whole declared schema, for the migrations that move real tables
    ///
    /// # Arguments
    ///
    /// * `suffix` - What makes the test's keyspace unique (lowercase, short)
    pub async fn new_full(suffix: &str) -> World {
        // every declared table and view
        World::build(suffix, true).await
    }

    /// Build a world
    ///
    /// # Arguments
    ///
    /// * `suffix` - What makes the test's keyspace unique
    /// * `full` - Whether to create every declared object
    async fn build(suffix: &str, full: bool) -> World {
        // the run's namespace comes from the environment, or is made up for a local run
        let base = super::base_namespace::base_namespace();
        let ns = format!("{base}_{suffix}");
        let mut conf = Conf::new(CONFIG).expect("the test config loads");
        conf.thorium.namespace.clone_from(&ns);
        let session = Arc::new(
            thorium::models::schema::connect(&conf)
                .await
                .expect("Scylla is reachable"),
        );
        // start from nothing, then converge the two state tables
        session
            .query_unpaged(format!("DROP KEYSPACE IF EXISTS {ns}"), ())
            .await
            .expect("the keyspace drops");
        let objects = SchemaObject::declared()
            .into_iter()
            .filter(|object| {
                full || matches!(
                    object.name.as_str(),
                    "schema_migrations" | "schema_migration_issues"
                )
            })
            .collect::<Vec<_>>();
        apply_objects(&session, &conf, &objects, false, "", &|_| ())
            .await
            .expect("the state tables apply");
        // the fixture tables and a view over one of them
        for cql in [
            format!(
                "CREATE TABLE {ns}.{} (id text PRIMARY KEY, body text, layout int)",
                fixture::TABLE
            ),
            format!(
                "CREATE TABLE {ns}.{} (id text PRIMARY KEY, body text)",
                fixture::PLAIN_TABLE
            ),
        ] {
            session
                .query_unpaged(cql, ())
                .await
                .expect("a fixture table creates");
        }
        let redis = thoradm::shared::redis::get_client(&conf)
            .await
            .expect("Redis is reachable");
        let s3 = thoradm::migrate::context::s3_client(&conf);
        let backends = RunBackends {
            partition_sizes: thoradm::migrate::context::partition_sizes(&conf),
            namespace: ns.clone(),
            scylla: session.clone(),
            redis,
            s3,
            conf: Arc::new(conf.clone()),
        };
        let store = PassStore::prepare(session, &ns)
            .await
            .expect("statements prepare");
        // a marker left by an earlier run must not satisfy this one
        {
            let mut redis_conn = backends.redis.get().await.expect("a Redis connection");
            let _: () = redis::cmd("DEL")
                .arg(fixture::marker_key(&ns))
                .arg(thorium::models::schema::lease_key(&ns))
                .query_async(&mut *redis_conn)
                .await
                .expect("the marker and lease clear");
        }
        World {
            ns,
            conf,
            backends,
            store,
        }
    }

    /// The Scylla session
    pub fn session(&self) -> &Session {
        // shared with the store and the runs
        &self.backends.scylla
    }

    /// Drop the world's keyspace
    pub async fn drop_keyspace(&self) {
        // the keyspace holds nothing a later test needs
        let _ = self
            .session()
            .query_unpaged(format!("DROP KEYSPACE IF EXISTS {}", self.ns), ())
            .await;
    }

    /// Add rows to the fixture table
    ///
    /// # Arguments
    ///
    /// * `rows` - The `(id, body)` of each row, in layout 1 unless the body says otherwise
    pub async fn seed(&self, rows: &[(String, String)]) {
        // insert every row with its layout
        let insert = self
            .session()
            .prepare(format!(
                "INSERT INTO {}.{} (id, body, layout) VALUES (?, ?, ?)",
                self.ns,
                fixture::TABLE
            ))
            .await
            .expect("the insert prepares");
        for (id, body) in rows {
            let layout = if body.contains("theme") { 2 } else { 1 };
            self.session()
                .execute_unpaged(&insert, (id, body, layout))
                .await
                .expect("a row inserts");
        }
    }

    /// Add `count` valid layout 1 rows named `<prefix>-<n>`
    ///
    /// # Arguments
    ///
    /// * `prefix` - The ids' prefix
    /// * `count` - How many rows
    pub async fn seed_valid(&self, prefix: &str, count: usize) {
        // a valid layout 1 body per row
        let rows = (0..count)
            .map(|n| (format!("{prefix}-{n}"), format!("{{\"name\":\"n{n}\"}}")))
            .collect::<Vec<_>>();
        self.seed(&rows).await;
    }

    /// Read every row of the fixture table as `(id, body, layout)`, sorted by id
    pub async fn rows(&self) -> Vec<(String, String, i32)> {
        // read the whole table
        let result = self
            .session()
            .query_unpaged(
                format!(
                    "SELECT id, body, layout FROM {}.{}",
                    self.ns,
                    fixture::TABLE
                ),
                (),
            )
            .await
            .expect("the table reads")
            .into_rows_result()
            .expect("rows");
        let mut rows = result
            .rows::<(String, Option<String>, Option<i32>)>()
            .expect("typed rows")
            .map(|row| {
                let (id, body, layout) = row.expect("a row");
                (id, body.unwrap_or_default(), layout.unwrap_or_default())
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    }

    /// Read the layout marker a finished pass records
    pub async fn marker(&self) -> Option<String> {
        // the marker is a plain Redis string
        let mut conn = self.backends.redis.get().await.expect("a Redis connection");
        redis::cmd("GET")
            .arg(fixture::marker_key(&self.ns))
            .query_async(&mut *conn)
            .await
            .expect("the marker reads")
    }

    /// The pass spec of a registered fixture migration
    ///
    /// # Arguments
    ///
    /// * `def` - The migration
    pub fn spec(def: &MigrationDef) -> PassSpec {
        // the registry's own defaults, with no accepted codes
        thoradm::migrate::context::spec_for(def, std::iter::empty(), None)
    }

    /// A context for a run, with limits that make every batch checkpoint
    ///
    /// # Arguments
    ///
    /// * `mode` - `Run` or `Check`
    /// * `batch_rows` - The rows of each batch
    pub fn ctx(&self, mode: RunMode, batch_rows: usize) -> RunContext {
        // checkpoint after every batch, and give up on a failing backend quickly
        let mut ctx = RunContext::new(mode, self.backends.clone(), CancellationToken::new());
        ctx.limits = RunLimits {
            batch_rows,
            checkpoint_every: std::time::Duration::ZERO,
            backoff: std::time::Duration::from_millis(50),
            retries: 6,
            ..RunLimits::default()
        };
        ctx
    }

    /// Take the namespace's writer lease as a test holder
    ///
    /// # Arguments
    ///
    /// * `holder` - The holder's name
    pub async fn lease(&self, holder: &str) -> WriterLease {
        // never forced, so a held lease fails the test
        WriterLease::acquire(&self.backends.redis, &self.ns, holder, false)
            .await
            .expect("the lease is free")
    }

    /// Read a pass's row status
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    pub async fn status(&self, key: &str) -> Option<MigrationRunStatus> {
        // read the pass row
        self.store
            .pass_row(key)
            .await
            .expect("the pass row reads")
            .map(|row| row.status)
    }

    /// A thoradm command against this world's namespace
    pub fn thoradm(&self) -> Command {
        // run the binary cargo built for these tests against the test config
        let mut command = Command::new(env!("CARGO_BIN_EXE_thoradm"));
        command
            .args(["--cluster-conf", CONFIG])
            .env("THORIUM__THORIUM__NAMESPACE", &self.ns);
        command
    }

    /// Run a thoradm command to completion
    ///
    /// # Arguments
    ///
    /// * `command` - The command to run
    pub fn run(command: &mut Command) -> Output {
        // keep stdout and stderr
        command.output().expect("thoradm runs")
    }

    /// A fresh file for a termination summary
    ///
    /// # Arguments
    ///
    /// * `name` - What makes the file unique
    pub fn termination_log(name: &str) -> PathBuf {
        // a file under the temporary directory, removed if an earlier run left it
        let path =
            std::env::temp_dir().join(format!("thoradm-mig-{name}-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }
}

/// How a child that was sent a signal ended
#[derive(Debug)]
pub struct Signalled {
    /// Whether the signal was sent
    pub sent: bool,
    /// How the child exited
    pub status: std::process::ExitStatus,
    /// Every line the child wrote to stderr
    pub stderr: Vec<String>,
    /// How long after the signal the child was gone
    pub after: Option<std::time::Duration>,
}

/// Run a command and send its process a signal the first time a stderr line contains a pattern
///
/// # Arguments
///
/// * `command` - The command to run; its stderr is captured
/// * `pattern` - The text whose first appearance on stderr triggers the signal
/// * `signal` - The signal to send, such as `libc::SIGTERM`
pub async fn run_signal(
    command: Command,
    pattern: &str,
    signal: i32,
) -> std::io::Result<Signalled> {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};
    // spawn the child with stderr piped so its progress can be watched
    let mut command = tokio::process::Command::from(command);
    command
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = i32::try_from(child.id().expect("the child is running")).expect("a pid fits");
    let mut lines = BufReader::new(child.stderr.take().expect("stderr was piped")).lines();
    let mut captured = Vec::new();
    let mut sent_at = None;
    // read stderr until the child closes it, signalling at the first match
    while let Some(line) = lines.next_line().await? {
        if sent_at.is_none() && line.contains(pattern) {
            // SAFETY: kill only sends a signal; it touches no memory of this process
            let result = unsafe { libc::kill(pid, signal) };
            assert_eq!(result, 0, "the signal was sent");
            sent_at = Some(std::time::Instant::now());
        }
        captured.push(line);
    }
    let status = child.wait().await?;
    Ok(Signalled {
        sent: sent_at.is_some(),
        status,
        stderr: captured,
        after: sent_at.map(|at| at.elapsed()),
    })
}
