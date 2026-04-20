use colored::Colorize;
use kanal::AsyncSender;
use std::sync::Arc;
use thorium::models::PipelineRequest;
use thorium::{CtlConf, Error, Thorium};

use crate::args::pipelines::ImportPipelines;
use crate::handlers::progress::{Bar, BarKind};
use crate::handlers::{MonitorMsg, SimpleMonitor, Worker};
use crate::Args;

type PipelineImportMonitor = SimpleMonitor;

pub struct PipelineImportWorker {
    /// The Thorium client for this worker
    thorium: Arc<Thorium>,
    /// The progress bars to log progress with
    bar: Bar,
    /// The arguments for downloading repos
    pub cmd: ImportPipelines,
    /// The channel to send monitor updates on
    pub monitor_tx: AsyncSender<MonitorMsg<PipelineImportMonitor>>,
}

impl PipelineImportWorker {
    /// Import an pipeline from a specific group by name
    pub async fn import(&mut self, name: &str) -> Result<(), Error> {
        self.bar.set_message("Importing pipeline");
        let file_path = self.cmd.import.join("pipelines").join(format!("{name}.json"));
        let pipeline_str = tokio::fs::read_to_string(&file_path)
            .await
            .map_err(|e| Error::new(format!("Failed to read pipeline '{name}': {e}")))?;
        let mut pipeline_req: PipelineRequest = serde_json::from_str(&pipeline_str)
            .map_err(|e| Error::new(format!("Failed to parse pipeline '{name}': {e}")))?;
        pipeline_req.group = self.cmd.group.clone();
        self.thorium
            .pipelines
            .create(&pipeline_req)
            .await
            .map_err(|e| Error::new(format!("Failed to create pipeline '{name}': {e}")))?;
        Ok(())
    }
}

/// The trait for what workers should do
#[async_trait::async_trait]
impl Worker for PipelineImportWorker {
    /// The cmd part of args for this specific worker
    type Cmd = ImportPipelines;

    /// The type of jobs to recieve
    type Job = String;

    /// The global monitor to use
    type Monitor = PipelineImportMonitor;

    /// Initialize our worker
    async fn init(
        thorium: &Thorium,
        _conf: &CtlConf,
        bar: Bar,
        _args: &Args,
        cmd: Self::Cmd,
        updates: &AsyncSender<MonitorMsg<Self::Monitor>>,
    ) -> Self {
        // create this pipeline export worker
        PipelineImportWorker {
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
        // export this pipeline
        if let Err(error) = self.import(&job).await {
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
