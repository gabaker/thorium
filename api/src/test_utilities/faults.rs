//! Storage failures that integration tests inject into the in-process test API
//!
//! The test API runs in the same process as the tests, so a test can ask a write path to fail
//! partway through and then check what was left behind. Failures are keyed by the name or id of
//! the entity being written, so tests running at the same time never trip each other's failures.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use uuid::Uuid;

/// The names of the tools whose next create fails after its tool instance is written
static FAIL_AFTER_INSTANCE_WRITE: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next create of a tool with this name fail after its first instance is written
///
/// # Arguments
///
/// * `tool` - The name of the tool whose create should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_after_instance_write(tool: &str) {
    // add this tool to the failures to inject
    FAIL_AFTER_INSTANCE_WRITE
        .lock()
        .expect("fault list poisoned")
        .insert(tool.to_owned());
}

/// Check whether a tool create should fail after its instance is written, clearing the failure
///
/// # Arguments
///
/// * `tool` - The name of the tool being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_instance_write_failure(tool: &str) -> bool {
    // remove this tool's failure so a retried create succeeds
    FAIL_AFTER_INSTANCE_WRITE
        .lock()
        .expect("fault list poisoned")
        .remove(tool)
}

/// The entities whose next update fails right after the entity is written
static FAIL_AFTER_ENTITY_WRITE: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next update of an entity fail right after the entity itself is written
///
/// # Arguments
///
/// * `id` - The id of the entity whose update should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_after_entity_write(id: Uuid) {
    // add this entity to the failures to inject
    FAIL_AFTER_ENTITY_WRITE
        .lock()
        .expect("fault list poisoned")
        .insert(id);
}

/// Check whether an entity update should fail after the entity is written, clearing the failure
///
/// # Arguments
///
/// * `id` - The id of the entity being updated
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_entity_write_failure(id: Uuid) -> bool {
    // remove this entity's failure so a retried update succeeds
    FAIL_AFTER_ENTITY_WRITE
        .lock()
        .expect("fault list poisoned")
        .remove(&id)
}

/// The ids of the entities whose next update fails before anything is written
static FAIL_BEFORE_ENTITY_UPDATE: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next update of an entity fail with a `500` before anything is written
///
/// # Arguments
///
/// * `id` - The id of the entity whose update should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_before_entity_update(id: Uuid) {
    // add this entity to the failures to inject
    FAIL_BEFORE_ENTITY_UPDATE
        .lock()
        .expect("fault list poisoned")
        .insert(id);
}

/// Check whether an entity update should fail before anything is written, clearing the failure
///
/// # Arguments
///
/// * `id` - The id of the entity being updated
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_entity_update_failure(id: Uuid) -> bool {
    // remove this entity's failure so a retried update succeeds
    FAIL_BEFORE_ENTITY_UPDATE
        .lock()
        .expect("fault list poisoned")
        .remove(&id)
}

/// The names of the entities whose next create fails right before the entity is written
static FAIL_BEFORE_ENTITY_INSERT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next create of an entity with this name fail right before the entity is written
///
/// # Arguments
///
/// * `name` - The name of the entity whose create should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_before_entity_insert(name: &str) {
    // add this name to the failures to inject
    FAIL_BEFORE_ENTITY_INSERT
        .lock()
        .expect("fault list poisoned")
        .insert(name.to_owned());
}

/// Check whether a create should fail right before its entity is written, clearing the failure
///
/// # Arguments
///
/// * `name` - The name of the entity being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_entity_insert_failure(name: &str) -> bool {
    // remove this name's failure so a retried create succeeds
    FAIL_BEFORE_ENTITY_INSERT
        .lock()
        .expect("fault list poisoned")
        .remove(name)
}

/// The names of the entities whose next create fails right after the entity is written
static FAIL_AFTER_ENTITY_INSERT: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next create of an entity with this name fail right after the entity is written
///
/// # Arguments
///
/// * `name` - The name of the entity whose create should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_after_entity_insert(name: &str) {
    // add this name to the failures to inject
    FAIL_AFTER_ENTITY_INSERT
        .lock()
        .expect("fault list poisoned")
        .insert(name.to_owned());
}

/// Check whether a create should fail right after its entity is written, clearing the failure
///
/// # Arguments
///
/// * `name` - The name of the entity being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_entity_inserted_failure(name: &str) -> bool {
    // remove this name's failure so a retried create succeeds
    FAIL_AFTER_ENTITY_INSERT
        .lock()
        .expect("fault list poisoned")
        .remove(name)
}

/// The entities whose next delete fails right after the entity is removed
static FAIL_AFTER_ENTITY_DELETE: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next delete of an entity fail right after the entity is removed, before anything
/// that refers to it is updated
///
/// # Arguments
///
/// * `id` - The id of the entity whose delete should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_after_entity_delete(id: Uuid) {
    // add this entity to the failures to inject
    FAIL_AFTER_ENTITY_DELETE
        .lock()
        .expect("fault list poisoned")
        .insert(id);
}

/// Check whether a delete should fail after its entity is removed, clearing the failure
///
/// # Arguments
///
/// * `id` - The id of the entity being deleted
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_entity_delete_failure(id: Uuid) -> bool {
    // remove this entity's failure so a retried delete goes through
    FAIL_AFTER_ENTITY_DELETE
        .lock()
        .expect("fault list poisoned")
        .remove(&id)
}

/// The sources whose next edge write skips the outgoing edge caps
static UNCAPPED_EDGE_WRITES: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Let the next edge write from a source skip its outgoing edge caps, as a source written
/// before the caps or before a limit was lowered did
///
/// # Arguments
///
/// * `source` - The id of the source whose next edge write is uncapped
///
/// # Panics
///
/// Panics if another thread panicked while holding the source list
pub fn write_edges_uncapped(source: Uuid) {
    // add this source to the uncapped writes
    UNCAPPED_EDGE_WRITES
        .lock()
        .expect("source list poisoned")
        .insert(source);
}

/// Check whether an edge write from a source skips its edge caps, clearing the exemption
///
/// # Arguments
///
/// * `source` - The id of the source being written
///
/// # Panics
///
/// Panics if another thread panicked while holding the source list
#[must_use]
pub fn take_uncapped_edge_write(source: Uuid) -> bool {
    // remove this source's exemption so its later writes are capped
    UNCAPPED_EDGE_WRITES
        .lock()
        .expect("source list poisoned")
        .remove(&source)
}

/// The ids chosen for the next create of an entity with a given name
static CREATE_IDS: LazyLock<Mutex<HashMap<String, Uuid>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Give the next create of an entity with this name a chosen id, so a test can order ids
///
/// The create must not upload an image, since its image path is built before the id is chosen.
///
/// # Arguments
///
/// * `name` - The name of the entity to be created
/// * `id` - The id to give it
///
/// # Panics
///
/// Panics if another thread panicked while holding the id list
pub fn create_with_id(name: &str, id: Uuid) {
    // remember the id for this name
    CREATE_IDS
        .lock()
        .expect("id list poisoned")
        .insert(name.to_owned(), id);
}

/// Take the id chosen for the next create of an entity with this name, if one was chosen
///
/// # Arguments
///
/// * `name` - The name of the entity being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the id list
#[must_use]
pub fn take_create_id(name: &str) -> Option<Uuid> {
    // remove the chosen id so a later create with the same name gets a random one
    CREATE_IDS.lock().expect("id list poisoned").remove(name)
}

/// The sources whose next edge write fails at an edge, by the edge's position in the write
static EDGE_WRITE_FAILURES: LazyLock<Mutex<HashMap<Uuid, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make the next edge write from a source fail at one edge, before that edge is written
///
/// # Arguments
///
/// * `source` - The id of the source whose edge write should fail
/// * `position` - The position of the failing edge in the write, from zero
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_edge_write_at(source: Uuid, position: usize) {
    // add this source to the failures to inject
    EDGE_WRITE_FAILURES
        .lock()
        .expect("fault list poisoned")
        .insert(source, position);
}

/// Check whether the edge at a position in a source's edge write should fail, clearing the
/// failure when it does
///
/// # Arguments
///
/// * `source` - The id of the source being written
/// * `position` - The position of the edge about to be written, from zero
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_edge_write_failure(source: Uuid, position: usize) -> bool {
    // fail only at the chosen edge, and only once
    let mut failures = EDGE_WRITE_FAILURES.lock().expect("fault list poisoned");
    if failures.get(&source) == Some(&position) {
        failures.remove(&source);
        return true;
    }
    false
}

/// The sources whose next edge write fails to queue its edges' changes
static EDGE_QUEUE_FAILURES: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next edge write from a source fail to queue its edges' changes
///
/// # Arguments
///
/// * `source` - The id of the source whose queueing should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_edge_queue(source: Uuid) {
    // add this source to the failures to inject
    EDGE_QUEUE_FAILURES
        .lock()
        .expect("fault list poisoned")
        .insert(source);
}

/// Check whether an edge write from a source should fail to queue its changes, clearing the
/// failure
///
/// # Arguments
///
/// * `source` - The id of the source being written
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_edge_queue_failure(source: Uuid) -> bool {
    // remove this source's failure so a retried write queues
    EDGE_QUEUE_FAILURES
        .lock()
        .expect("fault list poisoned")
        .remove(&source)
}

/// Where a paused write path for one user stands at one of its pause points
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UserPause {
    /// The write path will pause when it reaches the pause point for the user
    Armed,
    /// The write path reached the pause point for the user and is waiting
    Paused,
    /// The test let the write path go on
    Released,
}

/// The state of each user's write path at one pause point, keyed by username
type UserPauses = LazyLock<Mutex<HashMap<String, UserPause>>>;

/// The users whose next whitelist scan pauses before writing their settings
static SCAN_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// The users whose next whitelist scan pauses before writing their ban notifications
static NOTIFY_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Arm a pause for a user at one pause point
///
/// # Arguments
///
/// * `pauses` - The pause point to arm
/// * `username` - The user whose write path should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
fn arm(pauses: &UserPauses, username: &str) {
    // arm the pause for this user
    pauses
        .lock()
        .expect("pause list poisoned")
        .insert(username.to_owned(), UserPause::Armed);
}

/// Check whether a user's write path is waiting at one pause point
///
/// # Arguments
///
/// * `pauses` - The pause point to check
/// * `username` - The user to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
fn is_paused(pauses: &UserPauses, username: &str) -> bool {
    // the write path is paused once it reached this user
    pauses.lock().expect("pause list poisoned").get(username) == Some(&UserPause::Paused)
}

/// Let a user's write path go on from one pause point
///
/// # Arguments
///
/// * `pauses` - The pause point to release
/// * `username` - The user whose write path to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
fn release(pauses: &UserPauses, username: &str) {
    // mark this user's write path as released
    pauses
        .lock()
        .expect("pause list poisoned")
        .insert(username.to_owned(), UserPause::Released);
}

/// Wait at one pause point if a test armed a pause for this user, until the test releases it
///
/// # Arguments
///
/// * `pauses` - The pause point the write path reached
/// * `username` - The user being written
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
async fn wait_at(pauses: &UserPauses, username: &str) {
    // mark the write path paused if a test armed a pause for this user
    {
        let mut pauses = pauses.lock().expect("pause list poisoned");
        match pauses.get_mut(username) {
            Some(state) if *state == UserPause::Armed => *state = UserPause::Paused,
            _ => return,
        }
    }
    // wait until the test releases the write path, then forget the pause
    loop {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut pauses = pauses.lock().expect("pause list poisoned");
        if pauses.get(username) == Some(&UserPause::Released) {
            pauses.remove(username);
            return;
        }
    }
}

/// Make the next whitelist scan pause after it reads a user and before it writes their settings
///
/// # Arguments
///
/// * `username` - The user whose scan should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_scan_before_write(username: &str) {
    // arm the pause before the settings write for this user
    arm(&SCAN_PAUSES, username);
}

/// Check whether a whitelist scan is paused before writing a user's settings
///
/// # Arguments
///
/// * `username` - The user to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn scan_paused(username: &str) -> bool {
    // check the pause before the settings write for this user
    is_paused(&SCAN_PAUSES, username)
}

/// Let a paused whitelist scan write a user's settings
///
/// # Arguments
///
/// * `username` - The user whose scan to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_scan(username: &str) {
    // release the pause before the settings write for this user
    release(&SCAN_PAUSES, username);
}

/// Pause a whitelist scan before it writes a user's settings if a test asked for it
///
/// # Arguments
///
/// * `username` - The user being scanned
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_scan_paused(username: &str) {
    // wait at the pause before the settings write if one is armed
    wait_at(&SCAN_PAUSES, username).await;
}

/// Make the next whitelist scan pause after it wrote and re-read a user and before it writes
/// their ban notifications
///
/// # Arguments
///
/// * `username` - The user whose scan should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_scan_before_notify(username: &str) {
    // arm the pause before the notification write for this user
    arm(&NOTIFY_PAUSES, username);
}

/// Check whether a whitelist scan is paused before writing a user's ban notifications
///
/// # Arguments
///
/// * `username` - The user to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn scan_notify_paused(username: &str) -> bool {
    // check the pause before the notification write for this user
    is_paused(&NOTIFY_PAUSES, username)
}

/// Let a paused whitelist scan write a user's ban notifications
///
/// # Arguments
///
/// * `username` - The user whose scan to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_scan_notify(username: &str) {
    // release the pause before the notification write for this user
    release(&NOTIFY_PAUSES, username);
}

/// Pause a whitelist scan before it writes a user's ban notifications if a test asked for it
///
/// # Arguments
///
/// * `username` - The user being scanned
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_scan_notify_paused(username: &str) {
    // wait at the pause before the notification write if one is armed
    wait_at(&NOTIFY_PAUSES, username).await;
}

/// The usernames whose next create pauses after its existing-user check and before the user is
/// written
static CREATE_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make the next create of a username pause after its existing-user check and before the user
/// is written
///
/// Only the first create to reach the pause waits; later creates of the same username go on.
///
/// # Arguments
///
/// * `username` - The username whose create should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_user_create(username: &str) {
    // arm the pause before the user write for this username
    arm(&CREATE_PAUSES, username);
}

/// Check whether a create of a username is paused before the user is written
///
/// # Arguments
///
/// * `username` - The username to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn user_create_paused(username: &str) -> bool {
    // check the pause before the user write for this username
    is_paused(&CREATE_PAUSES, username)
}

/// Let a paused create of a username write the user
///
/// # Arguments
///
/// * `username` - The username whose create to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_user_create(username: &str) {
    // release the pause before the user write for this username
    release(&CREATE_PAUSES, username);
}

/// Pause a create of a username before the user is written if a test asked for it
///
/// # Arguments
///
/// * `username` - The username being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_user_create_paused(username: &str) {
    // wait at the pause before the user write if one is armed
    wait_at(&CREATE_PAUSES, username).await;
}

/// The usernames whose next delete pauses after the user is removed
static DELETE_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make the next delete of a username pause after the user is removed
///
/// # Arguments
///
/// * `username` - The username whose delete should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_user_delete(username: &str) {
    // arm the pause after the user removal for this username
    arm(&DELETE_PAUSES, username);
}

/// Check whether a delete of a username is paused after the user is removed
///
/// # Arguments
///
/// * `username` - The username to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn user_delete_paused(username: &str) -> bool {
    // check the pause after the user removal for this username
    is_paused(&DELETE_PAUSES, username)
}

/// Let a paused delete of a username finish
///
/// # Arguments
///
/// * `username` - The username whose delete to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_user_delete(username: &str) {
    // release the pause after the user removal for this username
    release(&DELETE_PAUSES, username);
}

/// Pause a delete of a username after the user is removed if a test asked for it
///
/// # Arguments
///
/// * `username` - The username being deleted
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_user_delete_paused(username: &str) {
    // wait at the pause after the user removal if one is armed
    wait_at(&DELETE_PAUSES, username).await;
}

/// The entities whose next update pauses right before the entity is written, keyed by id
static ENTITY_UPDATE_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make the next update of an entity pause right before the entity is written
///
/// Only the first update to reach the pause waits; later updates of the same entity go on.
///
/// # Arguments
///
/// * `id` - The id of the entity whose update should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_entity_update(id: Uuid) {
    // arm the pause before the entity write for this entity
    arm(&ENTITY_UPDATE_PAUSES, &id.to_string());
}

/// Check whether an update of an entity is paused before the entity is written
///
/// # Arguments
///
/// * `id` - The id of the entity to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn entity_update_paused(id: Uuid) -> bool {
    // check the pause before the entity write for this entity
    is_paused(&ENTITY_UPDATE_PAUSES, &id.to_string())
}

/// Let a paused update of an entity write the entity
///
/// # Arguments
///
/// * `id` - The id of the entity whose update to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_entity_update(id: Uuid) {
    // release the pause before the entity write for this entity
    release(&ENTITY_UPDATE_PAUSES, &id.to_string());
}

/// Pause an update of an entity before the entity is written if a test asked for it
///
/// # Arguments
///
/// * `id` - The id of the entity being updated
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_entity_update_paused(id: Uuid) {
    // wait at the pause before the entity write if one is armed
    wait_at(&ENTITY_UPDATE_PAUSES, &id.to_string()).await;
}

/// The roots whose next lock release fails after the lock is released
static FAIL_AFTER_UNLOCK: LazyLock<Mutex<HashSet<Uuid>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Make the next release of a root's instance lock fail with a `500` after the lock is released,
/// as a lost reply from Redis would
///
/// # Arguments
///
/// * `root` - The id of the root whose lock release should fail
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
pub fn fail_after_unlock(root: Uuid) {
    // add this root to the failures to inject
    FAIL_AFTER_UNLOCK
        .lock()
        .expect("fault list poisoned")
        .insert(root);
}

/// Check whether a root's lock release should fail after the lock is released, clearing the
/// failure
///
/// # Arguments
///
/// * `root` - The id of the root whose lock was released
///
/// # Panics
///
/// Panics if another thread panicked while holding the failure list
#[must_use]
pub fn take_unlock_failure(root: Uuid) -> bool {
    // remove this root's failure so the next release succeeds
    FAIL_AFTER_UNLOCK
        .lock()
        .expect("fault list poisoned")
        .remove(&root)
}

/// The groups where tests may still create and update images through the image routes
static IMAGE_WRITE_GROUPS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

/// Let tests create and update images in a group through the image routes
///
/// Image writes are refused everywhere else, so only tests of the stored images opt in.
///
/// # Arguments
///
/// * `group` - The group to allow image writes in
///
/// # Panics
///
/// Panics if another thread panicked while holding the group list
pub fn allow_image_writes(group: &str) {
    // add this group to the groups that allow image writes
    IMAGE_WRITE_GROUPS
        .lock()
        .expect("group list poisoned")
        .insert(group.to_owned());
}

/// Refuse image writes in a group again, as everywhere else
///
/// # Arguments
///
/// * `group` - The group to refuse image writes in
///
/// # Panics
///
/// Panics if another thread panicked while holding the group list
pub fn forbid_image_writes(group: &str) {
    // remove this group from the groups that allow image writes
    IMAGE_WRITE_GROUPS
        .lock()
        .expect("group list poisoned")
        .remove(group);
}

/// Check whether a test allowed image writes in a group
///
/// # Arguments
///
/// * `group` - The group an image is written in
///
/// # Panics
///
/// Panics if another thread panicked while holding the group list
#[must_use]
pub fn image_writes_allowed(group: &str) -> bool {
    // check the groups that allow image writes
    IMAGE_WRITE_GROUPS
        .lock()
        .expect("group list poisoned")
        .contains(group)
}

/// The run a reaction created on a pipeline should belong to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionRun {
    /// The reaction is its own run, like a workflow run or direct tool run
    Own,
    /// The reaction belongs to another reaction's run, like a harness's tool call
    Of(Uuid),
}

/// The run each reaction created on a pipeline belongs to, keyed by pipeline name
static PIPELINE_RUNS: LazyLock<Mutex<HashMap<String, ReactionRun>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make every later reaction created on a pipeline belong to a run
///
/// # Arguments
///
/// * `pipeline` - The name of the pipeline
/// * `run` - The run the pipeline's reactions belong to
///
/// # Panics
///
/// Panics if another thread panicked while holding the run list
pub fn run_reactions(pipeline: &str, run: ReactionRun) {
    // remember the run for this pipeline's reactions
    PIPELINE_RUNS
        .lock()
        .expect("run list poisoned")
        .insert(pipeline.to_owned(), run);
}

/// Get the run a reaction created on a pipeline belongs to, if a test set one
///
/// # Arguments
///
/// * `pipeline` - The name of the pipeline
/// * `id` - The id of the reaction being created
///
/// # Panics
///
/// Panics if another thread panicked while holding the run list
#[must_use]
pub fn reaction_run(pipeline: &str, id: Uuid) -> Option<Uuid> {
    // resolve this pipeline's run for this reaction
    let runs = PIPELINE_RUNS.lock().expect("run list poisoned");
    match runs.get(pipeline) {
        Some(ReactionRun::Own) => Some(id),
        Some(ReactionRun::Of(run)) => Some(*run),
        None => None,
    }
}

/// The number of times each entity was read from Scylla
static ENTITY_READS: LazyLock<Mutex<HashMap<Uuid, u64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Count one read of an entity from Scylla
///
/// # Arguments
///
/// * `id` - The id of the entity that was read
///
/// # Panics
///
/// Panics if another thread panicked while holding the read counts
pub fn count_entity_read(id: Uuid) {
    // add one to this entity's read count
    *ENTITY_READS
        .lock()
        .expect("read counts poisoned")
        .entry(id)
        .or_default() += 1;
}

/// Get the number of times an entity was read from Scylla
///
/// # Arguments
///
/// * `id` - The id of the entity
///
/// # Panics
///
/// Panics if another thread panicked while holding the read counts
#[must_use]
pub fn entity_reads(id: Uuid) -> u64 {
    // get this entity's read count
    ENTITY_READS
        .lock()
        .expect("read counts poisoned")
        .get(&id)
        .copied()
        .unwrap_or_default()
}

/// The groups whose next workload prune pauses between counting the jobs of idle tool instances
/// and removing them
static PRUNE_PAUSES: UserPauses = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Make the next workload prune of a group pause after it counts the jobs of idle tool instances
/// and before it removes them
///
/// # Arguments
///
/// * `group` - The group whose prune should pause
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn pause_workload_prune(group: &str) {
    // arm the pause before the removal for this group
    arm(&PRUNE_PAUSES, group);
}

/// Check whether a workload prune of a group is paused before its removal
///
/// # Arguments
///
/// * `group` - The group to check
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
#[must_use]
pub fn workload_prune_paused(group: &str) -> bool {
    // check the pause before the removal for this group
    is_paused(&PRUNE_PAUSES, group)
}

/// Let a paused workload prune of a group remove its idle tool instances
///
/// # Arguments
///
/// * `group` - The group whose prune to release
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub fn release_workload_prune(group: &str) {
    // release the pause before the removal for this group
    release(&PRUNE_PAUSES, group);
}

/// Pause a workload prune of a group before its removal if a test asked for it
///
/// # Arguments
///
/// * `group` - The group being pruned
///
/// # Panics
///
/// Panics if another thread panicked while holding the pause list
pub async fn wait_if_workload_prune_paused(group: &str) {
    // wait at the pause before the removal if one is armed
    wait_at(&PRUNE_PAUSES, group).await;
}
