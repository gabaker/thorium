//! Container tarball export support for `images export` and `pipelines export`

use kanal::AsyncSender;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thorium::{CtlConf, Error, Thorium};

use crate::Args;
use crate::handlers::container;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::{MonitorMsg, SimpleMonitor, Worker};

/// The shared settings every tarball export worker is spawned with
#[derive(Clone)]
pub struct TarballExport {
    /// The directory the `<name>.tar.gz` tarballs are written into
    pub images_dir: PathBuf,
    /// The names of the images whose tarball failed to export, shared across workers
    pub failures: Arc<Mutex<Vec<String>>>,
}

/// A worker that exports a single image's container tarball
pub struct ImageExportWorker {
    /// The progress bar to log progress with
    bar: Bar,
    /// The tarball export settings (output directory and shared failure list)
    cmd: TarballExport,
    /// The channel to send monitor updates on
    monitor_tx: AsyncSender<MonitorMsg<SimpleMonitor>>,
}

impl ImageExportWorker {
    /// Pull an image's container and save it to `<images_dir>/<name>.tar.gz`
    ///
    /// Only the container pull/save runs here; the config file is written separately
    /// by the export's sequential config pass so concurrent tarball workers never race
    /// on prompting over an on-disk config conflict. The container url is supplied by
    /// the caller (read from the config on disk) so the worker doesn't re-fetch the image.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the image being exported (used for the tarball filename)
    /// * `image_url` - The container url to pull and save
    async fn export(&self, name: &str, image_url: &str) -> Result<(), Error> {
        // make sure the images directory exists before saving into it
        tokio::fs::create_dir_all(&self.cmd.images_dir)
            .await
            .map_err(|e| Error::new(format!("Failed to create export directory: {e}")))?;
        // pull the container so it is available locally to save
        container::pull(image_url, &self.bar).await?;
        // save the container to a gzipped tarball named after the image
        let tarball = self.cmd.images_dir.join(format!("{name}.tar.gz"));
        container::save(image_url, &tarball, &self.bar).await
    }
}

#[async_trait::async_trait]
impl Worker for ImageExportWorker {
    /// The shared tarball export settings for this worker
    type Cmd = TarballExport;

    /// The type of jobs to receive: the image name paired with its container url
    type Job = (String, String);

    /// The global monitor to use
    type Monitor = SimpleMonitor;

    /// Initialize our worker
    ///
    /// # Arguments
    ///
    /// * `_thorium` - The Thorium client (unused by this worker)
    /// * `_conf` - The Thorctl config (unused by this worker)
    /// * `bar` - The progress bar this worker logs progress with
    /// * `_args` - The shared Thorctl args (unused by this worker)
    /// * `cmd` - The shared tarball export settings
    /// * `updates` - The channel to send monitor updates on
    async fn init(
        _thorium: &Thorium,
        _conf: &CtlConf,
        bar: Bar,
        _args: &Args,
        cmd: Self::Cmd,
        updates: &AsyncSender<MonitorMsg<Self::Monitor>>,
    ) -> Self {
        // create this image export worker
        ImageExportWorker {
            bar,
            cmd,
            monitor_tx: updates.clone(),
        }
    }

    /// Log an info message
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to log
    fn info<T: AsRef<str>>(&mut self, msg: T) {
        self.bar.info(msg);
    }

    /// Export one image's container tarball
    ///
    /// # Arguments
    ///
    /// * `job` - The image name paired with its container url to export
    async fn execute(&mut self, job: Self::Job) {
        let (name, url) = job;
        // name the bar after the image being exported
        self.bar.rename(name.clone());
        self.bar.refresh("", BarKind::Timer);
        // export this image, recording a failure so the command exits non-zero
        if let Err(error) = self.export(&name, &url).await {
            self.bar.error(error.to_string());
            self.cmd
                .failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(name);
        }
        // send an update to our monitor
        if let Err(error) = self.monitor_tx.send(MonitorMsg::Update(())).await {
            // log a monitor channel send error
            self.bar
                .error(format!("Failed to send a progress update: {error}"));
        }
        // finish our progress bar
        self.bar.finish_and_clear();
    }
}
