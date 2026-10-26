use thorium::{
    Error,
    client::Thorium,
    models::{Image, ImageUpdate},
};

use crate::args::Args;
use crate::args::{
    DescribeCommand,
    images::{DescribeImages, GetImages, Images},
};

use crate::utils;

mod bans;
mod edit;
mod notifications;

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "linux", target_os = "macos"))] {
        use std::path::Path;
        use std::sync::{Arc, Mutex};

        use futures::stream::{self, StreamExt};
        use thorium::CtlConf;
        use thorium::models::{ImageRequest, ImageScaler};

        use crate::args::images::{ExportImages, ImportImages};
        use crate::handlers::imports::{self, editor::resolve_editor, merge::IMAGE_FIELD_ORDER};
        use crate::handlers::exports::{
            self, ConfigExport, DiskConflictResolver, ExportReport, WriteOutcome,
        };
        use crate::handlers::progress::{Bar, BarKind};
        use super::Controller;

        mod export;
        pub(crate) mod import;

        use export::{ImageExportWorker, TarballExport};
    }
}

/// What the user is told when Thorium refuses to write an image
const IMAGE_WRITES_POINTER: &str =
    "Images can no longer be created or updated; use tools (thorctl tools create|edit|publish)";

/// The name probed to find out whether Thorium accepts image writes at all
const IMAGE_WRITE_PROBE: &str = "thorctl-write-check";

/// Check whether an error is Thorium refusing an image write
///
/// # Arguments
///
/// * `err` - The error to check
fn is_image_write_refused(err: &Error) -> bool {
    // image writes are refused with 410 Gone
    err.status() == Some(http::StatusCode::GONE)
}

/// Replace a refused image write's raw error with the pointer to tools
///
/// Any other error is returned untouched.
///
/// # Arguments
///
/// * `err` - The error from an image write
fn point_refused_write_at_tools(err: Error) -> Error {
    // only the refusal is replaced, so every other failure keeps its own message
    if is_image_write_refused(&err) {
        Error::new(IMAGE_WRITES_POINTER)
    } else {
        err
    }
}

/// Fail with the pointer to tools when Thorium refuses image writes
///
/// Sends an update that changes nothing, which Thorium refuses before it looks for the
/// image, so a command that would otherwise open an editor or push containers can stop
/// first. Any other answer, including a missing image, means writes are accepted.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `group` - The group the command would write images in
/// * `name` - The image the command would write
async fn ensure_image_writes_accepted(
    thorium: &Thorium,
    group: &str,
    name: &str,
) -> Result<(), Error> {
    // an empty update is harmless where image writes are still accepted
    match thorium
        .images
        .update(group, name, &ImageUpdate::default())
        .await
    {
        // a refusal ends the command with the pointer
        Err(err) if is_image_write_refused(&err) => Err(point_refused_write_at_tools(err)),
        // anything else leaves the command to run and report its own errors
        _ => Ok(()),
    }
}

/// Prints the rows of the `images get` table
struct GetImagesLine;

impl GetImagesLine {
    /// Print the table header
    pub fn header() {
        println!(
            "{:<30} | {:<20} | {:<10} | {:<50}",
            "IMAGE NAME", "GROUP", "SCALER", "DESCRIPTION",
        );
        println!("{:-<31}+{:-<22}+{:-<12}+{:-<50}", "", "", "", "");
    }

    /// Print an image's info
    ///
    /// # Arguments
    ///
    /// * `image` - The image to print
    pub fn print_image(image: &Image) {
        // limit our description preview to at most 40 characters so the column stays aligned
        let description = utils::render::truncate_description(image.description.as_deref(), 40);
        // print our image info
        println!(
            "{:<30} | {:<20} | {:<10} | {}",
            image.name,
            image.group,
            image.scaler.as_str(),
            description
        );
    }
}

/// Get image info from Thorium
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The image get command to execute
async fn get(thorium: Thorium, cmd: &GetImages) -> Result<(), Error> {
    GetImagesLine::header();
    // get the current user's groups if no groups were specified
    let groups = if cmd.groups.is_empty() {
        utils::groups::get_all_groups(&thorium).await?
    } else {
        cmd.groups.clone()
    };
    // get image cursors for all groups specified
    let image_cursors = groups.iter().map(|group| {
        let cursor = thorium
            .images
            .list(group)
            .page_size(cmd.page_size as u64)
            .details();
        // only apply a limit if the user didn't request no limit
        if cmd.no_limit {
            cursor
        } else {
            cursor.limit(cmd.limit as u64)
        }
    });
    // retrieve the images in each cursor until we've reached our limit
    // or all cursors are exhausted
    let mut images: Vec<Image> = Vec::new();
    for mut cursor in image_cursors {
        while !cursor.exhausted {
            cursor.next().await?;
            // remove images with a non-matching scaler
            if let Some(scaler) = &cmd.scaler {
                cursor.details.retain(|image| &image.scaler == scaler);
            }
            if cmd.alpha {
                // save images for sorting later if alphabetize flag is set
                images.append(&mut cursor.details);
            } else {
                // otherwise print immediately if no need to alphabetize
                cursor.details.iter().for_each(GetImagesLine::print_image);
            }
        }
    }
    // sort and print in alphabetical order if alpha flag was set
    if cmd.alpha {
        images.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        images.iter().for_each(GetImagesLine::print_image);
    }
    Ok(())
}

/// Describe a specific image in full
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The describe image command to execute
async fn describe(thorium: Thorium, cmd: &DescribeImages) -> Result<(), Error> {
    cmd.describe(&thorium).await
}

/// Delete images from Thorium
///
/// Repeated names are deleted once. Images that aren't found are skipped with a warning and
/// other failures don't stop the rest of the batch; the command fails at the end if any image
/// couldn't be deleted.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The delete images command to execute
async fn delete(thorium: Thorium, cmd: &crate::args::images::DeleteImages) -> Result<(), Error> {
    use colored::Colorize;
    use itertools::Itertools;
    // repeated names would only fail as not-found on their second delete
    let names: Vec<&String> = cmd.images.iter().unique().collect();
    // deleting is irreversible, so confirm exactly what will be removed
    if !cmd.skip_confirm {
        // fail clearly (not with a raw dialoguer error) when we can't prompt
        utils::require_confirm_terminal("--skip-confirm")?;
        println!("{}", "Images to delete:".bright_red());
        for name in &names {
            println!("  {}", utils::resource_id(&cmd.group, name));
        }
        let confirmed = dialoguer::Confirm::new()
            .with_prompt("Delete the images listed above?")
            .default(false)
            .interact()?;
        if !confirmed {
            return Ok(());
        }
    }
    // delete each image, continuing past failures so one bad image doesn't strand the rest
    let mut failed: Vec<String> = Vec::new();
    for name in names {
        match thorium.images.delete(&cmd.group, name).await {
            Ok(_) => println!("Deleted image '{}'", utils::resource_id(&cmd.group, name)),
            // a missing image isn't fatal to the rest of the batch
            Err(err) if err.status() == Some(http::StatusCode::NOT_FOUND) => {
                eprintln!(
                    "{}: image '{}' not found; skipping",
                    "Warning".bright_yellow(),
                    utils::resource_id(&cmd.group, name)
                );
            }
            Err(err) => {
                eprintln!(
                    "{}: Failed to delete image '{}': {err}",
                    "Error".bright_red(),
                    utils::resource_id(&cmd.group, name)
                );
                failed.push(format!("'{}'", utils::resource_id(&cmd.group, name)));
            }
        }
    }
    // exit non-zero if anything failed to delete
    if !failed.is_empty() {
        return Err(Error::new(format!(
            "Failed to delete {} image(s): {}",
            failed.len(),
            failed.join(", ")
        )));
    }
    Ok(())
}

/// Import images into Thorium through the shared conflict engine
///
/// The import stops before any container or config work when Thorium refuses image writes.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The import images command to execute
/// * `conf` - The Thorctl config
/// * `workers` - The maximum number of concurrent workers to use
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn import(
    thorium: &Thorium,
    cmd: &ImportImages,
    conf: &CtlConf,
    workers: usize,
) -> Result<(), Error> {
    let progress = Bar::new("images import", "Importing images", BarKind::Timer);
    // build the shared import options from the command
    let opts = import::ImageImportOpts::from_cmd(cmd, workers);
    // no explicit list imports every config in the export directory
    let names = if cmd.images.is_empty() {
        imports::list_export_configs(&cmd.import, "images").await?
    } else {
        imports::dedup_names(cmd.images.clone(), &progress)
    };
    // stop before any container or config work when image writes are refused
    ensure_image_writes_accepted(
        thorium,
        &cmd.group,
        cmd.images.first().map_or(IMAGE_WRITE_PROBE, String::as_str),
    )
    .await?;
    // load the requests and check what already exists before changing anything
    let images = import::categorize_from_disk(thorium, &opts, &names, &progress).await?;
    // no pipelines for a standalone image import; the shared driver handles the rest
    Box::pin(imports::disk::run_disk_import(
        thorium,
        conf,
        &progress,
        &opts,
        images,
        Vec::new(),
        cmd.rollback_on_failure,
    ))
    .await
}

/// The options for exporting image configs and their container tarballs
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct ImageExportOpts<'a> {
    /// The group to export images from
    pub group: &'a str,
    /// The root export directory (configs and tarballs go in its `images` subdirectory)
    pub output: &'a Path,
    /// Only export configs, without container tarballs
    pub config_only: bool,
    /// Open each config in an editor for review before writing
    pub review: bool,
    /// The editor used for reviews
    pub editor: &'a str,
}

/// Export image configs and their container tarballs into an export directory
///
/// Configs are written sequentially through the shared `resolver` so on-disk
/// conflicts can be prompted for and "all" choices carry across the whole export.
/// Tarballs are only saved for K8s images (the only scaler import loads them for)
/// and run afterwards in a worker pool for every config that ended up on disk, even
/// when the user quit partway, so no exported config is left without its tarball.
/// Failures are recorded in the returned report instead of aborting the export.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `opts` - The image export options
/// * `names` - The (de-duplicated) names of the images to export
/// * `resolver` - The on-disk conflict resolver shared across the whole export
/// * `progress` - The progress bar to log through (cleared before tarballs export)
/// * `args` - The shared Thorctl args (for the worker count)
/// * `conf` - The Thorctl config
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) async fn export_images_with(
    thorium: &Thorium,
    opts: &ImageExportOpts<'_>,
    names: Vec<String>,
    resolver: &mut DiskConflictResolver,
    progress: &Bar,
    args: &Args,
    conf: &CtlConf,
) -> ExportReport {
    // collect failures and a Quit across the config and tarball passes
    let mut report = ExportReport::default();
    // configs and tarballs both live in the export's images directory
    let images_dir = opts.output.join("images");
    // fetch the images concurrently (bounded by --workers, never zero); `buffered` keeps
    // the input order so prompts and errors come out in a stable order
    let fetch_workers = std::cmp::min(args.workers, names.len()).max(1);
    let fetched: Vec<(String, Result<Image, Error>)> = stream::iter(names)
        .map(|name| async move {
            let result = thorium.images.get(opts.group, &name).await;
            (name, result)
        })
        .buffered(fetch_workers)
        .collect()
        .await;
    // write each config sequentially so conflicts can be resolved interactively
    let cfg = ConfigExport {
        kind: "image",
        dir: images_dir.clone(),
        order: IMAGE_FIELD_ORDER,
        review: opts.review,
        editor: opts.editor,
    };
    let exported = exports::export_configs::<Image, ImageRequest>(
        &cfg,
        fetched,
        |request: &ImageRequest| request.name.as_str(),
        resolver,
        progress,
        &mut report,
    )
    .await;
    // queue a tarball for every K8s image config now on disk that has a container url.
    // A config left as-is (identical or a skipped conflict) only re-exports a missing
    // tarball, so a large archive that's already present isn't re-pulled and re-saved
    let mut docker_jobs: Vec<(String, String)> = Vec::new();
    if !opts.config_only {
        for config in exported {
            // import only loads tarballs for K8s images, so other scalers stay config-only
            if config.request.scaler != ImageScaler::K8s {
                continue;
            }
            if let Some(url) = config.request.image {
                let tarball = images_dir.join(format!("{}.tar.gz", config.name));
                if config.outcome == WriteOutcome::Written || !tarball.exists() {
                    docker_jobs.push((config.name, url));
                }
            }
        }
    }
    // export the queued tarballs in parallel, collecting per-image failures
    if !docker_jobs.is_empty() {
        // clear the config bar so it doesn't fight with the worker bars
        progress.finish_and_clear();
        let failures = Arc::new(Mutex::new(Vec::new()));
        let tarballs = TarballExport {
            images_dir,
            failures: failures.clone(),
        };
        // never spawn more workers than jobs, and never zero (no one would take the jobs)
        let workers = std::cmp::min(args.workers, docker_jobs.len()).max(1);
        let mut controller = Controller::<ImageExportWorker>::spawn(
            "Exporting Images",
            thorium,
            workers,
            conf,
            args,
            &tarballs,
        )
        .await;
        for (name, url) in docker_jobs {
            // a job that can't be queued never runs, so count it as failed
            if let Err(error) = controller.add_job((name.clone(), url)).await {
                controller.error(&format!(
                    "Failed to queue tarball export for image '{name}': {error}"
                ));
                report.fail("image tarball", name);
            }
        }
        // wait for the workers; a controller error means results may be incomplete
        if let Err(error) = controller.finish().await {
            progress.error(format!("Failed to finish exporting tarballs: {error}"));
            report.fail("image tarball", "(worker pool)");
        }
        // fold the workers' failures into the report
        let failed = std::mem::take(
            &mut *failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for name in failed {
            report.fail("image tarball", name);
        }
    }
    report
}

/// Export images from Thorium
///
/// A Quit at a conflict prompt stops writing further configs, still exports the
/// tarballs of configs already written, and exits non-zero ("Export stopped early").
/// Configs left untouched because no terminal was available to ask about them also end the
/// export with an error unless `--skip-conflicts` was given.
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `cmd` - The export images command to execute
/// * `args` - The shared Thorctl args (for the worker count)
/// * `conf` - The Thorctl config
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn export(
    thorium: &Thorium,
    cmd: &ExportImages,
    args: &Args,
    conf: &CtlConf,
) -> Result<(), Error> {
    // fail before doing any work if --review can't open an editor
    exports::require_review_terminal(cmd.review)?;
    // no explicit list exports every image in the group
    let listed = if cmd.images.is_empty() {
        let names: Vec<String> = utils::images::list_all_images(thorium, &cmd.group)
            .await?
            .into_iter()
            .map(|image| image.name)
            .collect();
        // an empty group has nothing to export; say so rather than silently succeeding
        if names.is_empty() {
            return Err(Error::new(format!(
                "No images found in group '{}'",
                cmd.group
            )));
        }
        Some(names)
    } else {
        None
    };
    // --quiet gets an inert bar that still prints warnings and errors
    let progress = Bar::new_or_quiet(
        "images export",
        "Exporting image configs",
        BarKind::Timer,
        args.quiet,
    );
    // explicit names are de-duplicated so one image is never exported twice at once
    let names = listed.unwrap_or_else(|| imports::dedup_names(cmd.images.clone(), &progress));
    // build a resolver that prompts only when interactive and on a terminal
    let can_prompt = !cmd.skip_conflicts && imports::is_interactive_terminal();
    let editor = resolve_editor(cmd.editor.as_deref(), conf);
    let mut resolver = DiskConflictResolver::new(cmd.overwrite, can_prompt, editor.to_string())
        .explicit_skip(cmd.skip_conflicts);
    // export the configs and tarballs
    let opts = ImageExportOpts {
        group: &cmd.group,
        output: &cmd.output,
        config_only: cmd.config_only,
        review: cmd.review,
        editor,
    };
    let mut report =
        export_images_with(thorium, &opts, names, &mut resolver, &progress, args, conf).await;
    // report the configs left untouched, which refuses the export unless --skip-conflicts asked
    resolver.settle(&mut report, &progress);
    // print the final banner and exit non-zero on any failure or a Quit
    progress.refresh(report.banner(), BarKind::Timer);
    progress.finish();
    report.into_result()
}

/// Handle all images commands
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The images command to execute
pub async fn handle(args: &Args, cmd: &Images) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        super::update::ask_update(&thorium).await?;
    }
    // call the right images handler
    match cmd {
        Images::Get(cmd) => get(thorium, cmd).await,
        Images::Describe(cmd) => describe(thorium, cmd).await,
        Images::Notifications(cmd) => notifications::handle(thorium, cmd).await,
        Images::Bans(cmd) => bans::handle(thorium, cmd).await,
        Images::Edit(cmd) => edit::edit(thorium, &conf, cmd).await,
        Images::Delete(cmd) => delete(thorium, cmd).await,
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Images::Import(cmd) => {
            // resolve the container runtime (docker/podman) before any image work
            crate::handlers::container::init_runtime(
                args.container_runtime,
                conf.container_runtime,
            );
            import(&thorium, cmd, &conf, args.workers).await
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Images::Export(cmd) => {
            // resolve the container runtime (docker/podman) before any image work
            crate::handlers::container::init_runtime(
                args.container_runtime,
                conf.container_runtime,
            );
            export(&thorium, cmd, args, &conf).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use thorium::Thorium;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Start a mock API that answers every request with one status and body
    ///
    /// Returns the mock's url and the request lines it received.
    ///
    /// # Arguments
    ///
    /// * `code` - The status code to answer with
    /// * `body` - The body to answer with
    async fn mock(code: u16, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        // bind to a random local port and answer in the background
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let lines = Arc::new(Mutex::new(Vec::new()));
        let inner = lines.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                // read the request head, which is all the mock needs
                let mut buf = vec![0_u8; 8192];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..read]).to_string();
                inner
                    .lock()
                    .unwrap()
                    .push(text.lines().next().unwrap_or_default().to_owned());
                // answer with the configured status and body
                let resp = format!(
                    "HTTP/1.1 {code} Mock\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (host, lines)
    }

    /// A refused write becomes the pointer to tools and every other error is left alone
    #[test]
    fn refused_write_becomes_the_tools_pointer() {
        // the API's refusal is replaced with the pointer
        let gone = Error::Thorium {
            code: http::StatusCode::GONE,
            msg: Some("Images can no longer be created or updated; see /api/entities/".to_owned()),
        };
        let mapped = point_refused_write_at_tools(gone);
        assert_eq!(mapped.msg().as_deref(), Some(IMAGE_WRITES_POINTER));
        assert!(
            mapped
                .to_string()
                .contains("thorctl tools create|edit|publish")
        );
        // an unrelated failure keeps its own status and message
        let missing = Error::Thorium {
            code: http::StatusCode::NOT_FOUND,
            msg: Some("no such image".to_owned()),
        };
        let kept = point_refused_write_at_tools(missing);
        assert_eq!(kept.status(), Some(http::StatusCode::NOT_FOUND));
        assert_eq!(kept.msg().as_deref(), Some("no such image"));
    }

    /// The write check stops with the pointer when Thorium answers 410 Gone
    #[tokio::test]
    async fn write_check_stops_with_the_pointer_when_refused() {
        // a Thorium that refuses image writes
        let (host, lines) = mock(410, "Images can no longer be created or updated").await;
        let thorium = Thorium::build(&host).token("token").build().await.unwrap();
        // the check fails with only the pointer, not the raw refusal
        let err = ensure_image_writes_accepted(&thorium, "corn", "husker")
            .await
            .expect_err("a refusal fails the check");
        assert_eq!(err.msg().as_deref(), Some(IMAGE_WRITES_POINTER));
        // the check probed the update route of the image it was given
        assert_eq!(
            lines.lock().unwrap().as_slice(),
            ["PATCH /api/images/corn/husker HTTP/1.1"]
        );
    }

    /// The write check lets a command run when Thorium does not refuse the probe
    #[tokio::test]
    async fn write_check_passes_when_the_image_is_missing() {
        // a Thorium that still accepts image writes answers 404 for an unknown image
        let (host, _lines) = mock(404, "no such image").await;
        let thorium = Thorium::build(&host).token("token").build().await.unwrap();
        // a missing image is not a refusal
        ensure_image_writes_accepted(&thorium, "corn", IMAGE_WRITE_PROBE)
            .await
            .expect("only a refusal fails the check");
    }
}
