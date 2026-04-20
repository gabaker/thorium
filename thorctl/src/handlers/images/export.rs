//! Image export support for thorctl

use colored::Colorize;
use kanal::AsyncSender;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use thorium::models::ImageRequest;
use thorium::{CtlConf, Error, Thorium};
use tokio::fs::File;
use tokio::process::Command;

use crate::Args;
use crate::args::images::ExportImages;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::{MonitorMsg, SimpleMonitor, Worker};

type ImageExportMonitor = SimpleMonitor;

pub struct ImageExportWorker {
    /// The Thorium client for this worker
    thorium: Arc<Thorium>,
    /// The progress bars to log progress with
    bar: Bar,
    /// The arguments for downloading repos
    pub cmd: ExportImages,
    /// The channel to send monitor updates on
    pub monitor_tx: AsyncSender<MonitorMsg<ImageExportMonitor>>,
}

impl ImageExportWorker {
    /// Export a single images info docker image
    async fn export_docker(
        &mut self,
        name: &str,
        image_url: &str,
        mut export_path: PathBuf,
    ) -> Result<(), Error> {
        self.bar.set_message("Pulling image");
        let pull_output = Command::new("docker")
            .args(["pull", image_url])
            .output()
            .await
            .map_err(|e| Error::new(format!("Failed to run docker pull: {e}")))?;
        if !pull_output.status.success() {
            let stderr = String::from_utf8_lossy(&pull_output.stderr);
            return Err(Error::new(format!("docker pull failed for '{image_url}': {stderr}")));
        }

        self.bar.set_message("Saving image");
        let mut save_child = Command::new("docker")
            .args(["save", image_url])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(format!("Failed to run docker save: {e}")))?;

        // Pipe docker save stdout through gzip into a .tar.gz file
        let save_stdout: Stdio = save_child
            .stdout
            .take()
            .ok_or_else(|| Error::new("docker save did not produce stdout"))?
            .try_into()
            .map_err(|e| Error::new(format!("Failed to convert stdout to Stdio: {e}")))?;

        export_path.push(format!("{name}.tar.gz"));
        let gz_file = File::create(&export_path)
            .await
            .map_err(|e| Error::new(format!("Failed to create '{}': {e}", export_path.display())))?
            .into_std()
            .await;

        let mut gzip_child = Command::new("gzip")
            .arg("--stdout")
            .stdin(save_stdout)
            .stdout(gz_file)
            .spawn()
            .map_err(|e| Error::new(format!("Failed to run gzip: {e}")))?;

        save_child.wait().await
            .map_err(|e| Error::new(format!("docker save failed: {e}")))?;
        gzip_child.wait().await
            .map_err(|e| Error::new(format!("gzip failed: {e}")))?;
        Ok(())
    }

    pub async fn export(&mut self, name: &str) -> Result<(), Error> {
        self.bar.set_message("Exporting config");
        let images_dir = self.cmd.output.join("images");
        tokio::fs::create_dir_all(&images_dir)
            .await
            .map_err(|e| Error::new(format!("Failed to create export directory: {e}")))?;
        let image = self
            .thorium
            .images
            .get(&self.cmd.group, name)
            .await
            .map_err(|e| Error::new(format!("Failed to get image '{name}': {e}")))?;
        let export_path = images_dir.join(format!("{}.json", &image.name));
        let image_req = ImageRequest::from(image.clone());
        let serialized = serde_json::to_string_pretty(&image_req)
            .map_err(|e| Error::new(format!("Failed to serialize image '{name}': {e}")))?;
        tokio::fs::write(&export_path, &serialized)
            .await
            .map_err(|e| Error::new(format!("Failed to write image config '{name}': {e}")))?;

        if !self.cmd.config_only
            && let Some(image_url) = &image.image
        {
            self.export_docker(&image.name, image_url, images_dir)
                .await?;
        }
        Ok(())
    }
}

/// The trait for what workers should do
#[async_trait::async_trait]
impl Worker for ImageExportWorker {
    /// The cmd part of args for this specific worker
    type Cmd = ExportImages;

    /// The type of jobs to recieve
    type Job = String;

    /// The global monitor to use
    type Monitor = ImageExportMonitor;

    /// Initialize our worker
    async fn init(
        thorium: &Thorium,
        _conf: &CtlConf,
        bar: Bar,
        _args: &Args,
        cmd: Self::Cmd,
        updates: &AsyncSender<MonitorMsg<Self::Monitor>>,
    ) -> Self {
        // create this image export worker
        ImageExportWorker {
            thorium: Arc::new(thorium.clone()),
            bar,
            cmd: cmd.clone(),
            monitor_tx: updates.clone(),
        }
    }

    /// Log an info message
    fn info<T: AsRef<str>>(&mut self, msg: T) {
        self.bar.info(msg)
    }

    /// Start claiming and executing jobs
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to start
    async fn execute(&mut self, job: Self::Job) {
        // set that we are tarring this repository
        self.bar.rename(job.clone());
        self.bar.refresh("", BarKind::Timer);
        // export this image
        if let Err(error) = self.export(&job).await {
            // log this io error
            self.bar
                .error(format!("{}: {}", "Error".bright_red(), error));
        }
        // send an update to our monitor
        if let Err(error) = self.monitor_tx.send(MonitorMsg::Update(())).await {
            // log this io error
            self.bar
                .error(format!("{}: {}", "Error".bright_red(), error));
        }
        // finish our progress bar
        self.bar.finish_and_clear();
    }
}
