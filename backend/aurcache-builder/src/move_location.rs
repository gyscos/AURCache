use crate::build::Builder;
use anyhow::bail;
use aurcache_db::helpers::active_value_ext::ActiveValueExt;
use aurcache_utils::repo_ingest::{Artifact, ingest_pkgs};
use std::fs;
use std::path::PathBuf;

impl Builder {
    /// Move built files from the build container to the host and add them to the repo.
    ///
    /// Returns the version string extracted from the built package filenames (e.g. `"1.2.3-1"`).
    /// This is the version *actually produced by makepkg* and may differ from the version stored
    /// in `builds.version` at enqueue time (which comes from the AUR RPC).
    pub(crate) async fn move_and_add_pkgs(
        &self,
        host_build_path: PathBuf,
    ) -> anyhow::Result<String> {
        // Read the build output directory into in-memory artifacts, then hand off
        // to the shared server-side ingest. Hidden helper files are filtered by
        // the ingest itself.
        let mut artifacts: Vec<Artifact> = Vec::new();
        for entry in fs::read_dir(&host_build_path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                continue;
            }
            let filename = entry
                .file_name()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid filename"))?
                .to_string();
            if filename.starts_with('.') {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            artifacts.push((filename, bytes));
        }

        if artifacts.is_empty() {
            bail!("No files found in build directory");
        }

        let pkg_id = *self.package_model.id.get()?;
        let platform = self.build_model.platform.get()?;

        let version = ingest_pkgs(&self.db, &self.logger, pkg_id, platform, artifacts).await?;

        // Clean up the now-ingested build output.
        for entry in fs::read_dir(&host_build_path)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }

        Ok(version)
    }
}
