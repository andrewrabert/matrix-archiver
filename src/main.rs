mod archive;
mod commands;
mod session;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::login::Login;
use crate::commands::media::Media;
use crate::commands::messages::{self, Messages};
use crate::commands::rooms::Rooms;
use crate::commands::sync::SyncCmd;
use crate::commands::verify::Verify;
use crate::session::restore_client;

#[derive(Parser)]
#[command(name = "matrix-archiver", version = env!("MATRIX_ARCHIVER_VERSION"))]
struct Cli {
    /// Path to the data directory
    #[arg(long, env = "MATRIX_ARCHIVER_DATA")]
    data: PathBuf,

    /// Skip sync, use cached data only
    #[arg(long)]
    offline: bool,

    /// Enable verbose logging (shows HTTP requests)
    #[arg(long)]
    verbose: bool,

    /// Max retry attempts for HTTP requests (default 3)
    #[arg(long, default_value = "3")]
    retries: usize,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in to a Matrix homeserver
    Login,
    /// List users and their display names across rooms
    Users,
    /// Render room messages as a structured chat timeline
    Messages {
        /// Room ID
        #[arg(long)]
        room_id: Option<String>,
        /// Room name
        #[arg(long)]
        room_name: Option<String>,
        /// After this date, exclusive (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
        #[arg(long)]
        after: Option<String>,
        /// Before this date, exclusive (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
        #[arg(long)]
        before: Option<String>,
    },
    /// List rooms from the archive
    Rooms {
        /// Only show favorite rooms
        #[arg(long)]
        favorites: bool,
        /// Only show joined rooms
        #[arg(long)]
        joined: bool,
        /// Output raw JSON
        #[arg(long)]
        raw: bool,
    },
    /// Decrypt previously encrypted events in the archive
    Decrypt,
    /// Import E2E keys using recovery key
    Verify,
    /// Sync messages to a local SQLite database
    Sync {
        /// Messages per request (default 100, max depends on server)
        #[arg(long, default_value = "100")]
        batch: u32,
        /// How far back to backfill in days (default 30, 0 for unlimited)
        #[arg(long, default_value = "30")]
        backfill_days: u32,
        /// How far back to backfill media in days (default: same as backfill-days, 0 for unlimited)
        #[arg(long)]
        media_backfill_days: Option<u32>,
        /// Sync all joined rooms
        #[arg(long)]
        all: bool,
        /// Sync only favorite rooms
        #[arg(long)]
        favorites: bool,
        /// Only sync room metadata, members, and pinned events (skip event pagination)
        #[arg(long)]
        metadata: bool,
        /// Exclude a room by ID, repeatable
        #[arg(long)]
        exclude_room_id: Vec<String>,
        /// Sync a room by ID, repeatable
        #[arg(long)]
        room_id: Vec<String>,
        /// Sync a room by name, repeatable
        #[arg(long)]
        room_name: Vec<String>,
    },
    /// Get media content for an event
    Media {
        /// Event ID (e.g. "$abc123:matrix.org")
        event_id: String,
        /// Output thumbnail instead of full media
        #[arg(long)]
        thumbnail: bool,
    },
    /// Get events from a room
    Events {
        /// Room ID (e.g. "!abc:matrix.org"), repeatable
        #[arg(long)]
        room_id: Vec<String>,
        /// Room name (e.g. "General"), repeatable
        #[arg(long)]
        room_name: Vec<String>,
        /// Event ID (e.g. "$abc:matrix.org"), repeatable
        #[arg(long)]
        event_id: Vec<String>,
        /// After this date, exclusive (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
        #[arg(long)]
        after: Option<String>,
        /// Before this date, exclusive (YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS)
        #[arg(long)]
        before: Option<String>,
        /// Filter by sender (e.g. "@user:matrix.org"), repeatable
        #[arg(long)]
        sender: Vec<String>,
        /// Filter by message type (e.g. "m.image", "m.text"), repeatable
        #[arg(long)]
        msgtype: Vec<String>,
        /// Filter by event type (e.g. "m.room.message"), repeatable
        #[arg(long, id = "type")]
        r#type: Vec<String>,
        /// Output raw event JSON
        #[arg(long)]
        raw: bool,
        /// Show only pinned events
        #[arg(long)]
        pinned: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt()
            .with_env_filter("matrix_sdk::http_client=debug")
            .with_target(false)
            .with_writer(std::io::stderr)
            .init();
    }
    match cli.command {
        Commands::Login => Login.run(&cli.data).await,
        Commands::Users => {
            let db = cli.data.join("archive.db");
            commands::users::run(&db)
        }
        Commands::Decrypt => {
            let client = restore_client(&cli.data, cli.retries).await?;
            let db = cli.data.join("archive.db");
            commands::decrypt::run(&client, &db).await
        }
        Commands::Verify => Verify.run(&cli.data).await,
        Commands::Sync {
            batch,
            backfill_days,
            media_backfill_days,
            all,
            favorites,
            metadata,
            exclude_room_id,
            room_id,
            room_name,
        } => {
            let client = restore_client(&cli.data, cli.retries).await?;
            let db = cli.data.join("archive.db");
            let media_dir = cli.data.join("media");
            SyncCmd {
                room_ids: room_id,
                room_names: room_name,
                batch,
                backfill_days,
                media_backfill_days,
                all,
                favorites,
                metadata,
                exclude_room_ids: exclude_room_id,
            }
            .run(&client, &db, &media_dir, cli.offline)
            .await
        }
        Commands::Rooms {
            favorites,
            joined,
            raw,
        } => {
            let db = cli.data.join("archive.db");
            Rooms {
                favorites,
                joined,
                raw,
            }
            .run(&db)
        }
        Commands::Media {
            event_id,
            thumbnail,
        } => {
            let db = cli.data.join("archive.db");
            Media {
                event_id,
                thumbnail,
            }
            .run(&db)
        }
        Commands::Events {
            room_id,
            room_name,
            event_id,
            after,
            before,
            sender,
            msgtype,
            r#type,
            raw,
            pinned,
        } => {
            let after = after.map(|s| messages::parse_datetime(&s)).transpose()?;
            let before = before.map(|s| messages::parse_datetime(&s)).transpose()?;
            let db = cli.data.join("archive.db");
            Messages {
                room_ids: room_id,
                room_names: room_name,
                event_ids: event_id,
                after,
                before,
                sender,
                msgtype,
                event_type: r#type,
                raw,
                pinned,
            }
            .run(&db)
        }
        Commands::Messages {
            room_id,
            room_name,
            after,
            before,
        } => {
            let after = after
                .map(|s| commands::chat::parse_datetime(&s))
                .transpose()?;
            let before = before
                .map(|s| commands::chat::parse_datetime(&s))
                .transpose()?;
            let db = cli.data.join("archive.db");
            commands::chat::Chat {
                room_id,
                room_name,
                after,
                before,
            }
            .run(&db)
        }
    }
}
