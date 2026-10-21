//! Thorium's fixed Elastic indexes as the schema registry declares them

use crate::models::elastic::ElasticSource;

/// One of Thorium's fixed Elastic indexes as the schema registry declares it; the search-streamer
/// still creates the index with this body
#[derive(Debug, Clone, PartialEq)]
pub struct ElasticIndexDef {
    /// The index's stable name, the `ElasticIndex` variant (`SampleResults`, …), not the
    /// deployment's configured index name
    pub name: &'static str,
    /// The source the index belongs to
    pub source: ElasticSource,
    /// The mappings and the settings Thorium sets, as `ElasticIndex::create_body` builds them
    pub body: serde_json::Value,
}
