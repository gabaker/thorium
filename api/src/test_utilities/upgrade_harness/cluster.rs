//! The cluster driver: scenarios against a minithor dev cluster
//!
//! The driver talks to the cluster through `kubectl` and `minithor`, which a dev VM has, and
//! reads the data through the config `THORIUM_TEST_CONFIG` names. It patches the
//! `ThoriumCluster`'s `spec.upgrade` (target revision, approvals, the dev setting), watches
//! `status.upgrade`, and runs verifiers between the changes. It can't stop before a step: the
//! operator has no setting for that, so checks happen at gates and after completion, and the
//! step-by-step history is read from the upgrade state `ConfigMap`.

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::data::{RedisData, verify_layouts};
use super::types::{Check, HistoryExpectation, reasons_match};
use super::{
    ActionRecord, BackupRef, BlockReason, ConfirmSource, DriverKind, FixtureSeed, HarnessError,
    RestartTarget, Scenario, ScenarioAction, ScenarioDriver, VariantSet, Variation, loss_approval,
};
use crate::models::upgrades::{
    AppliedStep, ConfirmRefusal, STATE_CONFIG_MAP, STATE_KEY, StepState, UpgradeApproval,
    UpgradeState, UpgradeStatus,
};
use crate::test_utilities::{CONF, read_only, redis_pool};

/// How often the cluster's status is read while waiting for a change
const POLL: Duration = Duration::from_secs(3);

/// Runs scenarios against a minithor dev cluster
pub struct ClusterDriver {
    /// The cluster's fixture objects, read through `THORIUM_TEST_CONFIG`
    data: RedisData,
    /// The namespace the `ThoriumCluster` is in
    namespace: String,
    /// The `ThoriumCluster`'s name
    cluster: String,
    /// The `kubectl` to run, so a profile's context can be named
    kubectl: Vec<String>,
    /// The command that deploys an image, with `{image}` standing for the image
    deploy_command: Vec<String>,
    /// The operator's Deployment, which `restart` restarts
    operator: String,
    /// The label selector of a migration Job's pods, which `restart` deletes
    job_pods: String,
    /// How long a wait for a status change lasts
    wait: Duration,
    /// The code last given for each lossy step
    given: BTreeMap<String, String>,
}

impl ClusterDriver {
    /// Reach the dev cluster whose databases `THORIUM_TEST_CONFIG` names
    ///
    /// # Arguments
    ///
    /// * `namespace` - The namespace the `ThoriumCluster` is in
    /// * `cluster` - The `ThoriumCluster`'s name
    pub async fn connect(namespace: &str, cluster: &str) -> Result<Self, HarnessError> {
        // the cluster's databases are only read through an explicit config
        if !read_only() {
            return Err(HarnessError::Unsupported(
                "the cluster driver reads the cluster's databases through the config \
                 THORIUM_TEST_CONFIG names, which isn't set"
                    .to_owned(),
            ));
        }
        // the defaults suit a minithor deployment
        Ok(ClusterDriver {
            data: RedisData::new(redis_pool().await, &CONF.thorium.namespace),
            namespace: namespace.to_owned(),
            cluster: cluster.to_owned(),
            kubectl: vec!["kubectl".to_owned()],
            deploy_command: ["minithor", "deploy", "--local-image", "{image}"]
                .map(str::to_owned)
                .to_vec(),
            operator: "operator".to_owned(),
            job_pods: "thorium.sandia.gov/job-kind=migration".to_owned(),
            wait: Duration::from_mins(15),
            given: BTreeMap::new(),
        })
    }

    /// Use a different `kubectl` command line, such as one that names a context
    ///
    /// # Arguments
    ///
    /// * `kubectl` - The command and its leading arguments
    #[must_use]
    pub fn with_kubectl(mut self, kubectl: Vec<String>) -> Self {
        // keep the command as given
        self.kubectl = kubectl;
        self
    }

    /// Use a different command to deploy an image
    ///
    /// # Arguments
    ///
    /// * `command` - The command and its arguments, with `{image}` standing for the image
    #[must_use]
    pub fn with_deploy_command(mut self, command: Vec<String>) -> Self {
        // keep the command as given
        self.deploy_command = command;
        self
    }

    /// Wait for a status change this long
    ///
    /// # Arguments
    ///
    /// * `wait` - How long
    #[must_use]
    pub fn with_wait(mut self, wait: Duration) -> Self {
        // keep the limit
        self.wait = wait;
        self
    }

    /// Run a command and return its output
    ///
    /// # Arguments
    ///
    /// * `command` - The program and its arguments
    async fn exec(command: Vec<String>) -> Result<String, HarnessError> {
        // run it off the async threads, since it blocks until it exits
        let text = command.join(" ");
        tokio::task::spawn_blocking(move || {
            let (program, args) = command
                .split_first()
                .ok_or_else(|| HarnessError::Backend("an empty command".to_owned()))?;
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .map_err(|err| HarnessError::Backend(format!("{program}: {err}")))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                Err(HarnessError::Backend(format!(
                    "{program} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )))
            }
        })
        .await
        .map_err(|err| HarnessError::Backend(format!("{text}: {err}")))?
    }

    /// Run `kubectl` with arguments
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `kubectl`
    async fn kubectl(&self, args: &[&str]) -> Result<String, HarnessError> {
        // the configured command, then the arguments
        let mut command = self.kubectl.clone();
        command.extend(args.iter().map(|arg| (*arg).to_owned()));
        Self::exec(command).await
    }

    /// Read the `ThoriumCluster` as JSON
    async fn cluster_json(&self) -> Result<serde_json::Value, HarnessError> {
        // get the object
        let raw = self
            .kubectl(&[
                "get",
                "thoriumcluster",
                &self.cluster,
                "-n",
                &self.namespace,
                "-o",
                "json",
            ])
            .await?;
        serde_json::from_str(&raw).map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// Read `status.upgrade`
    async fn status(&self) -> Result<UpgradeStatus, HarnessError> {
        // a cluster that hasn't reported yet has an empty status
        let cluster = self.cluster_json().await?;
        let upgrade = cluster
            .pointer("/status/upgrade")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        serde_json::from_value(upgrade).map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// Read the approvals in `spec.upgrade`
    async fn approvals(&self) -> Result<Vec<UpgradeApproval>, HarnessError> {
        // an unset list is empty
        let cluster = self.cluster_json().await?;
        let approvals = cluster
            .pointer("/spec/upgrade/approvals")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        serde_json::from_value(approvals).map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// Merge fields into `spec.upgrade`
    ///
    /// # Arguments
    ///
    /// * `fields` - The fields to set
    async fn patch(&self, fields: serde_json::Value) -> Result<(), HarnessError> {
        // a merge patch replaces a list whole, so approvals are always written complete
        let patch = serde_json::json!({"spec": {"upgrade": fields}}).to_string();
        self.kubectl(&[
            "patch",
            "thoriumcluster",
            &self.cluster,
            "-n",
            &self.namespace,
            "--type",
            "merge",
            "-p",
            &patch,
        ])
        .await
        .map(|_| ())
    }

    /// Add or replace an approval
    ///
    /// # Arguments
    ///
    /// * `approval` - The approval to give
    async fn approve(&self, approval: UpgradeApproval) -> Result<(), HarnessError> {
        // replace the step's earlier approval and write the list back
        let mut approvals = self.approvals().await?;
        approvals.retain(|given| given.step != approval.step);
        approvals.push(approval);
        self.patch(serde_json::json!({"approvals": approvals}))
            .await
    }

    /// Wait until the status satisfies a condition
    ///
    /// # Arguments
    ///
    /// * `what` - What is waited for, for the timeout message
    /// * `ready` - Whether the status is what is waited for
    async fn wait_for<F: Fn(&UpgradeStatus) -> bool>(
        &self,
        what: &str,
        ready: F,
    ) -> Result<UpgradeStatus, HarnessError> {
        // poll until the condition holds or the wait runs out
        let started = Instant::now();
        loop {
            let status = self.status().await?;
            if ready(&status) {
                return Ok(status);
            }
            if started.elapsed() > self.wait {
                return Err(HarnessError::Failed(format!(
                    "timed out waiting for {what}; the status is at {:?} with steps {:?}",
                    status.current,
                    status
                        .steps
                        .iter()
                        .map(|step| format!("{}:{:?}", step.step, step.state))
                        .collect::<Vec<_>>()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Read the applied history from the upgrade state `ConfigMap`
    async fn history(&self) -> Result<UpgradeState, HarnessError> {
        // the state is JSON under one key
        let template = format!("go-template={{{{index .data \"{STATE_KEY}\"}}}}");
        let raw = self
            .kubectl(&[
                "get",
                "configmap",
                STATE_CONFIG_MAP,
                "-n",
                &self.namespace,
                "-o",
                &template,
            ])
            .await?;
        serde_json::from_str(&raw).map_err(|err| HarnessError::Backend(err.to_string()))
    }

    /// The confirmation code a blocked lossy step shows
    ///
    /// # Arguments
    ///
    /// * `status` - The status
    /// * `step` - The step
    fn code_of(status: &UpgradeStatus, step: &str) -> Option<String> {
        // a blocked step shows its code in its block reason
        status
            .steps
            .iter()
            .find(|planned| planned.step == step)
            .and_then(|planned| match planned.blocked.as_ref()? {
                BlockReason::LossAcceptance { confirm }
                | BlockReason::ConfirmationRefused { confirm, .. } => Some(confirm.clone()),
                _ => None,
            })
    }

    /// Write fixture objects into the cluster's Redis
    ///
    /// # Arguments
    ///
    /// * `seed` - What to write
    /// * `scenario` - The scenario, for the family's fixture
    async fn seed(&self, seed: &FixtureSeed, scenario: &Scenario) -> Result<String, HarnessError> {
        // the family's fixture builds each object
        let fixture = scenario.fixture(&seed.family).ok_or_else(|| {
            HarnessError::Failed(format!("no fixture is registered for {}", seed.family))
        })?;
        let variations = seed
            .variants
            .as_ref()
            .map(VariantSet::variations)
            .filter(|variations| !variations.is_empty())
            .unwrap_or_else(|| vec![Variation::default()]);
        // write each object, spreading the variations over them
        for index in 0..seed.count {
            let variation = &variations[index % variations.len()];
            let record = (fixture.generate)(seed.layout, index, variation);
            self.data.store(&record).await?;
        }
        Ok(format!("wrote {} objects of {}", seed.count, seed.family))
    }

    /// Check that a condition of the status names a step, which is how a refused plan shows
    ///
    /// # Arguments
    ///
    /// * `step` - The step the refusal must name
    async fn expect_refused(&self, step: &str) -> Result<String, HarnessError> {
        // the conditions carry the refusal's message
        let status = self.status().await?;
        let message = status
            .conditions
            .iter()
            .map(|condition| condition.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        if !message.contains(step) {
            return Err(HarnessError::Failed(format!(
                "no refusal names {step}; the conditions say {message:?}"
            )));
        }
        Ok(format!("refused: {message}"))
    }

    /// Check that a lossy step refused its confirmation for a reason and shows a new code
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `refusal` - Why its code must be refused
    async fn expect_confirm_refused(
        &self,
        step: &str,
        refusal: ConfirmRefusal,
    ) -> Result<String, HarnessError> {
        // wait for the refusal to show in the status
        let status = self
            .wait_for(&format!("{step} to refuse its confirmation"), |status| {
                status.steps.iter().any(|planned| {
                    planned.step == step
                        && matches!(
                            planned.blocked,
                            Some(BlockReason::ConfirmationRefused { refusal: shown, .. })
                                if shown == refusal
                        )
                })
            })
            .await?;
        // the status shows a code other than the one that was refused
        if Self::code_of(&status, step) == self.given.get(step).cloned() {
            return Err(HarnessError::Failed(format!(
                "{step} still shows the code that was refused"
            )));
        }
        Ok(format!("{step} refused its confirmation: {refusal:?}"))
    }

    /// Give a lossy approval with a code from the blocked step's status, the last approval, or
    /// the scenario
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `backup` - The backup the approval names, if any
    /// * `confirm` - Where the code comes from
    async fn approve_loss(
        &mut self,
        step: &str,
        backup: Option<&BackupRef>,
        confirm: &ConfirmSource,
    ) -> Result<String, HarnessError> {
        // find the code
        let code = match confirm {
            ConfirmSource::Current => {
                Self::code_of(&self.status().await?, step).ok_or_else(|| {
                    HarnessError::Failed(format!("{step} shows no confirmation code"))
                })?
            }
            ConfirmSource::Previous => self.given.get(step).cloned().ok_or_else(|| {
                HarnessError::Failed(format!("no confirmation code was given for {step} before"))
            })?,
            ConfirmSource::Literal(code) => code.clone(),
        };
        // give the approval and remember its code
        self.given.insert(step.to_owned(), code.clone());
        self.approve(loss_approval(step, backup, code)).await?;
        Ok(format!("approved the loss of {step}"))
    }

    /// Approve every gated step of the plan that isn't lossy and has no approval yet
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup the approvals name
    async fn approve_all(&self, backup: &BackupRef) -> Result<String, HarnessError> {
        // add an approval for each such step to the list
        let status = self.status().await?;
        let mut approvals = self.approvals().await?;
        for planned in &status.steps {
            if planned.kind.needs_approval()
                && planned.loss.is_none()
                && !approvals.iter().any(|given| given.step == planned.step)
            {
                approvals.push(UpgradeApproval {
                    step: planned.step.clone(),
                    backup: backup.0.clone(),
                    ..UpgradeApproval::default()
                });
            }
        }
        self.patch(serde_json::json!({"approvals": approvals}))
            .await?;
        Ok(format!("approved {} steps", approvals.len()))
    }

    /// Check that the operator's history holds an entry for each expectation
    ///
    /// The operator's history also holds the entries it writes around every step (`Blocked`,
    /// `Running`, the Job's own `Done` beside its own, the restored components), and the whole run
    /// of a long-lived cluster, so each expectation is met by any entry of its step, unlike the
    /// direct driver's history, which holds only what the scenario ran.
    ///
    /// # Arguments
    ///
    /// * `found` - The history
    /// * `expected` - The entries it must hold
    fn check_history(
        found: &[AppliedStep],
        expected: &[HistoryExpectation],
    ) -> Result<String, HarnessError> {
        // every expectation needs an entry of its step with its outcome and audit fields
        for want in expected {
            let met = found.iter().any(|applied| {
                applied.step == want.step
                    && applied.outcome == want.outcome
                    && applied.backup == want.backup
                    && applied.confirm.is_some() == want.confirmed
                    && applied.loss.is_some() == want.loss
            });
            if !met {
                return Err(HarnessError::Failed(format!(
                    "no history entry is {}:{:?} (backup {:?}, confirmed {}, loss {}); the history \
                     holds {:?}",
                    want.step,
                    want.outcome,
                    want.backup,
                    want.confirmed,
                    want.loss,
                    found
                        .iter()
                        .map(|applied| format!("{}:{:?}", applied.step, applied.outcome))
                        .collect::<Vec<_>>()
                )));
            }
        }
        Ok(format!(
            "{} expected entries are in the history",
            expected.len()
        ))
    }

    /// Run a check against the cluster
    ///
    /// # Arguments
    ///
    /// * `check` - The check
    /// * `scenario` - The scenario, for its fixtures
    async fn check(&self, check: &Check, scenario: &Scenario) -> Result<String, HarnessError> {
        // the checks the cluster can answer from its status, ConfigMap, and databases
        match check {
            Check::Fixtures(verifier) => {
                let fixture = scenario.fixture(&verifier.family).ok_or_else(|| {
                    HarnessError::Failed(format!(
                        "no fixture is registered for {}",
                        verifier.family
                    ))
                })?;
                let records = self.data.read(&verifier.family).await?;
                verify_layouts(&records, fixture, verifier)?;
                Ok(format!("{} objects of {}", records.len(), verifier.family))
            }
            Check::History(expected) => {
                Self::check_history(&self.history().await?.applied, expected)
            }
            Check::Condition { kind, present } => {
                let status = self.status().await?;
                let raised = status
                    .conditions
                    .iter()
                    .any(|condition| condition.kind == *kind && condition.status == "True");
                if raised == *present {
                    Ok(format!("condition {kind:?} present {raised}"))
                } else {
                    Err(HarnessError::Failed(format!(
                        "expected condition {kind:?} present {present}, but it is {raised}"
                    )))
                }
            }
            Check::Records(check) => {
                let records = self.data.read(&check.family).await?;
                (check.check)(&records).map_err(|why| {
                    HarnessError::Failed(format!(
                        "{} of {} doesn't hold: {why}",
                        check.name, check.family
                    ))
                })?;
                Ok(format!("{} of {}", check.name, check.family))
            }
            Check::Issues { .. } | Check::Drift(_) | Check::Passes(_) | Check::SameDataAs(_) => {
                Err(HarnessError::Unsupported(format!(
                    "the cluster driver doesn't answer {} yet; read it with thoradm",
                    super::render_check(check)
                )))
            }
        }
    }
}

#[async_trait]
impl ScenarioDriver for ClusterDriver {
    /// This is the cluster driver
    fn kind(&self) -> DriverKind {
        // always the cluster
        DriverKind::Cluster
    }

    /// Run one action against the cluster
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
            ScenarioAction::Seed(seed) => self.seed(seed, scenario).await?,
            ScenarioAction::Target(revision) => {
                self.patch(serde_json::json!({"target_revision": revision}))
                    .await?;
                format!("target_revision is {revision}")
            }
            ScenarioAction::ExpectRevision(revision) => {
                self.wait_for(&format!("revision {revision}"), |status| {
                    status.current.as_deref() == Some(revision.as_str())
                })
                .await?;
                format!("the cluster is at {revision}")
            }
            ScenarioAction::ExpectBlocked { step, reason, .. } => {
                self.wait_for(&format!("{step} to block"), |status| {
                    status.steps.iter().any(|planned| {
                        planned.step == *step
                            && planned.state == StepState::Blocked
                            && planned
                                .blocked
                                .as_ref()
                                .is_some_and(|blocked| reasons_match(reason, blocked))
                    })
                })
                .await?;
                format!("{step} is blocked")
            }
            ScenarioAction::ExpectRefused { step } => self.expect_refused(step).await?,
            ScenarioAction::ExpectConfirmRefused { step, refusal } => {
                self.expect_confirm_refused(step, *refusal).await?
            }
            ScenarioAction::Approve(approval) => {
                self.approve(approval.clone()).await?;
                format!("approved {}", approval.step)
            }
            ScenarioAction::ApproveLoss {
                step,
                backup,
                confirm,
            } => self.approve_loss(step, backup.as_ref(), confirm).await?,
            ScenarioAction::ApproveAll(backup) => self.approve_all(backup).await?,
            ScenarioAction::SetDevLossy(on) => {
                self.patch(serde_json::json!({"dangerous_downgrade_lossy": on}))
                    .await?;
                format!("dangerous_downgrade_lossy is {on}")
            }
            ScenarioAction::Step => {
                return Err(HarnessError::DirectOnly {
                    scenario: scenario.name().to_owned(),
                    action: action.render(),
                });
            }
            ScenarioAction::Check(check) => self.check(check, scenario).await?,
            ScenarioAction::Capture(_) | ScenarioAction::Inject(_) => {
                return Err(HarnessError::Unsupported(format!(
                    "the cluster driver doesn't run {} yet",
                    action.render()
                )));
            }
            ScenarioAction::Deploy(image) => {
                let command = self
                    .deploy_command
                    .iter()
                    .map(|part| part.replace("{image}", image))
                    .collect();
                // minithor exits non-zero when the deployed plan holds at an approval or waits for
                // a target, which a scenario goes on to assert, so only that refusal is let through
                match Self::exec(command).await {
                    Ok(_) => format!("deployed {image}"),
                    Err(HarnessError::Backend(why))
                        if why.contains("has blocked upgrade steps")
                            || why.contains("is in the UpgradeRequired phase") =>
                    {
                        format!("deployed {image}; the plan holds")
                    }
                    Err(err) => return Err(err),
                }
            }
            ScenarioAction::Restart(RestartTarget::Operator) => {
                let deployment = format!("deployment/{}", self.operator);
                self.kubectl(&["rollout", "restart", &deployment, "-n", &self.namespace])
                    .await?;
                "restarted the operator".to_owned()
            }
            ScenarioAction::Restart(RestartTarget::JobPod) => {
                self.kubectl(&["delete", "pod", "-l", &self.job_pods, "-n", &self.namespace])
                    .await?;
                "deleted the Job's pod".to_owned()
            }
        };
        Ok(ActionRecord {
            action: action.render(),
            outcome,
        })
    }
}
