//! Periodically folds buffered download counts into the database.
//!
//! The repository file server counts into memory so that serving a file costs
//! no database write (see `aurcache_db::helpers::downloads`). This is the other
//! half: without it the buffer grows for the life of the process and the counts
//! never survive a restart.
//!
//! The interval is the window of counts an unclean shutdown loses. A minute is
//! chosen on that basis rather than on write cost -- the writes are one row per
//! distinct file per interval, which is nothing.

use aurcache_db::helpers::downloads::DownloadBuffer;
use sea_orm::DatabaseConnection;
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// Spawn the flush loop. Runs every `DOWNLOAD_FLUSH_INTERVAL` seconds.
pub fn start_download_flush(db: DatabaseConnection, buffer: Arc<DownloadBuffer>) -> JoinHandle<()> {
    let secs = env::var("DOWNLOAD_FLUSH_INTERVAL")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60)
        .max(1);
    let interval = Duration::from_secs(secs);

    tokio::spawn(async move {
        info!("Download counter flushing every {secs}s");
        loop {
            tokio::time::sleep(interval).await;
            // A failed flush puts its counts back, so the next pass retries
            // them; nothing is lost to a database that is briefly away.
            if let Err(e) = buffer.flush(&db).await {
                warn!("Could not flush download counts: {e}");
            }
        }
    })
}
