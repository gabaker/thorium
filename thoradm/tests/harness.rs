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

/// A local run without a run id gets a random one, and an id that is given is used as it is
/// (lowercase, with dashes as underscores)
#[test]
fn a_local_run_id_is_given_or_random() {
    assert_eq!(
        harness::choose_run_id(None, false, || "abc123".to_owned()),
        Ok("abc123".to_owned())
    );
    assert_eq!(
        harness::choose_run_id(Some("Lane-7"), false, || unreachable!()),
        Ok("lane_7".to_owned())
    );
}

/// An empty run id is refused whether or not the run is in CI, since two runs with the same
/// empty id would wipe each other's keyspaces
#[test]
fn an_empty_run_id_is_refused() {
    for ci in [false, true] {
        let error = harness::choose_run_id(Some(""), ci, || unreachable!()).expect_err("refused");
        assert!(error.contains("THORADM_TEST_RUN"), "{error}");
    }
}

/// A CI run must name its run id, and the id must be the pipeline's stamp and five hex digits
#[test]
fn a_ci_run_needs_the_pipelines_run_id() {
    let missing = harness::choose_run_id(None, true, || unreachable!()).expect_err("refused");
    assert!(missing.contains("THORADM_TEST_RUN"), "{missing}");
    for bad in [
        "1",
        "ci",
        "123456-abcde",
        "0123456-abcd",
        "0123456-abcdeg",
        "0123456_abcde",
    ] {
        let error = harness::choose_run_id(Some(bad), true, || unreachable!()).expect_err(bad);
        assert!(error.contains("THORADM_TEST_RUN"), "{error}");
    }
    assert_eq!(
        harness::choose_run_id(Some("01234ab-5def0"), true, || unreachable!()),
        Ok("01234ab_5def0".to_owned())
    );
}

/// Scanning lists every key that matches across scan pages, and only those, treating nothing
/// else as a pattern
#[tokio::test]
async fn scan_keys_lists_only_matching_keys_across_pages() {
    // seed more keys than one scan page returns, beside keys that must not match
    let harness = Harness::get().await;
    let ns = &harness.conf.thorium.namespace;
    let pool = harness::redis_pool(&harness.conf).await.unwrap();
    let mut connection = pool.get().await.unwrap();
    let mut wanted = Vec::new();
    for index in 0..2500 {
        let key = format!("{ns}:scan_keys:{index}");
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("1")
            .query_async(&mut *connection)
            .await
            .unwrap();
        wanted.push(key);
    }
    let other = format!("{ns}:scan_other:1");
    let _: () = redis::cmd("SET")
        .arg(&other)
        .arg("1")
        .query_async(&mut *connection)
        .await
        .unwrap();
    // the scan finds exactly the wanted keys
    let mut found = harness::scan_keys(&mut *connection, &format!("{ns}:scan_keys:*"))
        .await
        .unwrap();
    found.sort();
    wanted.sort();
    assert_eq!(found, wanted);
    // a pattern that matches nothing finds nothing
    let none = harness::scan_keys(&mut *connection, &format!("{ns}:absent:*"))
        .await
        .unwrap();
    assert!(none.is_empty(), "{none:?}");
}

/// Every bucket the harness creates expires its objects after a day, so a run that is killed
/// leaves nothing behind for long
#[tokio::test]
async fn the_harness_buckets_expire_their_objects_after_a_day() {
    // read the lifecycle of each bucket the harness made
    let harness = Harness::get().await;
    let s3 = harness.s3();
    for role in BUCKET_ROLES {
        let bucket = harness::bucket_for(&harness.conf, role);
        let lifecycle = s3
            .get_bucket_lifecycle_configuration()
            .bucket(&bucket)
            .send()
            .await
            .unwrap_or_else(|err| panic!("no lifecycle on {bucket}: {err:?}"));
        let days: Vec<Option<i32>> = lifecycle
            .rules()
            .iter()
            .map(|rule| {
                rule.expiration()
                    .and_then(aws_sdk_s3::types::LifecycleExpiration::days)
            })
            .collect();
        assert_eq!(days, vec![Some(1)], "{bucket}");
    }
}

/// Every node a cluster advertises, not only the one the test reached, is mapped to the proxy on
/// both the node's port and the proxy's, so a multi-node cluster's connections all go through it
#[test]
fn the_translator_maps_every_node_of_a_cluster() {
    // a three node cluster behind one proxy
    let node: SocketAddr = "127.0.0.1:9042".parse().unwrap();
    let proxy: SocketAddr = "127.0.0.1:40000".parse().unwrap();
    let advertised: Vec<std::net::IpAddr> = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]
        .iter()
        .map(|address| address.parse().unwrap())
        .collect();
    let map = common::proxy::translations(node, proxy, &advertised);
    // the test's own address and the proxy map to the proxy
    assert_eq!(map.get(&node), Some(&proxy));
    assert_eq!(map.get(&proxy), Some(&proxy));
    // every advertised address maps on either port
    for address in &advertised {
        for port in [node.port(), proxy.port()] {
            let key = SocketAddr::new(*address, port);
            assert_eq!(map.get(&key), Some(&proxy), "{key}");
        }
    }
    // and nothing else
    assert_eq!(map.len(), 2 + 2 * advertised.len());
}
