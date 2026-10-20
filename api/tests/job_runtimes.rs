//! Tests that job runtimes stay within the bounds the rest of Thorium can schedule with

use chrono::{DateTime, Duration, Utc};
use thorium::models::{GenericJob, Image, ImageUpdate, Pipeline, Resources};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail};

/// A runtime far too large to add to any timestamp
const HUGE_RUNTIME: u64 = 10_000_000_000_000;

/// One week in seconds, the longest a job of an image without a timeout may run
const WEEK: u64 = 7 * 24 * 60 * 60;

/// Run a redis command against the test redis
///
/// # Arguments
///
/// * `cmd` - The command to run
async fn redis_exec(cmd: &bb8_redis::redis::Cmd) -> Result<(), Error> {
    // get a connection to the test redis
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // run our command
    let (): () = cmd
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Create a pipeline in a new group and return it along with its first image
///
/// # Arguments
///
/// * `client` - The client to create the pipeline with
async fn pipeline(client: &Thorium) -> Result<(Pipeline, Image), Error> {
    // create a pipeline in a new group
    let group = generators::groups(1, client).await?.remove(0).name;
    let pipe_req = generators::pipelines(&group, 1, false, client)
        .await?
        .remove(0);
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // get the image its first stage runs
    let image = client.images.get(&group, &pipe.order[0][0]).await?;
    Ok((pipe, image))
}

/// Create reactions for a pipeline and claim the jobs for their first stage
///
/// # Arguments
///
/// * `pipe` - The pipeline to create reactions for
/// * `count` - The number of reactions to create and jobs to claim
/// * `client` - The client to create and claim with
async fn claim(pipe: &Pipeline, count: u64, client: &Thorium) -> Result<Vec<GenericJob>, Error> {
    // create reactions so jobs exist to claim
    for _ in 0..count {
        let req = generators::gen_reaction(&pipe.group, pipe, None);
        client.reactions.create(&req).await?;
    }
    // register a node and worker to claim the jobs with
    let stage = &pipe.order[0][0];
    let worker = format!("runtimes-{}", &pipe.name[..8]);
    generators::node("cluster0", "node0", Resources::default(), client).await?;
    generators::worker(
        "cluster0",
        "node0",
        &worker,
        &pipe.group,
        &pipe.name,
        stage,
        client,
    )
    .await?;
    // claim our jobs
    let jobs = client
        .jobs
        .claim(
            &pipe.group,
            &pipe.name,
            stage,
            "cluster0",
            "node0",
            &worker,
            count,
        )
        .await?;
    assert_eq!(jobs.len() as u64, count, "claimed the wrong number of jobs");
    Ok(jobs)
}

/// Get the newest runtime in an image's runtime history
///
/// # Arguments
///
/// * `group` - The group the image is in
/// * `image` - The name of the image
async fn newest_runtime(group: &str, image: &str) -> Result<u64, Error> {
    // get a connection to the test redis
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // read the newest entry in this image's runtime history
    let ns = &test_utilities::CONF.thorium.namespace;
    let history = format!("{ns}:image_ttc:{group}:{image}");
    let newest: Vec<u64> = bb8_redis::redis::cmd("LRANGE")
        .arg(&history)
        .arg(0)
        .arg(0)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    newest
        .first()
        .copied()
        .ok_or_else(|| Error::new(format!("{history} is empty")))
}

/// Check that a job is no longer running and that its stage counts the expected completions
///
/// # Arguments
///
/// * `job` - The job that proceeded
/// * `image` - The image the job ran
/// * `completed` - The number of completed jobs its stage should count
/// * `client` - The client to check with
async fn finished(
    job: &GenericJob,
    image: &Image,
    completed: u64,
    client: &Thorium,
) -> Result<(), Error> {
    // the job is no longer in the running list
    let start = DateTime::from_timestamp(0, 0).unwrap();
    let end = Utc::now() + Duration::weeks(1);
    let running = client
        .jobs
        .running(image.scaler, &start, &end, 10_000)
        .await?;
    assert!(
        running.iter().all(|run| run.job_id != job.id),
        "job {} is still running",
        job.id
    );
    // its stage counts it as completed
    let stats = client.system.stats().await?;
    let counts = stats
        .groups
        .get(&job.group)
        .and_then(|group| group.pipelines.get(&job.pipeline))
        .and_then(|pipe| pipe.stages.get(&job.stage))
        .and_then(|users| users.get("thorium"))
        .ok_or_else(|| Error::new(format!("no stats for stage {}", job.stage)))?;
    assert_eq!(
        counts.completed, completed,
        "stage {} completions",
        job.stage
    );
    Ok(())
}

/// A runtime a little above the image's timeout proceeds and is stored as the timeout, so a job
/// whose finish was noticed late still completes
#[tokio::test]
async fn proceed_clamps_runtimes_slightly_above_the_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim jobs whose image has a timeout
    let (pipe, image) = pipeline(&client).await?;
    let timeout = image.timeout.expect("generated images have a timeout");
    let slack = (timeout / 10).max(300);
    let runtimes = [timeout + 1, timeout + timeout / 10, timeout + slack];
    let jobs = claim(&pipe, runtimes.len() as u64, &client).await?;
    let logs = generators::stage_logs();
    for (completed, (job, runtime)) in (1..).zip(jobs.iter().zip(runtimes)) {
        // a runtime within the slack above the timeout proceeds
        client.jobs.proceed(job, &logs, runtime).await?;
        finished(job, &image, completed, &client).await?;
        // the image's runtime history holds the timeout instead
        let newest = newest_runtime(&pipe.group, &image.name).await?;
        assert_eq!(
            newest, timeout,
            "a runtime of {runtime} was stored as {newest}"
        );
    }
    Ok(())
}

/// A runtime far above the image's timeout is refused, and one within it still proceeds
#[tokio::test]
async fn proceed_refuses_runtimes_far_above_the_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job whose image has a timeout
    let (pipe, image) = pipeline(&client).await?;
    let timeout = image.timeout.expect("generated images have a timeout");
    let slack = (timeout / 10).max(300);
    let job = claim(&pipe, 1, &client).await?.remove(0);
    let logs = generators::stage_logs();
    // runtimes above the timeout and its slack are refused
    for runtime in [HUGE_RUNTIME, timeout + slack + 1] {
        let resp = client.jobs.proceed(&job, &logs, runtime).await;
        fail!(resp, 400);
    }
    // a runtime within the timeout still proceeds and is stored as is
    client.jobs.proceed(&job, &logs, timeout - 1).await?;
    finished(&job, &image, 1, &client).await?;
    assert_eq!(newest_runtime(&pipe.group, &image.name).await?, timeout - 1);
    Ok(())
}

/// A job that finishes after its image's timeout was lowered still proceeds, and its runtime is
/// stored as the new timeout
#[tokio::test]
async fn proceed_survives_a_lowered_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job, then lower its image's timeout below the runtime it will report
    let (pipe, image) = pipeline(&client).await?;
    let job = claim(&pipe, 1, &client).await?.remove(0);
    let lowered = 100;
    let update = ImageUpdate::default().timeout(lowered);
    client
        .images
        .update(&pipe.group, &image.name, &update)
        .await?;
    // a runtime within the old timeout proceeds
    let logs = generators::stage_logs();
    client.jobs.proceed(&job, &logs, lowered + 150).await?;
    finished(&job, &image, 1, &client).await?;
    // the stored runtime is the lowered timeout
    assert_eq!(newest_runtime(&pipe.group, &image.name).await?, lowered);
    Ok(())
}

/// A huge runtime already in an image's history doesn't push its average past the bound
#[tokio::test]
async fn averaged_runtimes_are_clamped() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // put a huge runtime in an image's runtime history
    let (pipe, image) = pipeline(&client).await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let history = format!("{ns}:image_ttc:{}:{}", pipe.group, image.name);
    redis_exec(
        bb8_redis::redis::cmd("LPUSH")
            .arg(&history)
            .arg(HUGE_RUNTIME),
    )
    .await?;
    // average every image's runtime history
    client.images.update_runtimes().await?;
    // the average stays within the image's timeout
    let image = client.images.get(&pipe.group, &image.name).await?;
    let timeout = image.timeout.expect("generated images have a timeout");
    assert!(
        image.runtime <= f64::from(u32::try_from(timeout).unwrap()),
        "an average runtime of {} is above the timeout of {timeout}",
        image.runtime
    );
    Ok(())
}

/// Creating a reaction works even when an image's stored runtime is huge
#[tokio::test]
async fn reactions_survive_huge_stored_runtimes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // store a huge average runtime for the pipeline's image
    let (pipe, image) = pipeline(&client).await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let data = format!("{ns}:image_data:{}:{}", pipe.group, image.name);
    redis_exec(
        bb8_redis::redis::cmd("HSET")
            .arg(&data)
            .arg("runtime")
            .arg(HUGE_RUNTIME),
    )
    .await?;
    // creating a reaction for the pipeline still succeeds
    let req = generators::gen_reaction(&pipe.group, &pipe, None);
    client.reactions.create(&req).await?;
    Ok(())
}

/// A job whose image has no timeout can't proceed with a runtime far above a week, and the
/// image's recorded and average runtimes are clamped to a week
#[tokio::test]
async fn untimed_images_are_capped_at_a_week() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job, then remove its image's timeout
    let (pipe, image) = pipeline(&client).await?;
    let job = claim(&pipe, 1, &client).await?.remove(0);
    let ns = &test_utilities::CONF.thorium.namespace;
    let data = format!("{ns}:image_data:{}:{}", pipe.group, image.name);
    redis_exec(bb8_redis::redis::cmd("HDEL").arg(&data).arg("timeout")).await?;
    let image = client.images.get(&pipe.group, &image.name).await?;
    assert_eq!(image.timeout, None, "the image still has a timeout");
    // a runtime above a week and its slack is refused, and one just above a week is
    // recorded as a week
    let logs = generators::stage_logs();
    let resp = client.jobs.proceed(&job, &logs, WEEK + WEEK / 10 + 1).await;
    fail!(resp, 400);
    client.jobs.proceed(&job, &logs, WEEK + 1).await?;
    assert_eq!(newest_runtime(&pipe.group, &image.name).await?, WEEK);
    // a huge runtime in the image's history averages to at most a week
    let history = format!("{ns}:image_ttc:{}:{}", pipe.group, image.name);
    redis_exec(
        bb8_redis::redis::cmd("LPUSH")
            .arg(&history)
            .arg(HUGE_RUNTIME),
    )
    .await?;
    client.images.update_runtimes().await?;
    let image = client.images.get(&pipe.group, &image.name).await?;
    assert!(
        image.runtime <= f64::from(u32::try_from(WEEK).unwrap()),
        "an untimed image averaged a runtime of {}",
        image.runtime
    );
    Ok(())
}
