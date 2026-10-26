//! A fake resource and an in-memory store for driving the edit flow without a server

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use thorium::Error;

use crate::args::helpers::EditArgs;
use crate::handlers::edit::editable::{EditOutcome, Editable};
use crate::handlers::edit::flow::{Mode, run};
use crate::handlers::edit::seams::Seams;
use crate::utils::patch::{parse_set, parse_unset};

/// The fake resource as the store holds it
#[derive(Debug, Clone, PartialEq)]
pub struct FakeData {
    /// The resource's id
    pub id: String,
    /// The resource's name
    pub name: String,
    /// The resource's description
    pub description: Option<String>,
    /// The groups the resource is in
    pub groups: Vec<String>,
    /// The most turns the resource may take
    pub max_turns: u32,
}

/// The settings block of the fake view
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeConfig {
    /// The most turns the resource may take
    pub max_turns: u32,
}

/// The document users edit for the fake resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FakeView {
    /// The read-only id
    #[serde(rename = "*id*")]
    pub id: String,
    /// The name
    pub name: String,
    /// The description
    #[serde(default)]
    pub description: Option<String>,
    /// The groups
    pub groups: Vec<String>,
    /// The settings
    pub config: FakeConfig,
}

/// The request the fake resource sends: the whole new state
#[derive(Debug, Clone, PartialEq)]
pub struct FakeUpdate {
    /// The new name
    pub name: String,
    /// The new description
    pub description: Option<String>,
    /// The new groups
    pub groups: Vec<String>,
    /// The new turn limit
    pub max_turns: u32,
}

/// What the store records and how it misbehaves on request
pub struct Inner {
    /// The stored resource
    pub data: FakeData,
    /// The requests made, in order (`fetch`, `update`)
    pub log: Vec<String>,
    /// How many updates fail before one succeeds
    pub fail_updates: usize,
    /// A change applied to the stored resource when it is fetched the nth time
    pub change_on_fetch: Option<(usize, FakeData)>,
}

/// The in-memory store the fake resource talks to
#[derive(Clone)]
pub struct FakeStore(pub Arc<Mutex<Inner>>);

impl FakeStore {
    /// Create a store holding a resource named `agent`
    pub fn new() -> Self {
        // start with a resource that has every field filled in
        FakeStore(Arc::new(Mutex::new(Inner {
            data: FakeData {
                id: "11111111-2222-3333-4444-555555555555".to_string(),
                name: "agent".to_string(),
                description: Some("old".to_string()),
                groups: vec!["g1".to_string()],
                max_turns: 3,
            },
            log: Vec::new(),
            fail_updates: 0,
            change_on_fetch: None,
        })))
    }

    /// The stored resource
    pub fn data(&self) -> FakeData {
        // copy the stored resource out of the lock
        self.0.lock().unwrap().data.clone()
    }

    /// How many updates were sent
    pub fn updates(&self) -> usize {
        // count the update requests in the log
        self.0
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|r| *r == "update")
            .count()
    }

    /// How many times the resource was fetched
    pub fn fetches(&self) -> usize {
        // count the fetch requests in the log
        self.0
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|r| *r == "fetch")
            .count()
    }
}

/// The fake resource's [`Editable`] implementation
pub struct Fake;

// the fake store answers immediately, so its async methods never await
#[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
impl Editable for Fake {
    /// The fake is edited through an in-memory store
    type Client = FakeStore;
    /// The store holds one resource, so nothing picks it
    type Target = ();
    /// The stored resource
    type Data = FakeData;
    /// The fake document users edit
    type View = FakeView;
    /// The whole new state of the resource
    type Update = FakeUpdate;
    /// The fake is called `Fake` in messages
    const KIND: &'static str = "Fake";
    /// The fake document's keys
    const FIELD_ORDER: &'static [&'static str] = &["id", "name", "description", "groups", "config"];

    /// Read the stored resource, applying a scheduled remote change first
    ///
    /// # Arguments
    ///
    /// * `client` - The in-memory store
    /// * `()` - The unit target, since the store holds one resource
    async fn fetch(client: &FakeStore, (): &()) -> Result<FakeData, Error> {
        // record the request and count the fetches so far
        let mut inner = client.0.lock().unwrap();
        inner.log.push("fetch".to_string());
        let fetches = inner.log.iter().filter(|r| *r == "fetch").count();
        // apply a scheduled remote change on its fetch
        if let Some((nth, replacement)) = inner.change_on_fetch.clone()
            && nth == fetches
        {
            inner.data = replacement;
        }
        // hand back the (possibly changed) resource
        Ok(inner.data.clone())
    }

    /// Build the view from the stored resource
    ///
    /// # Arguments
    ///
    /// * `data` - The stored resource
    fn to_view(data: &FakeData) -> FakeView {
        // mirror every field except the id, which the view marks read-only
        FakeView {
            id: data.id.clone(),
            name: data.name.clone(),
            description: data.description.clone(),
            groups: data.groups.clone(),
            config: FakeConfig {
                max_turns: data.max_turns,
            },
        }
    }

    /// The stored resource's id
    ///
    /// # Arguments
    ///
    /// * `data` - The stored resource
    fn id(data: &FakeData) -> String {
        // the id is stored as text
        data.id.clone()
    }

    /// The stored resource's name
    ///
    /// # Arguments
    ///
    /// * `data` - The stored resource
    fn name(data: &FakeData) -> String {
        // the name is stored as text
        data.name.clone()
    }

    /// Require a group and a sane turn limit
    ///
    /// # Arguments
    ///
    /// * `view` - The edited view
    /// * `_data` - The stored resource, unused here
    fn validate(view: &FakeView, _data: &FakeData) -> Result<(), Error> {
        // a resource needs at least one group
        if view.groups.is_empty() {
            return Err(Error::new("groups must not be empty"));
        }
        // cap the turn limit
        if view.config.max_turns > 100 {
            return Err(Error::new("config.max_turns must be at most 100"));
        }
        Ok(())
    }

    /// Compare the view with the stored resource
    ///
    /// # Arguments
    ///
    /// * `_client` - The in-memory store, unused here
    /// * `data` - The stored resource
    /// * `view` - The edited view
    async fn calculate_update(
        _client: &FakeStore,
        data: &FakeData,
        view: FakeView,
    ) -> Result<Option<FakeUpdate>, Error> {
        // take the whole new state from the view
        let update = FakeUpdate {
            name: view.name,
            description: view.description,
            groups: view.groups,
            max_turns: view.config.max_turns,
        };
        // an update equal to the stored state is no change
        let same = update.name == data.name
            && update.description == data.description
            && update.groups == data.groups
            && update.max_turns == data.max_turns;
        // only send an update when something differs
        Ok((!same).then_some(update))
    }

    /// Store the update, failing first when the store is told to
    ///
    /// # Arguments
    ///
    /// * `client` - The in-memory store
    /// * `()` - The unit target, since the store holds one resource
    /// * `_data` - The stored resource, unused here
    /// * `update` - The new state to store
    async fn send_update(
        client: &FakeStore,
        (): &(),
        _data: &FakeData,
        update: &FakeUpdate,
    ) -> Result<(), Error> {
        // record the request
        let mut inner = client.0.lock().unwrap();
        inner.log.push("update".to_string());
        // fail while the store is told to
        if inner.fail_updates > 0 {
            inner.fail_updates -= 1;
            return Err(Error::new("the server refused the update"));
        }
        // store the new state
        inner.data.name = update.name.clone();
        inner.data.description = update.description.clone();
        inner.data.groups = update.groups.clone();
        inner.data.max_turns = update.max_turns;
        Ok(())
    }
}

/// Edit flags with the given `--set` and `--unset` arguments in order
///
/// Each argument is `set:PATH=VALUE` or `unset:PATH`.
///
/// # Arguments
///
/// * `ops` - The operations in command-line order
pub fn args_with(ops: &[&str]) -> EditArgs {
    // parse each operation the way clap would
    let mut args = EditArgs::default();
    for op in ops {
        args.ops.push(match op.split_once(':') {
            Some(("set", rest)) => parse_set(rest).unwrap(),
            Some(("unset", rest)) => parse_unset(rest).unwrap(),
            _ => panic!("bad operation {op}"),
        });
    }
    args
}

/// Run `update` against the fake resource
///
/// # Arguments
///
/// * `store` - The store holding the resource
/// * `args` - The edit flags
pub async fn update(store: &FakeStore, args: &EditArgs) -> Result<EditOutcome, Error> {
    // run the update flow with no terminal and no scripted answers
    run::<Fake>(store, "", &(), args, Mode::Update, &Seams::default()).await
}
