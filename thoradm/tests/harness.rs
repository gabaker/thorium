//! Tests of thoradm's test harness against the backing services

mod common;

use aws_sdk_s3::primitives::ByteStream;
use axum::http::Method;
use common::harness::{self, BUCKET_ROLES, Harness};
use common::mock_s3::{MockS3, S3Behavior, S3Rule};
use common::proxy::{FaultProxy, ProxyAction, ProxyRule};
use std::net::SocketAddr;
use std::sync::Arc;

/// The harness starts an isolated test namespace, its buckets, and a working API
#[tokio::test]
async fn harness_starts_an_isolated_namespace_with_its_api() {
    // start (or reuse) this binary's harness
    let harness = Harness::get().await;
    // the namespace and buckets are named for this run and binary
    assert!(harness.conf.thorium.namespace.starts_with("testing_"));
    assert!(harness.id.ends_with("_harness"), "{}", harness.id);
    harness::guard(&harness.conf).unwrap();
    // the API created the keyspace and answers authenticated requests
    let scylla = harness.scylla().await;
    let keyspaces = scylla
        .query_unpaged(
            "SELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name = ?",
            (harness.conf.thorium.namespace.as_str(),),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .rows_num();
    assert_eq!(keyspaces, 1);
    let me = harness.client().await.users.info().await.unwrap();
    assert_eq!(me.username, "thorium");
    // every bucket role has its bucket
    let s3 = harness.s3();
    for role in BUCKET_ROLES {
        let bucket = harness::bucket_for(&harness.conf, role);
        let head = s3.head_bucket().bucket(&bucket).send().await;
        assert!(head.is_ok(), "missing bucket {bucket} for {role}: {head:?}");
    }
    // the configs thoradm reads point at this namespace, without the test environment's overrides
    let text = std::fs::read_to_string(&harness.cluster_conf).unwrap();
    let written: thorium::Conf = serde_norway::from_str(&text).unwrap();
    assert_eq!(written.thorium.namespace, harness.conf.thorium.namespace);
    assert_eq!(written.scylla.nodes, harness.conf.scylla.nodes);
    let ctl = thorium::CtlConf::from_path(&harness.ctl_conf).unwrap();
    assert_eq!(ctl.keys.api, harness.api_url);
}

/// `MockS3` forwards to the real S3 service by default and its rules reach the SDK's requests
#[tokio::test]
async fn mock_s3_forwards_to_the_real_s3_service() {
    // put MockS3 in front of the harness's S3 service
    let harness = Harness::get().await;
    let mock = MockS3::start(&harness.conf.thorium.s3.endpoint)
        .await
        .unwrap();
    let mut conf = harness.conf.clone();
    conf.thorium.s3.endpoint = mock.endpoint.clone();
    conf.thorium.s3.use_path_style = true;
    let s3 = harness::s3_client(&conf);
    let bucket = harness::bucket_for(&conf, "ephemeral");
    // an object put through the mock lands in the real service
    s3.put_object()
        .bucket(&bucket)
        .key("harness/odd key+%.bin")
        .body(ByteStream::from(vec![7_u8; 4096]))
        .send()
        .await
        .unwrap();
    let direct = harness
        .s3()
        .get_object()
        .bucket(&bucket)
        .key("harness/odd key+%.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(
        direct.body.collect().await.unwrap().into_bytes().len(),
        4096
    );
    // the first GET of the key gets a 503 and the SDK's retry gets the object
    mock.set_rules(vec![S3Rule {
        method: Method::GET,
        key: "harness/*".to_owned(),
        query: None,
        nth: Some(1),
        behavior: S3Behavior::Status {
            status: 503,
            code: "SlowDown".to_owned(),
        },
    }])
    .await;
    let fetched = s3
        .get_object()
        .bucket(&bucket)
        .key("harness/odd key+%.bin")
        .send()
        .await
        .unwrap();
    assert_eq!(
        fetched.body.collect().await.unwrap().into_bytes().to_vec(),
        vec![7_u8; 4096]
    );
    // the mock saw the PUT, the failed GET, and the retried GET, with the decoded key
    let seen = mock.seen().await;
    let statuses: Vec<(String, u16)> = seen
        .iter()
        .map(|request| (request.method.to_string(), request.status))
        .collect();
    assert_eq!(
        statuses,
        vec![
            ("PUT".to_owned(), 200),
            ("GET".to_owned(), 503),
            ("GET".to_owned(), 200)
        ]
    );
    assert!(
        seen.iter()
            .all(|request| request.key == "harness/odd key+%.bin")
    );
    assert!(seen.iter().all(|request| request.bucket == bucket));
    harness::empty_bucket(&harness.s3(), &bucket).await.unwrap();
}

/// A session built for a config routed through a proxy reaches Scylla only through the proxy
#[tokio::test]
async fn scylla_session_reaches_nodes_through_a_proxy() {
    // put a proxy in front of the harness's Scylla node
    let harness = Harness::get().await;
    let node: SocketAddr = harness.conf.scylla.nodes[0].parse().unwrap();
    let proxy = FaultProxy::start(node).await.unwrap();
    // build a session that knows only the proxy and translates every address the node
    // advertises to it
    let mut conf = harness.conf.clone();
    conf.scylla.nodes = vec![proxy.listen.to_string()];
    let translator =
        common::proxy::scylla_translator(&*harness.scylla().await, node, proxy.listen).await;
    thoradm::shared::scylla::route_through(&proxy.listen.to_string(), Arc::new(translator));
    let session = thoradm::shared::scylla::get_client(&conf).await.unwrap();
    // queries work and every connection went through the proxy
    session
        .query_unpaged("SELECT release_version FROM system.local", &[])
        .await
        .unwrap();
    assert!(proxy.connections() >= 1);
    // resetting every proxied connection breaks the session's next query, so none bypass it
    proxy
        .set_rules(vec![ProxyRule {
            from_connection: None,
            after_bytes: 0,
            upstream: true,
            action: ProxyAction::Reset,
            times: None,
        }])
        .await;
    let broken = session
        .query_unpaged("SELECT release_version FROM system.local", &[])
        .await;
    assert!(
        broken.is_err(),
        "a query should fail while the proxy resets everything"
    );
}
