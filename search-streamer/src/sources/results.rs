//! Add support for streaming results to a search store

use std::collections::{HashMap, HashSet};

use bytesize::ByteSize;
use chrono::prelude::*;
use futures::StreamExt;
use scylla::client::pager::TypedRowStream;
use scylla::client::session::Session;
use serde_json::{Value, json};
use thorium::client::ResultSearchEvents;
use thorium::models::schema::queries::search::{
    ResultEnumerateRow, SearchResultsAuthQueries, SearchResultsIdsQueries, SearchResultsQueries,
};
use thorium::models::{ElasticIndex, OutputKind, ResultSearchEvent};
use thorium::{Error, Thorium};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::DataSource;
use super::utils::proportional_truncate;
use crate::events::CompactResultEvent;
use crate::index::{IndexMapping, IndexTyped};
use crate::stores::{Elastic, StoreIdentifiable, StoreLookup};

/// The source of results to stream, with the statements it reads them with
#[derive(Clone)]
pub struct Results {
    /// The statements on the `results_ids` view
    ids: SearchResultsIdsQueries,
    /// The statements on the results table
    bodies: SearchResultsQueries,
    /// The statements on the `results_auth` view
    auth: SearchResultsAuthQueries,
}

#[async_trait::async_trait]
impl DataSource for Results {
    const DATA_NAME: &'static str = "Results";

    // pull and send fewer results concurrently because results can
    // be *very* large; helps avoid timeouts and OOM errors
    const INIT_CONCURRENT: usize = 5;

    type DataBundle = ResultBundle;

    type IndexType = OutputKind;

    type InitRow = ResultEnumerateRow;

    type InitInfo = ResultInfo;

    type Event = ResultSearchEvent;

    type CompactEvent = CompactResultEvent;

    type EventClient = ResultSearchEvents;

    /// Prepare the statements the results source reads with
    ///
    /// # Arguments
    ///
    /// * `scylla` - The Scylla session to prepare the statements on
    /// * `ns` - The keyspace the results tables and views are in
    async fn new(scylla: &Session, ns: &str) -> Result<Self, Error> {
        // prepare the generated statements this source reads with
        let ids = SearchResultsIdsQueries::prepare(scylla, ns)
            .await
            .map_err(|err| Error::new(format!("Failed to prepare results statements: {err}")))?;
        let bodies = SearchResultsQueries::prepare(scylla, ns)
            .await
            .map_err(|err| Error::new(format!("Failed to prepare results statements: {err}")))?;
        let auth = SearchResultsAuthQueries::prepare(scylla, ns)
            .await
            .map_err(|err| Error::new(format!("Failed to prepare results statements: {err}")))?;
        Ok(Self { ids, bodies, auth })
    }

    fn event_client(thorium: &Thorium) -> &Self::EventClient {
        &thorium.search.events.results
    }

    /// Page through the keys that have results in one token range, reading each key once
    ///
    /// # Arguments
    ///
    /// * `scylla` - The Scylla session to read with
    /// * `start` - The first token of the range
    /// * `end` - The last token of the range
    async fn enumerate(
        &self,
        scylla: &Session,
        start: i64,
        end: i64,
    ) -> Result<TypedRowStream<ResultEnumerateRow>, Error> {
        // page through the keys with results in this range
        self.ids
            .enumerate(scylla, start, end)
            .await
            .map_err(|err| Error::new(format!("Failed to enumerate data from Scylla: {err}")))
    }

    #[instrument(
        name = "DataSource<SampleResults>::to_value",
        skip(data, now),
        err(Debug)
    )]
    fn to_values(
        data: Vec<ResultBundle>,
        data_type: &OutputKind,
        now: DateTime<Utc>,
        max_document_size: ByteSize,
    ) -> Result<Vec<Value>, Error> {
        let item_label = match data_type {
            OutputKind::Files => "sha256",
            OutputKind::Repos => "url",
        };
        let values = data.into_iter().try_fold(
            Vec::new(),
            |mut values, mut bundle| -> Result<_, Error> {
                // truncate results if needed to the max data length with some buffer for the other fields
                // TODO: truncate other fields along with the results
                proportional_truncate(
                    &mut bundle.results,
                    (max_document_size - ByteSize::kib(8)).as_u64(),
                );
                // get a store id from the bundle
                values.push(json!({"index": {"_id": bundle.as_store_id().to_string() }}));
                // build the final document to add and add it to our vec of docs
                values.push(json!({
                    item_label: &bundle.item,
                    "streamed": &now,
                    "group": bundle.group,
                    "results": bundle.results,
                    "files": bundle.files,
                    "children": bundle.children
                }));
                Ok(values)
            },
        )?;
        Ok(values)
    }

    #[instrument(name = "DataSource<Results>::bundle_init", skip_all, err(Debug))]
    async fn bundle_init(
        &self,
        info: Vec<ResultInfo>,
        scylla: &Session,
    ) -> Result<Vec<(OutputKind, Vec<ResultBundle>)>, Error> {
        // only pull if we actually have info
        if info.is_empty() {
            Ok(Vec::new())
        } else {
            // get ids from the results data
            let keys = info.iter().map(|i| &i.key).collect::<Vec<_>>();
            // pull results info
            let results_data = self.pull_results_data(&keys, scylla).await?;
            // pull the actual results for files/repos
            let all_ids = results_data.all_ids();
            let results = self.pull_results(&all_ids, scylla).await?;
            // bundle the results together
            let file_bundles = bundle_results(results_data.files, &results);
            let repo_bundles = bundle_results(results_data.repos, &results);
            Ok(vec![
                (OutputKind::Files, file_bundles),
                (OutputKind::Repos, repo_bundles),
            ])
        }
    }

    #[instrument(name = "DataSource<Results>::bundle_event", skip_all, err(Debug))]
    async fn bundle_event(
        &self,
        compacted_event: CompactResultEvent,
        scylla: &Session,
    ) -> Result<Vec<ResultBundle>, Error> {
        // pull results data for the event
        let results_data = self
            .pull_results_data_event(compacted_event, scylla)
            .await?;
        // get ids from the info
        let ids = results_data
            .values()
            .flat_map(|i| i.keys())
            .collect::<Vec<_>>();
        // pull the actual results
        let results = self.pull_results(&ids, scylla).await?;
        Ok(bundle_results(results_data, &results))
    }
}

/// A map of result id's to (result, files, children)
type ResultsMap = HashMap<Uuid, (String, Vec<String>, HashMap<String, Uuid>)>;

/// A map of a key to a map of its results to their groups
type ResultsDataMap = HashMap<String, HashMap<Uuid, HashSet<String>>>;

/// A bundle of data to stream to elastic
#[derive(Debug)]
pub struct ResultBundle {
    /// The sample/repo these results are for
    pub item: String,
    /// The group the item is in
    pub group: String,
    /// The results
    pub results: Vec<String>,
    /// Any files tied to the results
    pub files: Vec<String>,
    /// Any children found by this tool
    pub children: Vec<String>,
}

/// Contains results info for both files and repos
#[derive(Default)]
struct ResultsData {
    /// Info for files
    pub files: ResultsDataMap,
    /// Info for repos
    pub repos: ResultsDataMap,
}

impl ResultsData {
    /// Get a list of all result ID's we'll need when we're pulling the results
    fn all_ids(&self) -> Vec<&Uuid> {
        self.files
            .values()
            .flat_map(|info| info.keys())
            .chain(self.repos.values().flat_map(|info| info.keys()))
            .collect()
    }
}

/// Bundle together results from the data map and results map
///
/// Truncates results in a single bundle if their combined size is greater than
/// the maximum document size (see [`thorium::conf::SearchStreamer::max_document_size`])
///
/// # Arguments
///
/// * `results_data` - The data on the results
/// * `results_map` - A map of the results themselves
#[instrument(name = "sources::results::bundle_results", skip_all)]
fn bundle_results(results_data: ResultsDataMap, results_map: &ResultsMap) -> Vec<ResultBundle> {
    let mut bundles = Vec::new();
    for (item, data) in results_data {
        let (results, files, children) = data.keys().fold(
            (Vec::new(), Vec::new(), Vec::new()),
            |(mut results, mut files, mut children), id| {
                if let Some((r, f, c)) = results_map.get(id) {
                    results.push(r.clone());
                    files.extend(f.iter().cloned());
                    children.extend(c.keys().cloned());
                } else {
                    // we're missing this result, so log an error but continue on
                    event!(Level::ERROR, "Missing result with id '{id}'");
                }
                (results, files, children)
            },
        );
        for (_, groups) in data {
            for group in groups {
                bundles.push(ResultBundle {
                    item: item.clone(),
                    group,
                    // TODO: don't copy results; use references instead
                    results: results.clone(),
                    files: files.clone(),
                    children: children.clone(),
                });
            }
        }
    }
    bundles
}

impl Results {
    /// Pull results for the given result ids
    ///
    /// # Argument
    ///
    /// * `ids` - The ids of results to pull
    /// * `scylla` - The scylla client
    #[instrument(name = "sources::results::Results::pull_results", skip_all, err(Debug))]
    async fn pull_results(&self, ids: &[&Uuid], scylla: &Session) -> Result<ResultsMap, Error> {
        let mut result_map = ResultsMap::with_capacity(ids.len());
        // chunk into groups of 100
        for ids_chunk in ids.chunks(100) {
            let ids_chunk = ids_chunk.iter().map(|id| **id).collect::<Vec<Uuid>>();
            let mut typed_stream = self
                .bodies
                .results_chunk(scylla, &ids_chunk)
                .await
                .map_err(|err| Error::new(format!("Error pulling results: {err}")))?;
            while let Some(row) = typed_stream.next().await {
                // check the row is valid
                let row = row.unwrap();
                result_map.insert(row.id, (row.result, row.files, row.children));
            }
        }
        Ok(result_map)
    }

    /// Pull result info for the given keys
    ///
    /// # Argument
    ///
    /// * `keys` - The keys of items to pull result info for
    /// * `scylla` - The scylla client
    #[instrument(
        name = "sources::results::Results::pull_results_data",
        skip_all,
        err(Debug)
    )]
    async fn pull_results_data(
        &self,
        keys: &[&String],
        scylla: &Session,
    ) -> Result<ResultsData, Error> {
        let mut results_data = ResultsData::default();
        // chunk into groups of 100
        for keys_chunk in keys.chunks(100) {
            // get info on results for the given keys
            let keys_chunk = keys_chunk
                .iter()
                .map(|key| (*key).clone())
                .collect::<Vec<String>>();
            let mut typed_stream = self
                .ids
                .init_data_chunk(scylla, &keys_chunk)
                .await
                .map_err(|err| Error::new(format!("Error pulling results info: {err}")))?;
            // organize the info by key
            while let Some(row) = typed_stream.next().await {
                // make sure the row is valid
                let row = row.unwrap();
                // get the right map based on the row's kind
                let map = match row.kind {
                    OutputKind::Files => &mut results_data.files,
                    OutputKind::Repos => &mut results_data.repos,
                };
                // save info from the row to the map
                let info = map.entry(row.key).or_default();
                info.entry(row.id).or_default().insert(row.group);
            }
        }
        Ok(results_data)
    }

    /// Pull result data for the given event
    ///
    /// # Argument
    ///
    /// * `event` - The event to pull data for
    /// * `scylla` - The scylla client
    #[instrument(
        name = "sources::results::Results::pull_results_data_event",
        skip_all,
        err(Debug)
    )]
    async fn pull_results_data_event(
        &self,
        compacted_event: CompactResultEvent,
        scylla: &Session,
    ) -> Result<ResultsDataMap, Error> {
        let mut results_info = ResultsDataMap::new();
        // get a contiguous Vec of groups from our compacted event
        let groups = compacted_event.groups.into_iter().collect::<Vec<_>>();
        // chunk groups by 100
        for groups_chunk in groups.chunks(100) {
            // get info on results for the given keys
            let mut typed_stream = self
                .auth
                .event_data_chunk(
                    scylla,
                    &compacted_event.item,
                    &compacted_event.result_kind,
                    groups_chunk,
                )
                .await
                .map_err(|err| Error::new(format!("Error pulling info on event: {err}")))?;
            // organize the info by key
            while let Some(row) = typed_stream.next().await {
                // make sure the row is valid
                let row = row.unwrap();
                // save info from the row to the map
                let info = results_info
                    .entry(compacted_event.item.clone())
                    .or_default();
                info.entry(row.id).or_default().insert(row.group);
            }
        }
        Ok(results_info)
    }
}

/// The info required to pull a result bundle for a single item
pub struct ResultInfo {
    pub key: String,
}

impl From<ResultEnumerateRow> for ResultInfo {
    fn from(row: ResultEnumerateRow) -> Self {
        Self { key: row.key }
    }
}

impl From<ResultSearchEvent> for ResultInfo {
    fn from(event: ResultSearchEvent) -> Self {
        Self { key: event.item }
    }
}

/// A unique id to a result document in the search store
pub struct ResultStoreId<'a> {
    /// The item the document refers to
    item: &'a String,
    /// The group the item is in
    group: &'a String,
}

impl std::fmt::Display for ResultStoreId<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // a plain `{item}-{group}` join is not injective — `("proj", "team-a")` and
        // `("proj-team", "a")` collide to the same _id, letting one group clobber another group's
        // document. Length-prefixing `item` makes the boundary unambiguous, so distinct (item, group)
        // pairs always map to distinct ids.
        write!(f, "{}-{}-{}", self.item.len(), self.item, self.group)
    }
}

impl<'a> StoreIdentifiable<'a> for ResultBundle {
    type Id = ResultStoreId<'a>;

    fn as_store_id(&'a self) -> Self::Id {
        ResultStoreId {
            item: &self.item,
            group: &self.group,
        }
    }
}

impl IndexMapping<Elastic> for OutputKind {
    fn all_indexes() -> Vec<ElasticIndex> {
        vec![ElasticIndex::SampleResults, ElasticIndex::RepoResults]
    }

    fn map_index(&self) -> Result<ElasticIndex, Error> {
        match self {
            OutputKind::Files => Ok(ElasticIndex::SampleResults),
            OutputKind::Repos => Ok(ElasticIndex::RepoResults),
        }
    }
}

impl IndexTyped for CompactResultEvent {
    type IndexType = OutputKind;

    fn index_type(&self) -> Self::IndexType {
        self.result_kind
    }
}

impl<'a> StoreLookup<'a> for CompactResultEvent {
    type Id = ResultStoreId<'a>;

    fn store_ids(&'a self) -> Vec<Self::Id> {
        // return all the combos of item+group for each group
        self.groups
            .iter()
            .map(|group| ResultStoreId {
                item: &self.item,
                group,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::ResultStoreId;

    #[test]
    fn result_store_id_is_injective() {
        // build the two colliding (item, group) pairs that a plain join would map together
        let victim_item = "github.com/org/proj".to_string();
        let victim_group = "team-a".to_string();
        let attacker_item = "github.com/org/proj-team".to_string();
        let attacker_group = "a".to_string();
        // build the search store ids for each pair
        let victim = ResultStoreId {
            item: &victim_item,
            group: &victim_group,
        };
        let attacker = ResultStoreId {
            item: &attacker_item,
            group: &attacker_group,
        };
        // distinct (item, group) pairs must map to distinct Elasticsearch _ids so one
        // group's repo document cannot overwrite or delete another group's document
        assert_ne!(
            victim.to_string(),
            attacker.to_string(),
            "distinct (item, group) pairs collided to the same _id"
        );
    }
}
