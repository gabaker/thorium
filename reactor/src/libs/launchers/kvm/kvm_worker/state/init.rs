//! The logic for a `KvmWorker` in the [`Init`] state

use std::path::{Path, PathBuf};

use thorium::models::Image;
use tokio::process::Command;
use tracing::{Level, event, instrument};

use crate::{
    args::Kvm,
    libs::{
        Error,
        launchers::kvm::os_detector::{GuestOs, OsDetector},
    },
};

use super::{Init, KvmWorkerState, Overlayed};

impl Init {
    /// Create a qcow2 overlay for this worker based on a golden image, saving on disk space and
    /// I/O by leveraging copy-on-write
    ///
    /// # Arguments
    ///
    /// * `args` - The KVM-specific args passed to the reactor
    /// * `image` - The image configuration for this worker
    /// * `os_detector` - A detector that attempts to determine the OS of the guest VM
    #[instrument(skip_all, fields(worker = self.worker))]
    pub async fn overlay(
        self,
        args: &Kvm,
        image: &Image,
        os_detector: &OsDetector,
    ) -> Result<Overlayed, Error> {
        // find the path to the golden image for this worker
        let golden_path = Self::find_golden_image(&args.golden_dir, image).await?;
        // detect the OS of the VM disk image
        let guest_os = match os_detector.detect(&args.temp_dir, &golden_path).await {
            Ok(guest_os) => {
                event!(
                    Level::DEBUG,
                    "Detected disk OS of image '{}': {}",
                    golden_path.display(),
                    guest_os
                );
                guest_os
            }
            // if we hit an error, just use Unknown and proceed
            Err(_err) => GuestOs::Unknown,
        };
        // define an overlay path
        let overlay_path = self.overlay_path(args);
        // create overlay using qemu-img
        let output = Command::new("qemu-img")
            .args([
                "create",
                "-f",
                "qcow2",
                "-F",
                "qcow2",
                "-b",
                golden_path.to_string_lossy().as_ref(),
                overlay_path.to_string_lossy().as_ref(),
            ])
            .output()
            .await?;
        if !output.status.success() {
            return Err(Error::new(format!(
                "Failed to create overlay image '{}' from golden image '{}': {}",
                golden_path.display(),
                overlay_path.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        Ok(self.next_state((guest_os, overlay_path)))
    }

    /// Proceed to the next state as if we actually overlayed the worker
    pub fn mock_overlay(self, args: &Kvm) -> Overlayed {
        // presume the overlay path is defined correctly
        let overlay_path = self.overlay_path(args);
        self.next_state((GuestOs::Unknown, overlay_path))
    }

    /// Find the golden qcow2 image for a given worker
    ///
    /// # Arguments
    ///
    /// * `worker` - the path to the base golden directory
    /// * `image` - the image information
    #[instrument(skip_all)]
    async fn find_golden_image(golden_dir: &Path, image: &Image) -> Result<PathBuf, Error> {
        let mut golden_path = golden_dir.to_path_buf();
        golden_path.push(&image.group);
        golden_path.push(&image.name);
        golden_path.push(&image.name);
        golden_path.set_extension("qcow2");
        // make sure the file exists
        if !tokio::fs::try_exists(&golden_path).await? {
            return Err(Error::new(format!(
                "Failed to find golden image at path '{}'",
                golden_path.display()
            )));
        }
        Ok(golden_path)
    }

    /// Define a path to a temporary overlay for the worker
    fn overlay_path(&self, args: &Kvm) -> PathBuf {
        // define the overlay path in the kvm temporary directory
        let mut overlay_path = args.temp_dir.clone();
        overlay_path.push(self.worker.clone());
        overlay_path.set_extension("qcow2");
        overlay_path
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::libs::launchers::WorkloadSource;
    use crate::libs::launchers::fake::FakeWorkloadSource;
    use thorium::models::ImageScaler;
    use uuid::Uuid;

    /// The golden image of a Kvm tool instance's worker is found under the run's group and the
    /// tool's name
    #[tokio::test]
    async fn golden_image_from_run_group_and_tool() {
        // a golden directory holding winvm's image for group ceres
        let golden = tempfile::tempdir().unwrap();
        let dir = golden.path().join("ceres").join("winvm");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("winvm.qcow2"), b"qcow2").unwrap();
        // a run of winvm in ceres projects the image the launcher reads
        let id = Uuid::new_v4();
        let mut source = FakeWorkloadSource::default();
        let spec = crate::libs::launchers::tests::kvm_spec(None);
        source.serve(id, "winvm", &["vesta", "ceres"], spec);
        let worker = crate::libs::launchers::tests::worker(id, "ceres");
        let image = source
            .workload_image(&worker, ImageScaler::Kvm)
            .await
            .unwrap();
        // the golden image is found at the run group's path
        let path = Init::find_golden_image(golden.path(), &image)
            .await
            .unwrap();
        assert_eq!(path, dir.join("winvm.qcow2"));
    }
}
