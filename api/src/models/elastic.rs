//! Structures related to searching in Elastic Search

use chrono::prelude::*;
use uuid::Uuid;

#[cfg(feature = "api")]
use strum::{EnumIter, IntoEnumIterator};

/// The different elastic indexes
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(EnumIter))]
pub enum ElasticIndex {
    // Result-related indexes
    /// The results for jobs on samples
    SampleResults,
    /// The results for jobs on repos
    RepoResults,

    // Tag-related indexes
    /// The tags on samples
    SampleTags,
    /// The tags on repos
    RepoTags,
}

impl std::fmt::Display for ElasticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ElasticIndex::SampleResults => write!(f, "SampleResults"),
            ElasticIndex::RepoResults => write!(f, "RepoResults"),
            ElasticIndex::SampleTags => write!(f, "SampleTags"),
            ElasticIndex::RepoTags => write!(f, "RepoTags"),
        }
    }
}

impl ElasticIndex {
    /// Every index, in the order the schema registry declares them
    pub const ALL: [ElasticIndex; 4] = [
        ElasticIndex::SampleResults,
        ElasticIndex::RepoResults,
        ElasticIndex::SampleTags,
        ElasticIndex::RepoTags,
    ];

    /// The search-streamer source this index belongs to
    #[must_use]
    pub const fn source(&self) -> ElasticSource {
        match self {
            ElasticIndex::SampleResults | ElasticIndex::RepoResults => ElasticSource::Results,
            ElasticIndex::SampleTags | ElasticIndex::RepoTags => ElasticSource::Tags,
        }
    }

    /// Get the full index name for a specific index
    ///
    /// # Arguments
    ///
    /// * `elastic_conf` - The elastic config
    #[must_use]
    pub fn full_name<'a>(&self, elastic_conf: &'a crate::conf::Elastic) -> &'a str {
        match self {
            ElasticIndex::SampleResults => &elastic_conf.results.samples,
            ElasticIndex::RepoResults => &elastic_conf.results.repos,
            ElasticIndex::SampleTags => &elastic_conf.tags.samples,
            ElasticIndex::RepoTags => &elastic_conf.tags.repos,
        }
    }

    /// Build the body that creates this index in Elastic with its mappings and settings
    ///
    /// # Arguments
    ///
    /// * `elastic_conf` - The elastic config
    #[must_use]
    pub fn create_body(&self, elastic_conf: &crate::conf::Elastic) -> serde_json::Value {
        // add our highlighting limits to this index's mappings
        serde_json::json!({
            "mappings": self.mappings(),
            "settings": {
                "index": {
                    "highlight": {
                        "max_analyzed_offset": elastic_conf.max_analyzed_offset
                    }
                }
            }
        })
    }

    /// Build this index's mappings, which depend on no configuration
    #[must_use]
    pub fn mappings(&self) -> serde_json::Value {
        // get the field that identifies the item each document describes
        let id_field = match self {
            ElasticIndex::SampleResults | ElasticIndex::SampleTags => "sha256",
            ElasticIndex::RepoResults | ElasticIndex::RepoTags => "url",
        };
        // start with the fields every index shares
        let mut properties = serde_json::json!({
            "group": { "type": "keyword" },
            "streamed": { "type": "date" },
        });
        // add the field identifying the item each document describes
        properties[id_field] = serde_json::json!({ "type": "keyword" });
        // get the text fields specific to this kind of index
        let text_fields: &[&str] = match self {
            ElasticIndex::SampleResults | ElasticIndex::RepoResults => {
                &["results", "files", "children"]
            }
            ElasticIndex::SampleTags | ElasticIndex::RepoTags => &["tags"],
        };
        // add each text field to our mappings
        for field in text_fields {
            properties[*field] = serde_json::json!({ "type": "text" });
        }
        // wrap our fields as the index's properties
        serde_json::json!({ "properties": properties })
    }

    /// Get the earliest date for documents in the given index
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[cfg(feature = "api")]
    pub fn earliest(&self, shared: &crate::utils::Shared) -> i64 {
        match self {
            ElasticIndex::SampleResults | ElasticIndex::RepoResults => {
                shared.config.thorium.results.earliest
            }
            ElasticIndex::SampleTags => shared.config.thorium.tags.files.earliest,
            ElasticIndex::RepoTags => shared.config.thorium.tags.repos.earliest,
        }
    }

    /// Get the earliest date for documents in all possible indexes
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    ///
    /// # Panics
    ///
    /// Panics if `ElasticIndex` has no variants (there are no possible indexes)
    #[cfg(feature = "api")]
    pub fn earliest_all(shared: &crate::utils::Shared) -> i64 {
        Self::iter()
            .map(|index| index.earliest(shared))
            .min()
            // safe to unwrap because we'll always have at least one index
            .expect("No elastic indexes")
    }
}

/// Returns all elastic indexes as a default
fn default_search_indexes() -> Vec<ElasticIndex> {
    // only search on samples, as the Web UI is the only user of
    // search and only samples are currently supported there
    vec![ElasticIndex::SampleResults, ElasticIndex::SampleTags]
}

/// Default the Result list limit to 50
fn default_search_limit() -> u32 {
    50
}

/// The query params for searching results
#[derive(Serialize, Deserialize, Debug, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
#[cfg_attr(feature = "api", derive(utoipa::IntoParams))]
#[cfg_attr(feature = "api", into_params(parameter_in = Query))]
pub struct ElasticSearchParams {
    /// The indexes to search
    #[serde(default = "default_search_indexes")]
    pub indexes: Vec<ElasticIndex>,
    /// The query to use when searching
    #[serde(default)]
    pub query: String,
    /// The groups to search data from
    #[serde(default)]
    pub groups: Vec<String>,
    /// When to start searching data at
    #[serde(default = "Utc::now")]
    pub start: DateTime<Utc>,
    /// When to stop searching data at
    pub end: Option<DateTime<Utc>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of items to return in this response
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

/// The query params for searching results
#[derive(Serialize, Deserialize, Debug)]
pub struct ElasticSearchOpts {
    /// The indexes to search
    pub indexes: Vec<ElasticIndex>,
    /// The query to use when searching
    pub query: String,
    /// The groups to search data from
    pub groups: Vec<String>,
    /// When to start searching data at
    pub start: Option<DateTime<Utc>>,
    /// When to stop searching data at
    pub end: Option<DateTime<Utc>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of objects to retrieve on a single page
    pub page_size: usize,
    /// The max number of items to return with this cursor
    pub limit: Option<usize>,
}

impl ElasticSearchOpts {
    /// Create a new search query
    ///
    /// # Arguments
    ///
    /// * `query` - The search query to use
    pub fn new<T: Into<String>>(query: T) -> Self {
        // build a base search options struct
        ElasticSearchOpts {
            indexes: default_search_indexes(),
            query: query.into(),
            groups: Vec::default(),
            start: None,
            end: None,
            cursor: None,
            page_size: 50,
            limit: None,
        }
    }

    /// Set the indexes to search on
    ///
    /// # Arguments
    ///
    /// * `indexes` - The indexes to search on
    #[must_use]
    pub fn indexes(mut self, indexes: Vec<ElasticIndex>) -> Self {
        self.indexes = indexes;
        self
    }
}

// A specific document in elastic
#[derive(Deserialize, Serialize, Default, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct ElasticDoc {
    /// The id for this document
    #[serde(alias = "_id")]
    pub id: String,
    /// The index this doc cames from
    #[serde(alias = "_index")]
    pub index: String,
    /// The score for this doc
    #[serde(alias = "_score")]
    pub score: Option<f64>,
    /// The actual document in elastic
    #[serde(alias = "_source")]
    pub source: Option<serde_json::Value>,
    /// The actual document in elastic
    pub highlight: Option<serde_json::Value>,
    /// The sort values for this document
    pub sort: Vec<i64>,
}

/// One of the search-streamer's independent sources; deleting any index of a source makes the
/// streamer resync both of its indexes
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ElasticSource {
    /// Results: the `SampleResults` and `RepoResults` indexes
    Results,
    /// Tags: the `SampleTags` and `RepoTags` indexes
    Tags,
}

#[cfg(test)]
mod tests {
    use super::ElasticIndex;
    use serde_json::json;

    /// Build an Elastic config with a non-default max analyzed offset
    fn elastic_conf() -> crate::conf::Elastic {
        // build a minimal config and let serde default everything else
        serde_json::from_value(json!({
            "node": "https://elastic:9200",
            "username": "thorium",
            "password": "password",
            "max_analyzed_offset": 1234,
        }))
        .expect("minimal elastic config parses")
    }

    /// Build the expected body for creating a results index
    ///
    /// # Arguments
    ///
    /// * `id_field` - The keyword field identifying the item each document describes
    fn results_body(id_field: &str) -> serde_json::Value {
        // build our expected mappings
        let mut properties = json!({
            "group": { "type": "keyword" },
            "streamed": { "type": "date" },
            "results": { "type": "text" },
            "files": { "type": "text" },
            "children": { "type": "text" },
        });
        properties[id_field] = json!({ "type": "keyword" });
        // wrap the mappings with our highlighting settings
        json!({
            "mappings": { "properties": properties },
            "settings": { "index": { "highlight": { "max_analyzed_offset": 1234 } } }
        })
    }

    /// Build the expected body for creating a tags index
    ///
    /// # Arguments
    ///
    /// * `id_field` - The keyword field identifying the item each document describes
    fn tags_body(id_field: &str) -> serde_json::Value {
        // build our expected mappings
        let mut properties = json!({
            "group": { "type": "keyword" },
            "streamed": { "type": "date" },
            "tags": { "type": "text" },
        });
        properties[id_field] = json!({ "type": "keyword" });
        // wrap the mappings with our highlighting settings
        json!({
            "mappings": { "properties": properties },
            "settings": { "index": { "highlight": { "max_analyzed_offset": 1234 } } }
        })
    }

    /// The sample results index keys on sha256 and maps results, files, and children as text
    #[test]
    fn sample_results_body() {
        // build the body and compare it to the full expected body
        let body = ElasticIndex::SampleResults.create_body(&elastic_conf());
        assert_eq!(body, results_body("sha256"));
    }

    /// The repo results index keys on url and maps results, files, and children as text
    #[test]
    fn repo_results_body() {
        // build the body and compare it to the full expected body
        let body = ElasticIndex::RepoResults.create_body(&elastic_conf());
        assert_eq!(body, results_body("url"));
    }

    /// The sample tags index keys on sha256 and maps only tags as text
    #[test]
    fn sample_tags_body() {
        // build the body and compare it to the full expected body
        let body = ElasticIndex::SampleTags.create_body(&elastic_conf());
        assert_eq!(body, tags_body("sha256"));
    }

    /// The repo tags index keys on url and maps only tags as text
    #[test]
    fn repo_tags_body() {
        // build the body and compare it to the full expected body
        let body = ElasticIndex::RepoTags.create_body(&elastic_conf());
        assert_eq!(body, tags_body("url"));
    }
}
