//! The Elasticsearch index creation the manual-tier probe tests need
//!
//! The CI Elasticsearch user cannot create indexes, so this helper is used only by `#[ignore]`d
//! tests and lives in this file, which the CI preflight does not scan.

use elasticsearch::indices::{IndicesCreateParts, IndicesDeleteParts};
use thorium::models::elastic::ElasticIndex;
use thorium::{Conf, Error};

/// Create every index but one with the body the search-streamer creates it with, dropping any
/// index an earlier run left
///
/// # Arguments
///
/// * `client` - The Elastic client to use
/// * `conf` - The config naming the indexes
/// * `skip` - The index to leave uncreated
pub(super) async fn create_indexes(
    client: &elasticsearch::Elasticsearch,
    conf: &Conf,
    skip: Option<&ElasticIndex>,
) -> Result<(), Error> {
    for index in &ElasticIndex::ALL {
        // drop what an earlier run left, then create the index unless it is skipped
        let name = index.full_name(&conf.elastic);
        client
            .indices()
            .delete(IndicesDeleteParts::Index(&[name]))
            .send()
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        if skip.is_some_and(|skip| skip.to_string() == index.to_string()) {
            continue;
        }
        let response = client
            .indices()
            .create(IndicesCreateParts::Index(name))
            .body(index.create_body(&conf.elastic))
            .send()
            .await
            .map_err(|err| Error::new(err.to_string()))?;
        assert!(response.status_code().is_success(), "creating {name}");
    }
    Ok(())
}
