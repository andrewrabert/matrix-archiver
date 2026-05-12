use std::path::Path;

use anyhow::{Context, bail};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::RequestConfig;
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::ruma::{OwnedDeviceId, UserId};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, SessionMeta, SessionTokens, SqliteStateStore, StateStore};
use serde::{Deserialize, Serialize};

const SESSION_KEY: &[u8] = b"matrix-cli-session";

#[derive(Serialize, Deserialize)]
pub struct StoredSession {
    pub homeserver_url: String,
    pub user_id: String,
    pub device_id: String,
    pub access_token: String,
}

pub async fn save_session(client: &Client, session: &StoredSession) -> anyhow::Result<()> {
    let json = serde_json::to_vec(session)?;
    client
        .state_store()
        .set_custom_value_no_read(SESSION_KEY, json)
        .await?;
    Ok(())
}

pub async fn restore_client(store_path: &Path, retry_limit: usize) -> anyhow::Result<Client> {
    if !store_path.exists() {
        bail!("Not logged in. Run `matrix-archiver login` first.");
    }

    let store = SqliteStateStore::open(store_path, None).await?;
    let data = store
        .get_custom_value(SESSION_KEY)
        .await?
        .context("Not logged in. Run `matrix-archiver login` first.")?;
    let stored: StoredSession = serde_json::from_slice(&data)?;

    let client = Client::builder()
        .server_name_or_homeserver_url(&stored.homeserver_url)
        .sqlite_store(store_path, None)
        .request_config(RequestConfig::default().retry_limit(retry_limit))
        .with_encryption_settings(EncryptionSettings {
            backup_download_strategy: BackupDownloadStrategy::OneShot,
            ..Default::default()
        })
        .build()
        .await?;

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: UserId::parse(&stored.user_id)?,
            device_id: OwnedDeviceId::from(stored.device_id.as_str()),
        },
        tokens: SessionTokens {
            access_token: stored.access_token,
            refresh_token: None,
        },
    };
    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await?;

    Ok(client)
}
