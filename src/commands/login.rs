use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use rpassword::prompt_password;

use crate::session::{StoredSession, save_session};

pub struct Login;

impl Login {
    pub async fn run(&self, store_path: &Path) -> anyhow::Result<()> {
        let mut matrix_id = String::new();
        eprint!("Matrix ID (e.g. @user:matrix.org): ");
        std::io::stdin().read_line(&mut matrix_id)?;
        let matrix_id = matrix_id.trim();

        let (localpart, server) = parse_matrix_id(matrix_id)?;

        let password = prompt_password("Password: ")?;

        let client = Client::builder()
            .server_name_or_homeserver_url(server)
            .sqlite_store(store_path, None)
            .build()
            .await
            .context("Failed to connect to homeserver")?;

        client
            .matrix_auth()
            .login_username(localpart, &password)
            .initial_device_display_name("matrix-archiver")
            .send()
            .await
            .context("Login failed")?;

        let auth_session = client
            .matrix_auth()
            .session()
            .context("Failed to get session after login")?;

        let stored = StoredSession {
            homeserver_url: client.homeserver().to_string(),
            user_id: auth_session.meta.user_id.to_string(),
            device_id: auth_session.meta.device_id.to_string(),
            access_token: auth_session.tokens.access_token,
        };
        save_session(&client, &stored).await?;

        eprintln!("Logged in as {}", stored.user_id);
        let settings = SyncSettings::default().timeout(Duration::ZERO);
        client.sync_once(settings).await?;
        Ok(())
    }
}

fn parse_matrix_id(id: &str) -> anyhow::Result<(&str, &str)> {
    let id = id
        .strip_prefix('@')
        .context("Matrix ID must start with @")?;
    let (localpart, server) = id
        .split_once(':')
        .context("Matrix ID must be @localpart:server")?;
    Ok((localpart, server))
}
