use std::path::Path;

use anyhow::bail;
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::json;

use crate::archive::Archive;

pub struct Messages {
    pub room_ids: Vec<String>,
    pub room_names: Vec<String>,
    pub event_ids: Vec<String>,
    pub after: Option<NaiveDateTime>,
    pub before: Option<NaiveDateTime>,
    pub sender: Vec<String>,
    pub msgtype: Vec<String>,
    pub event_type: Vec<String>,
    pub raw: bool,
    pub pinned: bool,
}

impl Messages {
    pub fn run(&self, archive_path: &Path) -> anyhow::Result<()> {
        let archive = Archive::open(archive_path)?;

        // Resolve room IDs
        let mut resolved_room_ids: Vec<String> = self.room_ids.clone();
        for name in &self.room_names {
            match archive.find_room_id(name)? {
                Some(id) => resolved_room_ids.push(id),
                None => bail!("Room not found: {name}"),
            }
        }

        // If event IDs specified, fetch those directly
        if !self.event_ids.is_empty() {
            for eid in &self.event_ids {
                if let Some(msg) = archive.get_event(eid)? {
                    self.print_event(&msg);
                }
            }
            return Ok(());
        }

        if resolved_room_ids.is_empty() {
            bail!("Specify --room-id, --room-name, or --event-id");
        }

        let after_ms = self.after.map(datetime_to_ms);
        let before_ms = self.before.map(datetime_to_ms);

        for room_id in &resolved_room_ids {
            let events = if self.pinned {
                archive.pinned_events(room_id)?
            } else {
                archive.query_events(
                    room_id,
                    after_ms,
                    before_ms,
                    &self.sender,
                    &self.msgtype,
                    &self.event_type,
                )?
            };

            for msg in &events {
                self.print_event(msg);
            }
        }

        Ok(())
    }

    fn print_event(&self, msg: &crate::archive::ArchivedMessage) {
        if self.raw {
            println!("{}", msg.raw_json);
        } else {
            let obj = json!({
                "event_id": msg.event_id,
                "sender": msg.sender,
                "timestamp": msg.timestamp,
                "type": msg.event_type,
                "msgtype": msg.msgtype,
                "body": msg.body,
            });
            println!("{}", serde_json::to_string(&obj).unwrap());
        }
    }
}

fn datetime_to_ms(dt: NaiveDateTime) -> i64 {
    Utc.from_utc_datetime(&dt).timestamp_millis()
}

pub fn parse_datetime(s: &str) -> anyhow::Result<NaiveDateTime> {
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(dt);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap());
    }
    bail!("Invalid date format: {s}. Use YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS")
}
