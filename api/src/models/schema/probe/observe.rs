//! Putting the probe's halves together: the recorded Redis and S3 layout versions, and the
//! observed schema with its fingerprints

use scylla::client::session::Session;
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::super::{Fingerprint, RevisionSnapshot, SnapshotObject};
use super::{ObservedSchema, ProbeError, SchemaRows};
use crate::models::upgrades::SchemaBackend;

/// The backend rows of `schema_migrations` that record layout versions, by backend
const LAYOUT_ROWS: [(SchemaBackend, &str); 2] =
    [(SchemaBackend::Redis, "redis"), (SchemaBackend::S3, "s3")];

/// Build the observed snapshot entries of recorded layout versions
///
/// Redis and S3 have no server-side schema and no in-store markers, so what the probe observes for
/// them is the layout versions the last `schema apply` recorded; each family (or bucket role)
/// takes the registry's canonical form, its name mapped to its version.
///
/// # Arguments
///
/// * `backend` - `Redis` or `S3`
/// * `layouts` - The recorded version of each family or bucket role
#[must_use]
pub fn layout_objects(
    backend: SchemaBackend,
    layouts: &BTreeMap<String, u32>,
) -> Vec<SnapshotObject> {
    // a BTreeMap yields the families sorted by name
    layouts
        .iter()
        .map(|(family, version)| {
            let mut form = serde_json::Map::new();
            form.insert(family.clone(), json!(version));
            let canonical = Value::Object(form);
            SnapshotObject {
                backend,
                name: family.clone(),
                fingerprint: Fingerprint::of(&canonical),
                canonical,
            }
        })
        .collect()
}

/// Read the layout versions `schema_migrations` records for Redis and S3; only reads
///
/// A backend with no recorded row is left out, since nothing says what its data holds.
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, as Scylla stores its name
/// * `rows` - What `system_schema` holds for the keyspace, to skip a keyspace without the table
pub async fn recorded_layouts(
    session: &Session,
    keyspace: &str,
    rows: &SchemaRows,
) -> Result<BTreeMap<SchemaBackend, BTreeMap<String, u32>>, ProbeError> {
    // nothing is recorded before the first apply creates the table
    let mut recorded = BTreeMap::new();
    if !rows.tables.iter().any(|table| table == "schema_migrations") {
        return Ok(recorded);
    }
    // read each backend row's layouts
    let cql = format!(
        "SELECT layouts FROM {keyspace}.schema_migrations WHERE kind = 'backend' AND id = ?"
    );
    for (backend, id) in LAYOUT_ROWS {
        let result = session
            .query_unpaged(cql.as_str(), (id,))
            .await
            .map_err(|err| ProbeError::query(&cql, err))?
            .into_rows_result()
            .map_err(|err| ProbeError::query(&cql, err))?;
        let row = result
            .maybe_first_row::<(Option<BTreeMap<String, i64>>,)>()
            .map_err(|err| ProbeError::query(&cql, err))?;
        // a recorded row holds every family's version as a bigint
        if let Some((layouts,)) = row {
            let mut versions = BTreeMap::new();
            for (family, version) in layouts.unwrap_or_default() {
                let version = u32::try_from(version).map_err(|_| {
                    let reason = format!("recorded layout version {version} is out of range");
                    ProbeError::unreadable(&format!("{id}:{family}"), reason)
                })?;
                versions.insert(family, version);
            }
            recorded.insert(backend, versions);
        }
    }
    Ok(recorded)
}

impl ObservedSchema {
    /// Build an observed schema from the objects the probe saw on the backends it probed
    ///
    /// Each probed backend gets a fingerprint, computed the way a revision snapshot's is, even when
    /// none of its objects were found; a backend that wasn't probed gets none, so a comparison
    /// leaves it out rather than reporting all of its objects missing.
    ///
    /// # Arguments
    ///
    /// * `objects` - Every observed object in canonical form
    /// * `probed` - The backends that were probed
    #[must_use]
    pub fn new(mut objects: Vec<SnapshotObject>, probed: &[SchemaBackend]) -> ObservedSchema {
        // keep only the probed backends' objects, sorted like a snapshot's
        objects.retain(|object| probed.contains(&object.backend));
        objects
            .sort_by(|left, right| (left.backend, &left.name).cmp(&(right.backend, &right.name)));
        // fingerprint each probed backend, an empty one included, then the deployment
        let mut backends = RevisionSnapshot::backend_fingerprints(&objects);
        for backend in probed {
            backends
                .entry(*backend)
                .or_insert_with(|| Fingerprint::of(&Value::Array(Vec::new())));
        }
        let deployment = RevisionSnapshot::deployment_fingerprint(&backends);
        ObservedSchema {
            objects,
            backends,
            deployment,
        }
    }
}

/// Probe Scylla's keyspace and the layout versions it records; only reads
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace, in any case
pub async fn probe_scylla(
    session: &Session,
    keyspace: &str,
) -> Result<(Vec<SnapshotObject>, Vec<SchemaBackend>), ProbeError> {
    // Scylla stores unquoted names in lowercase
    let keyspace = keyspace.to_lowercase();
    // read and canonicalize the keyspace's objects
    let rows = SchemaRows::read(session, &keyspace).await?;
    let mut objects = rows.objects()?;
    let mut probed = vec![SchemaBackend::Scylla];
    // then the layouts recorded for Redis and S3
    for (backend, layouts) in recorded_layouts(session, &keyspace, &rows).await? {
        objects.extend(layout_objects(backend, &layouts));
        probed.push(backend);
    }
    Ok((objects, probed))
}

/// Probe every backend: Scylla's keyspace, the recorded Redis and S3 layouts, and, given a
/// client, the configured Elastic indexes; only reads
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config naming the keyspace and indexes
/// * `elastic` - The Elastic client, or `None` to leave Elastic unprobed
#[cfg(any(feature = "api", feature = "client"))]
pub async fn probe(
    session: &Session,
    conf: &crate::Conf,
    elastic: Option<&elasticsearch::Elasticsearch>,
) -> Result<ObservedSchema, ProbeError> {
    // Scylla and the layouts it records
    let (mut objects, mut probed) = probe_scylla(session, &conf.thorium.namespace).await?;
    // then the Elastic indexes, when there is a client
    if let Some(client) = elastic {
        objects.extend(super::probe_elastic(client, &conf.elastic).await?);
        probed.push(SchemaBackend::Elastic);
    }
    Ok(ObservedSchema::new(objects, &probed))
}
