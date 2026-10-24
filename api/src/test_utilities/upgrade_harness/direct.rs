//! The direct driver: scenarios in-process against real backing services
//!
//! The planner composes each plan from the scenario's catalog; the driver runs the plan's items
//! itself. A converge or a step run as is is recorded in the history, and a pass runs through
//! the real migration runner (workers, checkpoints, grouped issues) over a Redis family. The
//! approval gate is the harness's own stand-in for the operator's, and the deployment's upgrade
//! state is held in memory in place of a `ConfigMap`.

use async_trait::async_trait;
use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};
use tokio::sync::{Mutex, OnceCell, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

use super::chain::ChainRunner;
use super::data::{FixtureData, RedisData, S3Data, check_history, verify_layouts};
use super::gate::{Gate, GateDecision};
use super::types::{
    Check, CorruptionKind, FamilyFixture, FixtureSeed, FixtureVerifier, Injection, Interrupt,
    PassCountCheck, loss_approval, reasons_match,
};
use super::{
    ActionRecord, BackupRef, ConfirmSource, DriverKind, HarnessError, Scenario, ScenarioAction,
};
use crate::models::schema::{
    MigrateError, MigrationRunStatus, PassCounts, PassSpec, PassStore, RunBackends, RunContext,
    RunMode, RunOptions, SAMPLE_KEYS, WriterLease, issue_groups, run_pass,
};
use crate::models::upgrades::{
    AppliedStep, BlockReason, Catalog, CatalogEntry, ConfirmRefusal, Family, IssueSummary,
    PlanItem, PlannedPass, PlannedStep, Record, RevisionId, StepState, UpgradeApproval,
    UpgradeConditionType, UpgradePlan, UpgradeState,
};
use crate::test_utilities::{CONF, prepare_backends, read_only, redis_pool, s3_client};
use crate::{Conf, Error};

/// The version the harness records its states as applied by
const VERSION: &str = "0.0.0";

/// Who the harness is in the history and the writer lease
const HOLDER: &str = "upgrade-harness";

/// How many keys a scan step asks for when a pass is to be interrupted
const INTERRUPTED_BATCH_ROWS: usize = 5;

/// Held for a driver's lifetime so scenarios of one test binary don't share a namespace at once
static EXCLUSIVE: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));

/// Prepares the backends once per process
static BACKENDS: OnceCell<()> = OnceCell::const_new();

/// A snapshot of every object of a family: its key and its fields
type Snapshot = BTreeMap<String, BTreeMap<String, String>>;

/// What the plan is blocked on
#[derive(Debug, Clone)]
struct Blocked {
    /// The step
    step: String,
    /// What it waits for
    reason: BlockReason,
    /// What the status says
    message: String,
}

/// How a run of the plan ended
#[derive(Debug, Clone, PartialEq, Eq)]
enum Settled {
    /// Items ran, and the plan can go on
    Ran,
    /// An item waits for an approval
    Blocked,
    /// Nothing is left to run
    Finished,
    /// A pass failed
    Failed,
    /// A pass was interrupted and resumes the next time the plan runs
    Interrupted,
}

/// How one item ended
enum ItemEnd {
    /// Every step finished
    Done,
    /// A pass failed
    Failed,
    /// A pass was stopped before it ended
    Interrupted,
}

/// Runs scenarios in-process against real backing services
pub struct DirectDriver {
    /// Keeps other scenarios of the process out of the namespace
    _exclusive: OwnedMutexGuard<()>,
    /// The test config
    conf: Arc<Conf>,
    /// The clients migrations read and write through
    backends: RunBackends,
    /// The fixture objects in Redis and S3
    data: FixtureData,
    /// The pass rows
    store: PassStore,
    /// The catalog plans are made from
    catalog: Catalog,
    /// The deployment's upgrade state, held in memory instead of a `ConfigMap`
    state: UpgradeState,
    /// The approvals given and the codes shown
    gate: Gate,
    /// The plan being run, with the items still to run
    plan: Option<UpgradePlan>,
    /// Why the last target was refused
    refused: Option<String>,
    /// The steps the plan drops because a later step supersedes them, until they are recorded
    superseded: Vec<(PlannedStep, &'static str)>,
    /// What the plan is blocked on now
    blocked: Option<Blocked>,
    /// How many objects each family holds
    seeded: BTreeMap<Family, usize>,
    /// What the passes of each family read and wrote since the family was last seeded
    counts: BTreeMap<Family, BTreeMap<String, PassCounts>>,
    /// The families this scenario has wiped, so a second seed adds to the first
    wiped: BTreeSet<Family>,
    /// Data captured under a label
    captures: BTreeMap<String, BTreeMap<String, Snapshot>>,
    /// Cancel the next pass after this many batches
    stop_after: Option<u64>,
}

impl DirectDriver {
    /// Start a driver for a scenario
    ///
    /// Takes the process's exclusive hold on the test namespace, prepares the backends once, and
    /// starts the deployment at the catalog's baseline. Refuses to run when
    /// `THORIUM_TEST_CONFIG` points the harness at a real deployment, since the direct driver
    /// wipes and writes the families it seeds.
    ///
    /// # Arguments
    ///
    /// * `scenario` - The scenario to run
    pub async fn start(scenario: &Scenario) -> Result<Self, HarnessError> {
        // a real deployment's backends are never written
        if read_only() {
            return Err(HarnessError::Unsupported(
                "the direct driver writes fixtures, so it won't run against a config named by \
                 THORIUM_TEST_CONFIG"
                    .to_owned(),
            ));
        }
        // one scenario at a time works in the namespace
        let exclusive = EXCLUSIVE.clone().lock_owned().await;
        BACKENDS
            .get_or_try_init(|| async { prepare_backends(&CONF, false).await })
            .await
            .map_err(|err: Error| HarnessError::Backend(err.to_string()))?;
        // connect to the backends the runner uses
        let conf = Arc::new(CONF.clone());
        let session = Arc::new(
            crate::test_utilities::scylla_session()
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?,
        );
        let namespace = conf.thorium.namespace.clone();
        let store = PassStore::prepare(session.clone(), &namespace)
            .await
            .map_err(|err| backend(&err))?;
        let backends = RunBackends {
            namespace,
            partition_sizes: BTreeMap::new(),
            scylla: session,
            redis: redis_pool().await,
            s3: s3_client(&conf.thorium.s3),
            conf: conf.clone(),
        };
        // the deployment starts at the catalog's baseline
        let catalog = scenario.planned_catalog().unwrap_or(Catalog::PRODUCTION);
        let baseline = catalog
            .edges
            .iter()
            .find(|edge| edge.from.is_none())
            .map(CatalogEntry::revision)
            .ok_or_else(|| HarnessError::Failed("the catalog has no baseline".to_owned()))?;
        Ok(DirectDriver {
            _exclusive: exclusive,
            conf,
            data: FixtureData::new(
                RedisData::new(backends.redis.clone(), &backends.namespace),
                S3Data::new(backends.s3.clone(), backends.conf.clone()),
            ),
            backends,
            store,
            catalog,
            state: UpgradeState::new(baseline, VERSION),
            gate: Gate::new(scenario.is_dev()),
            plan: None,
            refused: None,
            superseded: Vec::new(),
            blocked: None,
            seeded: BTreeMap::new(),
            counts: BTreeMap::new(),
            wiped: BTreeSet::new(),
            captures: BTreeMap::new(),
            stop_after: None,
        })
    }

    /// The test config the driver connected with
    #[must_use]
    pub fn conf(&self) -> &Conf {
        // the config the backends were built from
        &self.conf
    }

    /// The deployment's recorded upgrade state
    #[must_use]
    pub fn state(&self) -> &UpgradeState {
        // the state held in memory
        &self.state
    }

    /// Write fixture objects, wiping the family the first time the scenario seeds it
    ///
    /// Object `i` of a seed takes variation `i` of its variant set, repeating when there are more
    /// objects than variations.
    ///
    /// # Arguments
    ///
    /// * `seed` - What to write
    /// * `fixture` - How the family's objects are built
    async fn seed(
        &mut self,
        seed: &FixtureSeed,
        fixture: &FamilyFixture,
    ) -> Result<String, HarnessError> {
        // start from an empty family, with no pass rows left from an earlier scenario
        if self.wiped.insert(seed.family) {
            self.data.wipe(&seed.family).await?;
            self.store
                .delete_rows(&seed.family.to_string())
                .await
                .map_err(|err| backend(&err))?;
            self.seeded.remove(&seed.family);
        }
        let variations = seed
            .variants
            .as_ref()
            .map(super::types::VariantSet::variations)
            .filter(|variations| !variations.is_empty())
            .unwrap_or_else(|| vec![super::types::Variation::default()]);
        // write each object, numbering them after the family's earlier ones
        let before = self.seeded.get(&seed.family).copied().unwrap_or(0);
        for offset in 0..seed.count {
            let variation = &variations[offset % variations.len()];
            let record = (fixture.generate)(seed.layout, before + offset, variation);
            self.data.store(&seed.family, &record).await?;
        }
        // the passes' counts are per object, so they start again
        self.seeded.insert(seed.family, before + seed.count);
        self.counts.remove(&seed.family);
        Ok(format!("wrote {} objects of {}", seed.count, seed.family))
    }

    /// Break objects of a family
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    /// * `keys` - The objects to break, or `None` for every object
    /// * `kind` - How to break them
    /// * `field` - The field to break
    /// * `fixture` - How the family's objects are built, which may break them its own way
    async fn corrupt(
        &self,
        family: &Family,
        keys: Option<&[String]>,
        kind: CorruptionKind,
        field: Option<&crate::models::upgrades::FieldPath>,
        fixture: Option<&FamilyFixture>,
    ) -> Result<usize, HarnessError> {
        // find the objects and break each one
        let records = self.data.read(family).await?;
        let mut broken = 0;
        for record in records {
            let Record::Redis { key, .. } = &record else {
                continue;
            };
            if keys.is_some_and(|keys| !keys.contains(key)) {
                continue;
            }
            let record = match fixture.and_then(|fixture| fixture.corrupt) {
                Some(corrupt) => corrupt(record, kind, field),
                None => break_record(record, kind, field)?,
            };
            self.data.store(family, &record).await?;
            broken += 1;
        }
        // naming an object that doesn't exist is a mistake in the scenario
        if keys.is_some_and(|keys| keys.len() != broken) {
            return Err(HarnessError::Failed(format!(
                "asked to corrupt {} objects of {family} but found {broken}",
                keys.map_or(0, <[String]>::len)
            )));
        }
        Ok(broken)
    }

    /// Rewrite objects of a family with a function
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    /// * `keys` - The objects to rewrite, or `None` for every object
    /// * `with` - Rewrites one object
    async fn modify(
        &self,
        family: &Family,
        keys: Option<&[String]>,
        with: super::types::ModifyFn,
    ) -> Result<usize, HarnessError> {
        // find the objects and rewrite each one
        let mut changed = 0;
        for record in self.data.read(family).await? {
            if keys.is_some_and(|keys| {
                !keys
                    .iter()
                    .any(|key| key == crate::models::schema::record_key(&record))
            }) {
                continue;
            }
            self.data.store(family, &with(record)).await?;
            changed += 1;
        }
        // naming an object that doesn't exist is a mistake in the scenario
        if keys.is_some_and(|keys| keys.len() != changed) {
            return Err(HarnessError::Failed(format!(
                "asked to rewrite {} objects of {family} but found {changed}",
                keys.map_or(0, <[String]>::len)
            )));
        }
        Ok(changed)
    }

    /// Judge every object of a family with a check's function
    ///
    /// # Arguments
    ///
    /// * `check` - The check
    async fn check_records(
        &self,
        check: &super::types::RecordsCheck,
    ) -> Result<String, HarnessError> {
        // read the family's objects and let the function judge them
        let records = self.data.read(&check.family).await?;
        (check.check)(&records).map_err(|why| {
            HarnessError::Failed(format!(
                "{} of {} doesn't hold: {why}",
                check.name, check.family
            ))
        })?;
        Ok(format!(
            "{} of {} holds for {} objects",
            check.name,
            check.family,
            records.len()
        ))
    }

    /// The steps an item runs
    ///
    /// # Arguments
    ///
    /// * `item` - The item
    fn item_steps(item: &PlanItem) -> Vec<&PlannedStep> {
        // every step of every pass, or the converge's or single step
        match item {
            PlanItem::Converge { steps, .. } => steps.iter().collect(),
            PlanItem::Passes(passes) => passes.iter().flat_map(|pass| &pass.steps).collect(),
            PlanItem::Step(step) => vec![step],
        }
    }

    /// Record a step as finished in the history
    ///
    /// # Arguments
    ///
    /// * `step` - The planned step
    /// * `chain` - The step ids of the pass the step ran in, empty for a step run as is
    /// * `issues` - What the pass's issue groups came to
    fn record(&mut self, step: &PlannedStep, chain: Vec<String>, issues: Option<IssueSummary>) {
        // an approval names the backup, read before a lossy step's approval is used up
        let backup = self
            .gate
            .approval(&step.step)
            .map(|approval| approval.backup.clone())
            .filter(|backup| !backup.is_empty());
        let backup = backup.filter(|_| step.kind.needs_approval() || step.loss.is_some());
        // a lossy step uses up the code it ran with
        let confirm = if step.loss.is_some() {
            self.gate.ran(Some(&step.step))
        } else {
            None
        };
        self.state.record(AppliedStep {
            revision: step.revision.clone(),
            step: step.step.clone(),
            outcome: StepState::Done,
            message: None,
            at: Some(Utc::now().to_rfc3339()),
            by: Some(HOLDER.to_owned()),
            backup,
            confirm,
            chain,
            issues,
            loss: step.loss.clone(),
            ..AppliedStep::default()
        });
    }

    /// Run one pass through the migration runner
    ///
    /// # Arguments
    ///
    /// * `pass` - The planned pass
    async fn run_planned_pass(&mut self, pass: &PlannedPass) -> Result<ItemEnd, HarnessError> {
        // resolve the chain's transforms from the catalog
        let chain = self.catalog.chain(pass).ok_or_else(|| {
            HarnessError::Failed(format!("the pass over {} can't be resolved", pass.family))
        })?;
        let first = pass.steps.first().ok_or_else(|| {
            HarnessError::Failed(format!("the pass over {} has no steps", pass.family))
        })?;
        // an approval can accept issue groups by their reason codes
        let approvals: Vec<&UpgradeApproval> = pass
            .steps
            .iter()
            .filter_map(|step| self.gate.approval(&step.step))
            .collect();
        let accept: BTreeSet<String> = approvals
            .iter()
            .flat_map(|approval| approval.accept_issues.iter().cloned())
            .collect();
        let backup = approvals
            .iter()
            .map(|approval| approval.backup.clone())
            .find(|backup| !backup.is_empty());
        // the strictest step's limit applies to the whole pass
        let group_max = pass
            .steps
            .iter()
            .filter_map(|step| self.catalog.step(&step.step))
            .map(|(_, def)| def.group_max)
            .min()
            .unwrap_or(crate::models::upgrades::DEFAULT_GROUP_MAX);
        let key = pass.family.to_string();
        let spec = PassSpec {
            key: key.clone(),
            migration: first.step.clone(),
            transform: first.step.clone(),
            revision: pass.chain.last().map(|link| link.revision.to_string()),
            chain: pass.chain_ids(),
            group_max,
            accept: accept.clone(),
            job: None,
            backup,
            families: vec![key],
            has_views: false,
        };
        // run the pass under the writer lease, as a Job does
        let stop_after = self.stop_after.take();
        let runner = ChainRunner::new(chain, pass.family, stop_after);
        let mut ctx = RunContext::new(
            RunMode::Run,
            self.backends.clone(),
            CancellationToken::new(),
        );
        // an interrupted pass reads small batches, so there is more than one to stop between
        if stop_after.is_some() {
            ctx.limits.batch_rows = INTERRUPTED_BATCH_ROWS;
        }
        let lease =
            WriterLease::acquire(&self.backends.redis, &self.backends.namespace, HOLDER, true)
                .await
                .map_err(|err| HarnessError::Backend(err.to_string()))?;
        let options = RunOptions {
            slices: 1,
            ..RunOptions::default()
        };
        let outcome = run_pass(&runner, &spec, &options, &ctx, &self.store, &lease).await;
        lease.release().await;
        let outcome = outcome.map_err(|err| backend(&err))?;
        // a resumed attempt's counts include the earlier runs of the attempt, so each attempt keeps
        // its latest
        self.counts
            .entry(pass.family)
            .or_default()
            .insert(outcome.attempt.clone(), outcome.counts);
        if outcome.interrupted {
            return Ok(ItemEnd::Interrupted);
        }
        // a failed pass leaves its steps unfinished, with the failure in the history
        if outcome.status == MigrationRunStatus::Failed {
            for step in &pass.steps {
                self.state.record(AppliedStep {
                    revision: step.revision.clone(),
                    step: step.step.clone(),
                    outcome: StepState::Failed,
                    message: outcome.failure.clone(),
                    at: Some(Utc::now().to_rfc3339()),
                    by: Some(HOLDER.to_owned()),
                    chain: pass.chain_ids(),
                    ..AppliedStep::default()
                });
            }
            return Ok(ItemEnd::Failed);
        }
        // every step of the pass finished, with the issue groups the pass recorded
        let issues = (!outcome.groups.is_empty()).then(|| IssueSummary {
            groups: u32::try_from(outcome.groups.len()).unwrap_or(u32::MAX),
            objects: outcome.groups.iter().map(|group| group.count).sum(),
            accepted: outcome
                .groups
                .iter()
                .filter(|group| group.accepted)
                .map(|group| group.reason.clone())
                .collect(),
        });
        for step in &pass.steps {
            self.record(step, pass.chain_ids(), issues.clone());
        }
        Ok(ItemEnd::Done)
    }

    /// Run one item of the plan
    ///
    /// # Arguments
    ///
    /// * `item` - The item, which the gate has let run
    async fn run_item(&mut self, item: &PlanItem) -> Result<(ItemEnd, String), HarnessError> {
        // each kind of item records its steps in its own way
        let text = match item {
            PlanItem::Converge { target, steps } => {
                for step in steps {
                    self.record(step, Vec::new(), None);
                }
                format!(
                    "ran converge to {target} ({})",
                    steps
                        .iter()
                        .map(|step| step.step.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            PlanItem::Step(step) => {
                self.record(step, Vec::new(), None);
                format!("ran step {}", step.step)
            }
            PlanItem::Passes(passes) => {
                let mut ran = Vec::new();
                for pass in passes {
                    match self.run_planned_pass(pass).await? {
                        ItemEnd::Done => ran.push(pass.family.to_string()),
                        ItemEnd::Failed => {
                            return Ok((ItemEnd::Failed, format!("pass {} failed", pass.family)));
                        }
                        ItemEnd::Interrupted => {
                            return Ok((
                                ItemEnd::Interrupted,
                                format!("pass {} was interrupted", pass.family),
                            ));
                        }
                    }
                }
                format!("ran pass {}", ran.join(", "))
            }
        };
        // anything that ran changes the history, so a lossy step blocked later shows a new code
        self.gate.ran(None);
        Ok((ItemEnd::Done, text))
    }

    /// Record the steps a finished step supersedes as skipped, and move the revision past every
    /// edge whose steps finished
    fn after_item(&mut self) {
        // a dropped step is skipped once the step superseding it is finished
        let pending = std::mem::take(&mut self.superseded);
        for (dropped, by) in pending {
            let finished = self
                .state
                .applied
                .iter()
                .any(|applied| applied.step == by && applied.outcome == StepState::Done);
            if finished {
                self.state.record(AppliedStep {
                    revision: dropped.revision.clone(),
                    step: dropped.step.clone(),
                    outcome: StepState::Skipped,
                    message: Some(format!("superseded by {by}")),
                    at: Some(Utc::now().to_rfc3339()),
                    by: Some(HOLDER.to_owned()),
                    ..AppliedStep::default()
                });
            } else {
                self.superseded.push((dropped, by));
            }
        }
        // the revision moves past each edge with nothing left to run
        if let Some(plan) = &self.plan {
            let target = plan.to.clone();
            self.catalog.advance(&mut self.state, &target, VERSION);
        }
    }

    /// Run the plan until an item waits, the plan ends, or one item has run
    ///
    /// # Arguments
    ///
    /// * `one` - Whether to stop after one item
    async fn settle(&mut self, one: bool) -> Result<(Settled, String), HarnessError> {
        let mut ran = Vec::new();
        loop {
            // the next item, if the plan has one
            let Some(item) = self
                .plan
                .as_ref()
                .and_then(|plan| plan.items.first())
                .cloned()
            else {
                self.blocked = None;
                if self.plan.is_some() {
                    self.after_item();
                    self.plan = None;
                }
                let text = if ran.is_empty() {
                    "nothing left to run".to_owned()
                } else {
                    ran.join("; ")
                };
                return Ok((
                    if ran.is_empty() {
                        Settled::Finished
                    } else {
                        Settled::Ran
                    },
                    text,
                ));
            };
            // the gate decides whether the item may run
            let steps = Self::item_steps(&item);
            let decision = self.gate.evaluate(
                &steps,
                self.state.revision.as_str(),
                self.state.applied.len(),
            );
            if let GateDecision::Blocked {
                step,
                reason,
                message,
            } = decision
            {
                let text = format!("blocked on {step}: {message}");
                self.blocked = Some(Blocked {
                    step,
                    reason,
                    message,
                });
                return Ok((Settled::Blocked, text));
            }
            // run the item and drop it from the plan when its steps finished
            self.blocked = None;
            let (end, text) = self.run_item(&item).await?;
            match end {
                ItemEnd::Done => {
                    if let Some(plan) = &mut self.plan {
                        plan.items.remove(0);
                    }
                    self.after_item();
                    ran.push(text);
                    if one {
                        return Ok((Settled::Ran, ran.join("; ")));
                    }
                }
                ItemEnd::Failed => return Ok((Settled::Failed, text)),
                ItemEnd::Interrupted => return Ok((Settled::Interrupted, text)),
            }
        }
    }

    /// Run the plan to its next gate or its end, for an action that observes the outcome
    async fn settle_all(&mut self) -> Result<String, HarnessError> {
        // run everything that can run
        let (_, text) = self.settle(false).await?;
        Ok(text)
    }

    /// Check that the plan is blocked as expected
    ///
    /// # Arguments
    ///
    /// * `step` - The step it must be blocked on
    /// * `reason` - Why
    /// * `message` - Text the status must contain
    fn expect_blocked(
        &self,
        step: &str,
        reason: &BlockReason,
        message: Option<&String>,
    ) -> Result<(), HarnessError> {
        // the plan must be waiting
        let Some(blocked) = &self.blocked else {
            return Err(HarnessError::Failed(format!(
                "expected the plan to be blocked on {step}, but it isn't blocked (revision {}, {} steps in the history)",
                self.state.revision,
                self.state.applied.len()
            )));
        };
        // on this step, for this reason
        if blocked.step != step {
            return Err(HarnessError::Failed(format!(
                "expected the plan to be blocked on {step}, but it is blocked on {}",
                blocked.step
            )));
        }
        if !reasons_match(reason, &blocked.reason) {
            return Err(HarnessError::Failed(format!(
                "expected {step} to be blocked for {}, but it is blocked for {}",
                super::types::render_reason(reason),
                super::types::render_reason(&blocked.reason)
            )));
        }
        // with the expected text in its message
        if let Some(message) = message
            && !blocked.message.contains(message.as_str())
        {
            return Err(HarnessError::Failed(format!(
                "expected the block message to contain {message:?}, but it is {:?}",
                blocked.message
            )));
        }
        Ok(())
    }

    /// Run a check
    ///
    /// # Arguments
    ///
    /// * `check` - The check
    /// * `scenario` - The scenario, for its fixtures
    async fn check(&mut self, check: &Check, scenario: &Scenario) -> Result<String, HarnessError> {
        // each check reads what it needs
        match check {
            Check::Fixtures(verifier) => self.check_fixtures(verifier, scenario).await,
            Check::Issues { migration, groups } => self.check_issues(migration, groups).await,
            Check::History(expected) => check_history(&self.state.applied, expected),
            Check::Passes(counts) => self.check_passes(counts),
            Check::SameDataAs(label) => self.check_same(label, scenario).await,
            Check::Condition { kind, present } => {
                // only the refused dev setting is a condition the harness raises
                let raised = matches!(kind, UpgradeConditionType::LossySettingRefused)
                    && self.gate.setting_refused();
                if raised == *present {
                    Ok(format!("condition {kind:?} present {raised}"))
                } else {
                    Err(HarnessError::Failed(format!(
                        "expected condition {kind:?} present {present}, but it is {raised}"
                    )))
                }
            }
            Check::Records(check) => self.check_records(check).await,
            Check::Drift(_) => Err(HarnessError::Unsupported(
                "drift checks need the probe of the drift stage".to_owned(),
            )),
        }
    }

    /// Check every fixture object of a family
    ///
    /// # Arguments
    ///
    /// * `verifier` - What the objects must look like
    /// * `scenario` - The scenario, for the family's fixture
    async fn check_fixtures(
        &self,
        verifier: &FixtureVerifier,
        scenario: &Scenario,
    ) -> Result<String, HarnessError> {
        // the family's fixture says which layout an object is in
        let fixture = scenario.fixture(&verifier.family).ok_or_else(|| {
            HarnessError::Failed(format!("no fixture is registered for {}", verifier.family))
        })?;
        let records = self.data.read(&verifier.family).await?;
        // no object may be lost or invented
        if let Some(expected) = self.seeded.get(&verifier.family)
            && *expected != records.len()
        {
            return Err(HarnessError::Failed(format!(
                "{} holds {} objects but {expected} were seeded",
                verifier.family,
                records.len()
            )));
        }
        // every object is in the layout or holds its sentinels, except the flagged ones
        verify_layouts(&records, fixture, verifier)?;
        // every flagged object is among its issue group's samples, unless the groups keep only
        // some of their keys
        if !verifier.flagged.is_empty() {
            let groups = issue_groups(&self.store, &verifier.family.to_string())
                .await
                .map_err(|err| backend(&err))?;
            let samples: BTreeSet<&String> =
                groups.iter().flat_map(|group| &group.samples).collect();
            let truncated = groups.iter().any(|group| group.count > SAMPLE_KEYS as u64);
            for key in &verifier.flagged {
                if !samples.contains(key) && !truncated {
                    return Err(HarnessError::Failed(format!(
                        "{key} is flagged but isn't among any issue group's samples"
                    )));
                }
            }
        }
        Ok(format!(
            "{} objects of {} in layout {}",
            records.len(),
            verifier.family,
            verifier.layout.0
        ))
    }

    /// Check a pass's issue groups
    ///
    /// # Arguments
    ///
    /// * `migration` - The pass key
    /// * `expected` - The reason codes expected and their counts
    async fn check_issues(
        &self,
        migration: &str,
        expected: &[(String, u64)],
    ) -> Result<String, HarnessError> {
        // sum each reason's objects across steps and workers
        let groups = issue_groups(&self.store, migration)
            .await
            .map_err(|err| backend(&err))?;
        let mut found: BTreeMap<String, u64> = BTreeMap::new();
        for group in &groups {
            *found.entry(group.reason.clone()).or_default() += group.count;
        }
        // the groups must be exactly the expected ones
        let expected: BTreeMap<String, u64> = expected.iter().cloned().collect();
        if found == expected {
            Ok(format!("{} issue groups of {migration}", found.len()))
        } else {
            Err(HarnessError::Failed(format!(
                "the issue groups of {migration} are {found:?}, but {expected:?} were expected"
            )))
        }
    }

    /// Check how often the passes of a family touched each object
    ///
    /// # Arguments
    ///
    /// * `check` - The reads and writes per object expected
    fn check_passes(&self, check: &PassCountCheck) -> Result<String, HarnessError> {
        // the counts are totals over the objects seeded
        let objects = self.seeded.get(&check.family).copied().unwrap_or(0) as u64;
        let mut counts = PassCounts::default();
        for attempt in self
            .counts
            .get(&check.family)
            .into_iter()
            .flat_map(BTreeMap::values)
        {
            counts.read += attempt.read;
            counts.written += attempt.written;
        }
        let (reads, writes) = (
            check.reads_per_object * objects,
            check.writes_per_object * objects,
        );
        if counts.read == reads && counts.written == writes {
            Ok(format!(
                "{} reads and {} writes",
                counts.read, counts.written
            ))
        } else {
            Err(HarnessError::Failed(format!(
                "the passes over {} read {} and wrote {} of {objects} objects, but {} reads and {} writes per object ({reads} and {writes}) were expected",
                check.family,
                counts.read,
                counts.written,
                check.reads_per_object,
                check.writes_per_object
            )))
        }
    }

    /// Snapshot every family the scenario registered
    ///
    /// # Arguments
    ///
    /// * `scenario` - The scenario, for its families
    async fn snapshot(
        &self,
        scenario: &Scenario,
    ) -> Result<BTreeMap<String, Snapshot>, HarnessError> {
        // read each registered family
        let mut data = BTreeMap::new();
        for family in scenario.families() {
            let mut snapshot = Snapshot::new();
            for record in self.data.read(family).await? {
                match record {
                    Record::Redis { key, fields } => {
                        snapshot.insert(key, fields);
                    }
                    Record::S3 { key } => {
                        snapshot.insert(key, BTreeMap::new());
                    }
                    Record::Scylla { .. } => (),
                }
            }
            data.insert(family.to_string(), snapshot);
        }
        Ok(data)
    }

    /// Check that the data equals an earlier capture
    ///
    /// # Arguments
    ///
    /// * `label` - The capture to compare with
    /// * `scenario` - The scenario, for its families
    async fn check_same(&self, label: &str, scenario: &Scenario) -> Result<String, HarnessError> {
        // compare the data now with the capture
        let captured = self
            .captures
            .get(label)
            .ok_or_else(|| HarnessError::Failed(format!("nothing was captured as {label}")))?;
        let now = self.snapshot(scenario).await?;
        if &now == captured {
            Ok(format!("the data equals {label}"))
        } else {
            let differing: Vec<&String> = now
                .iter()
                .filter(|(family, snapshot)| captured.get(*family) != Some(*snapshot))
                .map(|(family, _)| family)
                .collect();
            Err(HarnessError::Failed(format!(
                "the data differs from {label} in {differing:?}"
            )))
        }
    }
}

/// Map a migration error to a harness error
///
/// # Arguments
///
/// * `error` - The error the runner or a store gave
fn backend(error: &MigrateError) -> HarnessError {
    // keep the runner's message
    HarnessError::Backend(error.to_string())
}

/// Break one record in the way a corruption kind says
///
/// # Arguments
///
/// * `record` - The record
/// * `kind` - How to break it
/// * `field` - The field to break, or `None` to break every field
fn break_record(
    mut record: Record,
    kind: CorruptionKind,
    field: Option<&crate::models::upgrades::FieldPath>,
) -> Result<Record, HarnessError> {
    // only Redis hashes are broken
    let Record::Redis { fields, .. } = &mut record else {
        return Err(HarnessError::Unsupported(
            "the direct driver corrupts Redis hashes only".to_owned(),
        ));
    };
    match (kind.value(), field) {
        // a field is set to broken text
        (Some(value), Some(field)) => {
            fields.insert(field.0.to_owned(), value.to_owned());
        }
        // a field is removed
        (None, Some(field)) => {
            fields.remove(field.0);
        }
        // malformed JSON of the whole record breaks every field
        (Some(value), None) if kind == CorruptionKind::MalformedJson => {
            for stored in fields.values_mut() {
                value.clone_into(stored);
            }
        }
        // any other kind needs a field to break
        (_, None) => {
            return Err(HarnessError::Failed(format!(
                "{kind:?} corruptions need a field to break"
            )));
        }
    }
    Ok(record)
}

impl DirectDriver {
    /// Plan the work that moves the deployment to a revision, or record why it is refused
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision to reach
    fn plan_target(&mut self, revision: &str) -> Result<String, HarnessError> {
        // the revision must be well formed
        let target: RevisionId = revision
            .parse()
            .map_err(|err| HarnessError::Failed(format!("{revision}: {err:?}")))?;
        // keep the plan, or the reason the catalog refuses the path
        self.blocked = None;
        match self.catalog.compose(&self.state, &target) {
            Ok(plan) => {
                self.superseded = self
                    .catalog
                    .superseded(&self.state, &target)
                    .unwrap_or_default();
                self.refused = None;
                let text = format!("planned {} items to {target}", plan.items.len());
                self.plan = Some(plan);
                Ok(text)
            }
            Err(error) => {
                self.plan = None;
                self.refused = Some(error.to_string());
                Ok(format!("refused: {error}"))
            }
        }
    }

    /// Check that the plan was refused, naming a step
    ///
    /// # Arguments
    ///
    /// * `step` - The step the refusal must name
    fn expect_refused(&self, step: &str) -> Result<String, HarnessError> {
        // the refusal came when the target was planned
        match &self.refused {
            Some(text) if text.contains(step) => Ok(format!("refused: {text}")),
            Some(text) => Err(HarnessError::Failed(format!(
                "the refusal {text:?} doesn't name {step}"
            ))),
            None => Err(HarnessError::Failed(format!(
                "expected the plan to be refused naming {step}, but it was planned"
            ))),
        }
    }

    /// Check that a lossy step refused its confirmation for a reason and shows a new code
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `refusal` - Why its code must be refused
    async fn expect_confirm_refused(
        &mut self,
        step: &str,
        refusal: ConfirmRefusal,
    ) -> Result<String, HarnessError> {
        // run on to the block, which must be this refusal
        let text = self.settle_all().await?;
        let reason = BlockReason::ConfirmationRefused {
            confirm: String::new(),
            refusal,
        };
        self.expect_blocked(step, &reason, None)?;
        // the status shows a current code, different from the one refused
        let shown = self.gate.current_code(step).unwrap_or_default().to_owned();
        let refused = self
            .gate
            .approval(step)
            .and_then(|approval| approval.confirm.clone());
        if shown.is_empty() || refused.as_deref() == Some(shown.as_str()) {
            return Err(HarnessError::Failed(format!(
                "expected {step} to show a new confirmation code, but it shows {shown:?} after \
                 refusing {refused:?}"
            )));
        }
        Ok(text)
    }

    /// Give a lossy approval with a code from the blocked step's status, the last approval, or
    /// the scenario
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `backup` - The backup the approval names, if any
    /// * `confirm` - Where the code comes from
    fn approve_loss(
        &mut self,
        step: &str,
        backup: Option<&BackupRef>,
        confirm: &ConfirmSource,
    ) -> Result<String, HarnessError> {
        // find the code
        let code = match confirm {
            ConfirmSource::Current => self.gate.current_code(step).map(str::to_owned).ok_or_else(
                || {
                    HarnessError::Failed(format!(
                        "{step} isn't blocked on its loss, so it shows no confirmation code to read"
                    ))
                },
            )?,
            ConfirmSource::Previous => self
                .gate
                .previous_code(step)
                .map(str::to_owned)
                .ok_or_else(|| {
                    HarnessError::Failed(format!(
                        "no confirmation code was given for {step} before"
                    ))
                })?,
            ConfirmSource::Literal(code) => code.clone(),
        };
        // give the approval and remember its code
        self.gate.remember(step, &code);
        self.gate.approve(loss_approval(step, backup, code));
        Ok(format!("approved the loss of {step}"))
    }

    /// Approve every gated step of the plan that isn't lossy, keeping an approval already given
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup the approvals name
    fn approve_all(&mut self, backup: &BackupRef) -> String {
        // every step of the plan that needs an approval and loses nothing
        let steps: Vec<String> = self
            .plan
            .iter()
            .flat_map(|plan| &plan.items)
            .flat_map(Self::item_steps)
            .filter(|step| step.kind.needs_approval() && step.loss.is_none())
            .map(|step| step.step.clone())
            .collect();
        for step in &steps {
            if self.gate.approval(step).is_none() {
                self.gate.approve(UpgradeApproval {
                    step: step.clone(),
                    backup: backup.0.clone(),
                    ..UpgradeApproval::default()
                });
            }
        }
        format!("approved {} steps", steps.len())
    }

    /// Inject a failure
    ///
    /// # Arguments
    ///
    /// * `injection` - What to inject
    /// * `scenario` - The scenario, for its fixtures
    async fn inject(
        &mut self,
        injection: &Injection,
        scenario: &Scenario,
    ) -> Result<String, HarnessError> {
        // each injection that runs in-process
        match injection {
            Injection::CorruptObjects {
                family,
                objects,
                kind,
                field,
            } => {
                let fixture = scenario.fixture(family);
                let broken = self
                    .corrupt(family, Some(objects), *kind, field.as_ref(), fixture)
                    .await?;
                Ok(format!("corrupted {broken} objects of {family}"))
            }
            Injection::SystematicBreak { family, field } => {
                let fixture = scenario.fixture(family);
                let kind = CorruptionKind::BadType;
                let broken = self
                    .corrupt(family, None, kind, Some(field), fixture)
                    .await?;
                Ok(format!("broke {} on {broken} objects of {family}", field.0))
            }
            Injection::InterruptAtBatch {
                batch,
                signal: Interrupt::Term,
            } => {
                self.stop_after = Some(*batch);
                Ok(format!("the next pass stops after batch {batch}"))
            }
            Injection::Modify { family, keys, with } => {
                let changed = self.modify(family, keys.as_deref(), *with).await?;
                Ok(format!("rewrote {changed} objects of {family}"))
            }
            Injection::InterruptAtBatch { .. }
            | Injection::BackendOutage { .. }
            | Injection::Drift { .. } => Err(HarnessError::Unsupported(format!(
                "the direct driver can't inject {injection:?}"
            ))),
        }
    }
}

#[async_trait]
impl super::ScenarioDriver for DirectDriver {
    /// This is the direct driver
    fn kind(&self) -> DriverKind {
        // always direct
        DriverKind::Direct
    }

    /// Run one action
    ///
    /// Moving to a revision plans the work and runs nothing. The deployment runs on when an
    /// action observes the outcome (an expectation) or `step` asks for one item, so a scenario
    /// can approve several steps and then see where the plan stops.
    ///
    /// # Arguments
    ///
    /// * `action` - The action to run
    /// * `scenario` - The scenario the action belongs to
    async fn run(
        &mut self,
        action: &ScenarioAction,
        scenario: &Scenario,
    ) -> Result<ActionRecord, HarnessError> {
        // run the action and say what happened
        let outcome = match action {
            ScenarioAction::Seed(seed) => {
                let fixture = scenario.fixture(&seed.family).ok_or_else(|| {
                    HarnessError::Failed(format!("no fixture is registered for {}", seed.family))
                })?;
                self.seed(seed, fixture).await?
            }
            ScenarioAction::Target(revision) => self.plan_target(revision)?,
            ScenarioAction::ExpectRevision(revision) => {
                let text = self.settle_all().await?;
                if self.state.revision.as_str() != revision {
                    return Err(HarnessError::Failed(format!(
                        "expected revision {revision} but the deployment is at {} ({text})",
                        self.state.revision
                    )));
                }
                text
            }
            ScenarioAction::ExpectBlocked {
                step,
                reason,
                message,
            } => {
                let text = self.settle_all().await?;
                self.expect_blocked(step, reason, message.as_ref())?;
                text
            }
            ScenarioAction::ExpectRefused { step } => self.expect_refused(step)?,
            ScenarioAction::ExpectConfirmRefused { step, refusal } => {
                self.expect_confirm_refused(step, *refusal).await?
            }
            ScenarioAction::Approve(approval) => {
                self.gate.approve(approval.clone());
                format!("approved {}", approval.step)
            }
            ScenarioAction::ApproveLoss {
                step,
                backup,
                confirm,
            } => self.approve_loss(step, backup.as_ref(), confirm)?,
            ScenarioAction::ApproveAll(backup) => self.approve_all(backup),
            ScenarioAction::SetDevLossy(on) => {
                self.gate.set_dev_lossy(*on);
                format!("dangerous downgrade lossy is {on}")
            }
            ScenarioAction::Step => self.settle(true).await?.1,
            ScenarioAction::Check(check) => self.check(check, scenario).await?,
            ScenarioAction::Capture(label) => {
                let data = self.snapshot(scenario).await?;
                self.captures.insert(label.clone(), data);
                format!("captured {label}")
            }
            ScenarioAction::Inject(injection) => self.inject(injection, scenario).await?,
            ScenarioAction::Deploy(_) | ScenarioAction::Restart(_) => {
                return Err(HarnessError::ClusterOnly {
                    scenario: scenario.name().to_owned(),
                    action: action.render(),
                });
            }
        };
        Ok(ActionRecord {
            action: action.render(),
            outcome,
        })
    }
}
