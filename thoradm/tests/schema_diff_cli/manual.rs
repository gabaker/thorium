//! The Elasticsearch index creation the manual-tier diff tests need
//!
//! The CI Elasticsearch user cannot create indexes, so this helper is used only by `#[ignore]`d
//! tests and lives in this file, which the CI preflight does not scan.

use thorium::Conf;
use thorium::models::elastic::ElasticIndex;

/// The test config every run starts from
const CONFIG: &str = "../api/tests/thorium-testing.yml";

/// Create every Elastic index the way the search-streamer does, dropping what an earlier run left
pub(super) async fn create_indexes() {
    // the config names the indexes and the node
    let conf = Conf::new(CONFIG).expect("the test config loads");
    let client = reqwest::Client::new();
    for index in &ElasticIndex::ALL {
        // drop the index, then create it with the search-streamer's body
        let url = format!("{}/{}", conf.elastic.node, index.full_name(&conf.elastic));
        client.delete(&url).send().await.expect("Elastic answers");
        let created = client
            .put(&url)
            .json(&index.create_body(&conf.elastic))
            .send()
            .await
            .expect("Elastic answers");
        assert!(created.status().is_success(), "{url}: {}", created.status());
    }
}
