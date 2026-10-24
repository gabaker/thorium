//! The Elastic half of the probe: each configured index's live mappings, in the registry's
//! canonical form

use serde_json::{Value, json};

use super::super::{ElasticIndexDef, Fingerprint, SchemaDefinition, SnapshotObject};
use super::ProbeError;
use crate::models::elastic::ElasticIndex;
use crate::models::upgrades::SchemaBackend;

/// Build an index's observed snapshot entry from its `GET <index>/_mapping` response
///
/// The response is keyed by the concrete index name, which differs from the configured name when
/// that is an alias, so its single entry is read whatever its key. The entry is named by the
/// index's stable name, like the registry's, and takes the registry's canonical form, which strips
/// the defaults Elastic adds (`"type": "object"`, `"index": true`, …).
///
/// # Arguments
///
/// * `index` - The index the response describes
/// * `response` - The `_mapping` response body
pub fn mapping_object(
    index: &ElasticIndex,
    response: &Value,
) -> Result<SnapshotObject, ProbeError> {
    // the declared index gives the stable name and source
    let declared = ElasticIndexDef::of(index);
    // the response holds one concrete index with its mappings
    let entries = response
        .as_object()
        .map(|map| map.values().collect::<Vec<_>>());
    let Some([entry]) = entries.as_deref() else {
        let reason = "the _mapping response doesn't hold exactly one index";
        return Err(ProbeError::unreadable(declared.name, reason));
    };
    let mappings = entry.get("mappings").cloned().unwrap_or_else(|| json!({}));
    // build the observed definition and take the registry's canonical form of it
    let observed = SchemaDefinition::Elastic(ElasticIndexDef {
        body: json!({ "mappings": mappings }),
        ..declared
    });
    let canonical = observed.canonical();
    Ok(SnapshotObject {
        backend: SchemaBackend::Elastic,
        name: declared.name.to_owned(),
        fingerprint: Fingerprint::of(&canonical),
        canonical,
    })
}

/// Read every configured index's mappings from the cluster; only reads
///
/// An index that doesn't exist is left out, so the comparison reports it missing.
///
/// # Arguments
///
/// * `client` - The Elastic client to read with
/// * `conf` - The Elastic config naming each index
#[cfg(any(feature = "api", feature = "client"))]
pub async fn probe_elastic(
    client: &elasticsearch::Elasticsearch,
    conf: &crate::conf::Elastic,
) -> Result<Vec<SnapshotObject>, ProbeError> {
    use elasticsearch::indices::IndicesGetMappingParts;
    // read each index's mappings on its own, so a missing index doesn't hide the others
    let mut objects = Vec::with_capacity(ElasticIndex::ALL.len());
    for index in &ElasticIndex::ALL {
        let name = index.full_name(conf);
        let what = format!("GET {name}/_mapping");
        let response = client
            .indices()
            .get_mapping(IndicesGetMappingParts::Index(&[name]))
            .send()
            .await
            .map_err(|err| ProbeError::query(&what, err))?;
        // a missing index is absent from what was observed
        let status = response.status_code();
        if status.as_u16() == 404 {
            continue;
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ProbeError::query(&what, format!("{status}: {body}")));
        }
        let body = response
            .json::<Value>()
            .await
            .map_err(|err| ProbeError::query(&what, err))?;
        objects.push(mapping_object(index, &body)?);
    }
    Ok(objects)
}
