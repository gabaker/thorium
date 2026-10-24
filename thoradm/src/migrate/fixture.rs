//! The fixture migrations thoradm's tests run through the real runner
//!
//! They move rows of a small test table from one layout to another: layout 1 stores
//! `{"name": ...}` and layout 2 stores `{"name": ..., "theme": ...}`. A row whose body parses as
//! neither is an issue with the reason `ReasonBuilder` builds, a row whose id starts with
//! `reject-` has its write rejected, and a finished pass records layout 2 in Redis. Only builds
//! with the `test-support` feature compile this module.

use async_trait::async_trait;
use scylla::value::CqlValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thorium::models::schema::ReportNote;
use thorium::models::schema::{
    AppliedReport, Batch, CheckWait, IssueReport, MigrateError, MigrationRunner, ObjectClass,
    Outcome, PassCounts, RunContext, RunMode, ScyllaScan, Slice, backend_error, is_rejection,
};
use thorium::models::upgrades::{
    Catalog, CatalogEntry, Fill, FillValue, IssueCategory, IssueReason, MigrationPhase,
    ReasonBuilder, Record, Reversibility, StepDef, StepKind, UpgradeComponent,
};

use super::registry::{Backend, MigrationDef, Support};

/// The table the fixture migrations read and write
pub const TABLE: &str = "mig_fixture";

/// The table the hand-written fixture migration reads and writes
pub const PLAIN_TABLE: &str = "mig_plain";

/// The environment variable that makes thoradm plan, check, and revert over [`CATALOG`] instead of
/// the catalog it ships
pub const CATALOG_ENV: &str = "THORADM_FIXTURE_CATALOG";

/// The environment variable that cuts the ring at a token, for a test that needs a slice
/// boundary at a chosen token
pub const CUT_ENV: &str = "THORADM_FIXTURE_CUT";

/// A theme, as layout 2 stores it
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Theme {
    /// The dark theme
    Dark,
    /// The light theme
    Light,
}

/// A row's body in layout 1
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BodyV1 {
    /// The name
    pub name: String,
}

/// A row's body in layout 2
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BodyV2 {
    /// The name
    pub name: String,
    /// The theme
    pub theme: Theme,
}

/// How a fixture migration behaves
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Layouts 1 and 2 of the test table, classified through `ReasonBuilder`
    Theme,
    /// A hand-written migration with no declared layouts, on its own table
    Plain,
    /// Layouts 1 and 2, whose check asks to wait while it finds work
    Wait,
    /// Layouts 1 and 2, whose report gets a warning note
    Notes,
}

/// A fixture migration's runner
pub struct Fixture {
    /// How it behaves
    kind: Kind,
}

/// The migration of layout 1 to layout 2 with a group limit of 5
pub const THEME: MigrationDef = MigrationDef {
    id: "fixture-theme",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture",
    chain: &["fixture-theme"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// The same migration with a group limit of 0, like the users family
pub const STRICT: MigrationDef = MigrationDef {
    id: "fixture-strict",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture",
    chain: &["fixture-theme"],
    group_max: 0,
    families: &["fixture"],
    has_views: false,
};

/// The same pass run as a different fused chain
pub const CHAIN: MigrationDef = MigrationDef {
    id: "fixture-chain",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture",
    chain: &["fixture-theme", "fixture-later"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// The same migration on a table with a view, which starts with fewer writes in flight
pub const VIEWS: MigrationDef = MigrationDef {
    id: "fixture-views",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture-views",
    chain: &["fixture-theme"],
    group_max: 5,
    families: &["fixture"],
    has_views: true,
};

/// The migration a lossy reverse step of [`CATALOG`] performs, which moves the fixture table's rows
/// from layout 1 to layout 2 like [`THEME`]
pub const LOSSY: MigrationDef = MigrationDef {
    id: "fixture-lossy",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture-lossy",
    chain: &["fixture-theme"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// The forward step of [`CATALOG`], which can be reversed only by losing a fixture field and a
/// field of a backed-up table
const LOSSY_FORWARD: StepDef = StepDef {
    id: "fixture-forward",
    kind: StepKind::Config,
    description: "A fixture step that lost data when it ran",
    phase: Some(MigrationPhase::Migrate),
    reversibility: Some(Reversibility::LossyRevert {
        fills: &[Fill {
            field: "theme",
            value: FillValue::Sentinel("unknown"),
        }],
        lost_fields: &["theme", "samples_list.description"],
        enum_mappings: &[],
        privilege_orders: &[],
        description: "Rows lose their theme",
    }),
    ..StepDef::BASE
};

/// The step of [`CATALOG`] that reverses [`LOSSY_FORWARD`] by running [`LOSSY`], with the event
/// handler and search streamer paused
const LOSSY_REVERSE: StepDef = StepDef {
    id: "fixture-reverse",
    kind: StepKind::Data,
    description: "A fixture reverse step that loses data",
    pause: &[
        UpgradeComponent::EventHandler,
        UpgradeComponent::SearchStreamer,
    ],
    thoradm_migration: Some("fixture-lossy"),
    manual_procedure: Some("Convert the fixture rows by hand."),
    phase: Some(MigrationPhase::Migrate),
    reverses: Some("fixture-forward"),
    ..StepDef::BASE
};

/// The catalog thoradm plans over under [`CATALOG_ENV`]: one lossy step and its reverse
pub const CATALOG: Catalog = Catalog {
    edges: &[
        CatalogEntry {
            from: None,
            id: "2026-10-v01",
            released_with: "0.0.0",
            description: "A fixture baseline",
            schema: None,
            prerelease: false,
            sibling: false,
            steps: &[],
        },
        CatalogEntry {
            from: Some("2026-10-v01"),
            id: "2026-10-v02",
            released_with: "0.0.0",
            description: "A fixture edge with a lossy step",
            schema: None,
            prerelease: false,
            sibling: false,
            steps: &[LOSSY_FORWARD],
        },
        CatalogEntry {
            from: Some("2026-10-v02"),
            id: "2026-10-v01",
            released_with: "0.0.0",
            description: "A fixture edge that reverses it",
            schema: None,
            prerelease: false,
            sibling: false,
            steps: &[LOSSY_REVERSE],
        },
    ],
    floors: &[],
};

/// A hand-written migration without a declared layout
pub const PLAIN: MigrationDef = MigrationDef {
    id: "fixture-plain",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Plain },
    pass: "fixture-plain",
    chain: &["fixture-plain"],
    group_max: 5,
    families: &["plain"],
    has_views: false,
};

/// A migration thoradm can check but not run
pub const CHECKED: MigrationDef = MigrationDef {
    id: "fixture-checkonly",
    backend: Backend::Scylla,
    support: Support::CheckOnly,
    runner: &Fixture { kind: Kind::Theme },
    pass: "fixture-checkonly",
    chain: &["fixture-theme"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// A migration whose finish adds a warning note to its report
pub const NOTES: MigrationDef = MigrationDef {
    id: "fixture-notes",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Notes },
    pass: "fixture-notes",
    chain: &["fixture-notes"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// A migration whose check asks to wait
pub const WAIT: MigrationDef = MigrationDef {
    id: "fixture-wait",
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &Fixture { kind: Kind::Wait },
    pass: "fixture-wait",
    chain: &["fixture-wait"],
    group_max: 5,
    families: &["fixture"],
    has_views: false,
};

/// The Redis key a finished pass records its layout in
///
/// # Arguments
///
/// * `ns` - The namespace
#[must_use]
pub fn marker_key(ns: &str) -> String {
    // namespaced like every other Thorium Redis key
    format!("{ns}:fixture:layout")
}

impl Fixture {
    /// The scan of the table this fixture reads
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context, for the keyspace
    fn scan(&self, ctx: &RunContext) -> ScyllaScan {
        // the hand-written migration has its own table with no layout column
        let plain = self.kind == Kind::Plain;
        ScyllaScan {
            keyspace: ctx.backends.namespace.clone(),
            table: if plain { PLAIN_TABLE } else { TABLE }.to_owned(),
            partition_key: vec!["id".to_owned()],
            columns: if plain {
                vec!["id".to_owned(), "body".to_owned()]
            } else {
                vec!["id".to_owned(), "body".to_owned(), "layout".to_owned()]
            },
            key_columns: vec!["id".to_owned()],
            cuts: std::env::var(CUT_ENV)
                .ok()
                .and_then(|cut| cut.parse().ok())
                .into_iter()
                .collect(),
        }
    }

    /// Build the record a converted row is written as
    ///
    /// # Arguments
    ///
    /// * `record` - The row as read
    /// * `body` - The converted body
    fn converted(&self, record: &Record, body: String) -> Record {
        // keep the key and the other columns, replacing the body and the layout
        let Record::Scylla { key, columns } = record else {
            return record.clone();
        };
        let mut columns: BTreeMap<String, Option<CqlValue>> = columns.clone();
        columns.insert("body".to_owned(), Some(CqlValue::Text(body)));
        if self.kind != Kind::Plain {
            columns.insert("layout".to_owned(), Some(CqlValue::Int(2)));
        }
        Record::Scylla {
            key: key.clone(),
            columns,
        }
    }
}

#[async_trait]
impl MigrationRunner for Fixture {
    /// Split the table's token ring into slices
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // the ring split contiguously
        Ok(self.scan(ctx).slices(n))
    }

    /// Read the next batch of a slice in token order
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last token done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // a token scan that ends on a partition boundary
        self.scan(ctx).next_batch(slice, after, ctx).await
    }

    /// Classify one row, converting a layout 1 body to layout 2
    ///
    /// # Arguments
    ///
    /// * `record` - The row
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // a check classifies exactly as a run does and only skips the write
        let _ = (mode, ctx);
        let Some(body) = record.text("body") else {
            return Ok(Outcome::Issue(IssueReport {
                key: key_of(record),
                transform: "fixture-theme".to_owned(),
                class: ObjectClass::Unrecognized,
                reason: IssueReason::new(IssueCategory::MissingField, "body"),
            }));
        };
        let key = key_of(record);
        // the hand-written migration knows two bodies and nothing else
        if self.kind == Kind::Plain {
            return Ok(match body.as_str() {
                "new" => Outcome::Current,
                "old" => Outcome::Write(self.converted(record, "new".to_owned())),
                _ => Outcome::Issue(IssueReport {
                    key,
                    transform: "fixture-plain".to_owned(),
                    class: ObjectClass::Unrecognized,
                    reason: IssueReason::new(IssueCategory::InvalidValue, "body"),
                }),
            });
        }
        // a body `poison:<n>` stands for a distinct failure, so a test can make as many groups as it
        // needs
        if let Some(n) = body.strip_prefix("poison:") {
            return Ok(Outcome::Issue(IssueReport {
                key,
                transform: "fixture-theme".to_owned(),
                class: ObjectClass::Unrecognized,
                reason: IssueReason::new(IssueCategory::InvalidValue, format!("poison{n}")),
            }));
        }
        // layout 2 is current; layout 1 converts; anything else is an issue with its reason
        if ReasonBuilder::parse::<BodyV2>(&body, "body").is_ok() {
            return Ok(Outcome::Current);
        }
        match ReasonBuilder::parse::<BodyV1>(&body, "body") {
            Ok(old) => {
                let new = BodyV2 {
                    name: old.name,
                    theme: Theme::Dark,
                };
                let json = serde_json::to_string(&new)
                    .map_err(|err| MigrateError::Systematic(err.to_string()))?;
                Ok(Outcome::Write(self.converted(record, json)))
            }
            Err(reason) => Ok(Outcome::Issue(IssueReport {
                key,
                transform: "fixture-theme".to_owned(),
                class: ObjectClass::Unrecognized,
                reason,
            })),
        }
    }

    /// Write a converted row, rejecting rows whose id starts with `reject-`
    ///
    /// # Arguments
    ///
    /// * `record` - The converted row
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // a row the fixture refuses for good
        let key = key_of(record);
        if key.starts_with("reject-") {
            return Err(MigrateError::Rejected(format!("the fixture refuses {key}")));
        }
        let ns = &ctx.backends.namespace;
        let body = record.text("body").unwrap_or_default();
        let result = if self.kind == Kind::Plain {
            ctx.backends
                .scylla
                .query_unpaged(
                    format!("UPDATE {ns}.{PLAIN_TABLE} SET body = ? WHERE id = ?"),
                    (body, key),
                )
                .await
        } else {
            ctx.backends
                .scylla
                .query_unpaged(
                    format!("UPDATE {ns}.{TABLE} SET body = ?, layout = 2 WHERE id = ?"),
                    (body, key),
                )
                .await
        };
        result
            .map(|_| ())
            .map_err(|err| backend_error("write a fixture row", &err, is_rejection(&err)))
    }

    /// Read the named rows directly
    ///
    /// # Arguments
    ///
    /// * `keys` - The rows' ids
    /// * `ctx` - The run's context
    async fn fetch(
        &self,
        keys: &[String],
        ctx: &RunContext,
    ) -> Result<Option<Vec<Record>>, MigrateError> {
        // one lookup per id, through the same scan's row reader
        let scan = self.scan(ctx);
        let mut found = Vec::new();
        for key in keys {
            let cql = format!(
                "SELECT id, body{} FROM {}.{} WHERE id = ?",
                if self.kind == Kind::Plain {
                    ""
                } else {
                    ", layout"
                },
                scan.keyspace,
                scan.table
            );
            let result = ctx
                .backends
                .scylla
                .query_unpaged(cql, (key,))
                .await
                .map_err(|err| backend_error("read a fixture row", &err, is_rejection(&err)))?;
            let rows = result
                .into_rows_result()
                .map_err(|err| backend_error("read a fixture row", &err, false))?;
            let mut columns: BTreeMap<String, Option<CqlValue>> = BTreeMap::new();
            if self.kind == Kind::Plain {
                if let Some((id, body)) = rows
                    .maybe_first_row::<(String, Option<String>)>()
                    .map_err(|err| backend_error("read a fixture row", &err, false))?
                {
                    columns.insert("id".to_owned(), Some(CqlValue::Text(id)));
                    columns.insert("body".to_owned(), body.map(CqlValue::Text));
                }
            } else if let Some((id, body, layout)) = rows
                .maybe_first_row::<(String, Option<String>, Option<i32>)>()
                .map_err(|err| backend_error("read a fixture row", &err, false))?
            {
                columns.insert("id".to_owned(), Some(CqlValue::Text(id)));
                columns.insert("body".to_owned(), body.map(CqlValue::Text));
                columns.insert("layout".to_owned(), layout.map(CqlValue::Int));
            }
            if !columns.is_empty() {
                found.push(Record::Scylla {
                    key: key.clone(),
                    columns,
                });
            }
        }
        Ok(Some(found))
    }

    /// Record layout 2 in Redis once every slice is done
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the marker only advances when a pass finishes
        let mut conn = ctx
            .backends
            .redis
            .get()
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))?;
        redis::cmd("SET")
            .arg(marker_key(&ctx.backends.namespace))
            .arg("2")
            .query_async::<()>(&mut *conn)
            .await
            .map_err(|err| MigrateError::Backend(err.to_string()))
    }

    /// Check that a row's body loads as layout 2
    ///
    /// # Arguments
    ///
    /// * `record` - The row
    /// * `ctx` - The run's context
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // the plain migration's rows are loaded when they say `new`
        let _ = ctx;
        let body = record.text("body").unwrap_or_default();
        if self.kind == Kind::Plain {
            return if body == "new" {
                Ok(())
            } else {
                Err(IssueReason::new(IssueCategory::InvalidValue, "body"))
            };
        }
        ReasonBuilder::parse::<BodyV2>(&body, "body").map(|_| ())
    }

    /// Add a warning note about a named item to the report
    ///
    /// # Arguments
    ///
    /// * `report` - The report to add to
    /// * `ctx` - The run's context
    async fn finish_report(
        &self,
        report: &mut AppliedReport,
        ctx: &RunContext,
    ) -> Result<(), MigrateError> {
        // only the notes fixture adds anything
        let _ = ctx;
        if self.kind == Kind::Notes {
            report.warnings_total += 1;
            report.notes.push(ReportNote {
                group: "fixture".to_owned(),
                item: "item-name".to_owned(),
                note: "an item needs a look".to_owned(),
                warning: true,
            });
        }
        Ok(())
    }

    /// Ask to wait while this migration finds work, when the test says so
    ///
    /// # Arguments
    ///
    /// * `counts` - What the check found
    /// * `ctx` - The run's context
    async fn check_wait(
        &self,
        counts: &PassCounts,
        ctx: &RunContext,
    ) -> Result<Option<CheckWait>, MigrateError> {
        // only the waiting fixture waits, and only while there is something to migrate
        let _ = ctx;
        Ok(
            (self.kind == Kind::Wait && counts.migratable > 0).then(|| CheckWait {
                reason: "workers are still registered".to_owned(),
                retry_after_secs: 30,
            }),
        )
    }
}

/// The key of a row
///
/// # Arguments
///
/// * `record` - The row
#[must_use]
pub fn key_of(record: &Record) -> String {
    // the scan puts the row's id in the record's key
    thorium::models::schema::record_key(record).to_owned()
}
