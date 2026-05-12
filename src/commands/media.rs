use std::io::Write;
use std::path::Path;

use anyhow::Context;

use crate::archive::Archive;

pub struct Media {
    pub event_id: String,
    pub thumbnail: bool,
}

impl Media {
    pub fn run(&self, archive_path: &Path) -> anyhow::Result<()> {
        let archive = Archive::open(archive_path)?;

        let msg = archive
            .get_event(&self.event_id)?
            .context(format!("Event not found: {}", self.event_id))?;

        let path = if self.thumbnail {
            msg.thumbnail_path
                .as_ref()
                .context("No thumbnail for this event")?
        } else {
            msg.media_path.as_ref().context("No media for this event")?
        };

        let bytes = std::fs::read(path).context(format!("Failed to read file: {path}"))?;
        std::io::stdout().write_all(&bytes)?;

        Ok(())
    }
}
