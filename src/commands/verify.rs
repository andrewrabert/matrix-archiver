use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use matrix_sdk::config::SyncSettings;

use crate::session::restore_client;

pub struct Verify;

impl Verify {
    pub async fn run(&self, store_path: &Path) -> anyhow::Result<()> {
        let client = restore_client(store_path, 3).await?;

        let settings = SyncSettings::default().timeout(Duration::ZERO);
        client.sync_once(settings).await?;

        eprint!("Recovery key: ");
        let mut recovery_key = String::new();
        std::io::stdin().read_line(&mut recovery_key)?;
        let recovery_key = recovery_key.trim().to_string();

        client
            .encryption()
            .recovery()
            .recover(&recovery_key)
            .await
            .context("Recovery failed")?;

        eprintln!("Recovery complete. E2E keys imported.");
        Ok(())
    }
}
