//! Probing the backends a config names and comparing them with the recorded revision

use scylla::client::session::Session;
use std::time::Duration;

use super::{Acknowledgement, DriftReport, LegacyObjects, ObservedSchema, ProbeError, probe};
use crate::models::schema::RevisionSnapshot;
use crate::models::upgrades::{RevisionId, is_legacy};

/// How long a request to Elastic may take, in seconds
const ELASTIC_TIMEOUT_SECS: u64 = 30;

/// Registered legacy objects, which the comparison treats as expected extras
struct Registered;

impl LegacyObjects for Registered {
    /// Ask the legacy registry whether an object is registered
    ///
    /// # Arguments
    ///
    /// * `backend` - The object's backend
    /// * `name` - The object's name
    fn is_legacy(&self, backend: crate::models::upgrades::SchemaBackend, name: &str) -> bool {
        // the registry holds every object an older release created and this one doesn't declare
        is_legacy(backend, name)
    }
}

/// Build an Elastic client from the config, validating certificates like the API does
///
/// # Arguments
///
/// * `conf` - The Elastic config
pub async fn elastic_client(
    conf: &crate::conf::Elastic,
) -> Result<elasticsearch::Elasticsearch, ProbeError> {
    use elasticsearch::auth::Credentials;
    use elasticsearch::http::transport::{SingleNodeConnectionPool, TransportBuilder};
    // every failure here is a failure to reach Elastic
    let failed = |error: String| ProbeError::query("connect to Elastic", error);
    // parse the node and build its certificate validation
    let url = url::Url::parse(&conf.node).map_err(|error| failed(error.to_string()))?;
    let validation = conf
        .try_cert_validation()
        .await
        .map_err(|error| failed(error.to_string()))?;
    // build the transport with the configured credentials
    let transport = TransportBuilder::new(SingleNodeConnectionPool::new(url))
        .auth(Credentials::Basic(
            conf.username.clone(),
            conf.password.clone(),
        ))
        .cert_validation(validation)
        .timeout(Duration::from_secs(ELASTIC_TIMEOUT_SECS))
        .build()
        .map_err(|error| failed(error.to_string()))?;
    Ok(elasticsearch::Elasticsearch::new(transport))
}

/// Probe every backend the config names: Scylla's keyspace, the recorded Redis and S3 layouts, and
/// the configured Elastic indexes; only reads
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config naming the keyspace, the indexes, and Elastic's address
pub async fn probe_configured(
    session: &Session,
    conf: &crate::Conf,
) -> Result<ObservedSchema, ProbeError> {
    // connect to Elastic, then probe everything
    let client = elastic_client(&conf.elastic).await?;
    probe(session, conf, Some(&client)).await
}

/// Probe the live backends and compare them with a recorded revision's snapshot, applying
/// acknowledgements
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config naming the keyspace, the indexes, and Elastic's address
/// * `revision` - The revision the deployment's record names
/// * `acks` - The acknowledgements to apply, beside the registered legacy objects
pub async fn diff(
    session: &Session,
    conf: &crate::Conf,
    revision: &RevisionId,
    acks: &[Acknowledgement],
) -> Result<DriftReport, ProbeError> {
    // the revision's committed snapshot is what the deployment should have
    let expected = match RevisionSnapshot::committed(revision.as_str()) {
        Some(Ok(snapshot)) => snapshot,
        Some(Err(error)) => {
            return Err(ProbeError::Unreadable {
                object: revision.to_string(),
                reason: format!("its snapshot is damaged: {error:?}"),
            });
        }
        None => {
            return Err(ProbeError::Unreadable {
                object: revision.to_string(),
                reason: "this build has no snapshot of the revision".to_owned(),
            });
        }
    };
    // probe the live schema and compare, then apply the acknowledgements
    let observed = probe_configured(session, conf).await?;
    let mut report = DriftReport::compare(&expected, &observed, &Registered);
    report.acknowledge(acks);
    Ok(report)
}
