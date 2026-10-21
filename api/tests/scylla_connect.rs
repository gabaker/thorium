//! Tests for the shared "connect to Scylla from the config" code

use thorium::models::schema::connect;
use thorium::test_utilities::CONF;

/// Connecting with the test config opens a session that runs queries
#[tokio::test]
async fn connect_opens_a_working_session() {
    // connect the way every component does
    let session = connect(&CONF)
        .await
        .expect("the test Scylla accepts a session");
    // a query against the schema tables runs
    let result = session
        .query_unpaged("SELECT release_version FROM system.local", &())
        .await
        .expect("the session runs queries");
    assert!(result.into_rows_result().is_ok());
}

/// Connecting to a node that doesn't answer fails with an error within the connect timeout
/// instead of hanging
#[tokio::test]
async fn connect_fails_for_unreachable_nodes() {
    // point the config at a closed port with a short timeout
    let mut conf = CONF.clone();
    conf.scylla.nodes = vec!["127.0.0.1:1".to_owned()];
    conf.scylla.setup_time = 2;
    let started = std::time::Instant::now();
    let result = connect(&conf).await;
    assert!(result.is_err(), "a session opened to a closed port");
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}
