//! Tests the thorctl cluster views

mod common;

use common::{TestUser, assert_success, stdout, unique};
use thorium::Error;
use thorium::models::{ImageScaler, Pools, Resources, WorkerDeleteMap, WorkerRegistrationList};
use thorium::test_utilities::generators;
use uuid::Uuid;

/// `clusters workers` prints each worker's tool instance, and the run of a per-run worker, in
/// place of a pipeline and stage
#[tokio::test]
async fn workers_show_tool_instances() -> Result<(), Error> {
    // register a node with a shared and a per-run worker for our user
    let user = TestUser::new("clusters-workers-tool-instances").await?;
    let node = unique("thebe-node");
    generators::node("cluster0", &node, Resources::default(), &user.admin).await?;
    let tool_instance = Uuid::new_v4();
    let run = Uuid::new_v4();
    let shared = unique("thebe-shared");
    let per_run = unique("thebe-run");
    let workers = WorkerRegistrationList::default()
        .add(
            "cluster0",
            &node,
            &shared,
            &user.username,
            &user.group,
            tool_instance,
            None,
            Resources::default(),
            Pools::Deadline,
        )
        .add(
            "cluster0",
            &node,
            &per_run,
            &user.username,
            &user.group,
            tool_instance,
            Some(run),
            Resources::default(),
            Pools::Deadline,
        );
    user.admin
        .system
        .register_workers(ImageScaler::K8s, &workers)
        .await?;
    // list the cluster's workers
    let args = ["clusters", "workers", "--clusters", "cluster0"];
    let output = user.run("clusters-workers", &args).await;
    assert_success(&output);
    let out = stdout(&output);
    // the header names the tool instance and run and no pipeline or stage
    let header = out.lines().next().unwrap_or_default();
    assert!(header.contains("TOOL INSTANCE"), "{out}");
    assert!(header.contains("RUN"), "{out}");
    assert!(!header.contains("PIPELINE"), "{out}");
    assert!(!header.contains("STAGE"), "{out}");
    // both workers print their tool instance and only the per-run worker prints its run
    let ours = out
        .lines()
        .filter(|line| line.contains(&tool_instance.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(ours.len(), 2, "{out}");
    assert_eq!(
        ours.iter()
            .filter(|line| line.contains(&run.to_string()))
            .count(),
        1,
        "{out}"
    );
    // clean up our workers
    user.admin
        .system
        .delete_workers(
            ImageScaler::K8s,
            &WorkerDeleteMap::default().add(&shared).add(&per_run),
        )
        .await?;
    Ok(())
}
