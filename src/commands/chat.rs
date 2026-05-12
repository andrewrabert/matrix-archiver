use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::bail;
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::json;

use crate::archive::Archive;

pub struct Chat {
    pub room_id: Option<String>,
    pub room_name: Option<String>,
    pub after: Option<NaiveDateTime>,
    pub before: Option<NaiveDateTime>,
}

impl Chat {
    pub fn run(&self, archive_path: &Path) -> anyhow::Result<()> {
        let archive = Archive::open(archive_path)?;

        let room_id = if let Some(ref id) = self.room_id {
            archive
                .find_room_id(id)?
                .ok_or_else(|| anyhow::anyhow!("Room not found: {id}"))?
        } else if let Some(ref name) = self.room_name {
            archive
                .find_room_id(name)?
                .ok_or_else(|| anyhow::anyhow!("Room not found: {name}"))?
        } else {
            bail!("Specify --room-id or --room-name");
        };

        let after_ms = self.after.map(datetime_to_ms);
        let before_ms = self.before.map(datetime_to_ms);

        // Use cache if no time filters
        let use_cache = after_ms.is_none() && before_ms.is_none();
        if use_cache && let Some(cached) = archive.get_chat_cache(&room_id)? {
            println!("{cached}");
            return Ok(());
        }

        let all_events = archive.query_events(&room_id, after_ms, before_ms, &[], &[], &[])?;

        // Index events by ID for lookups
        let events_by_id: HashMap<&str, &crate::archive::ArchivedMessage> = all_events
            .iter()
            .map(|e| (e.event_id.as_str(), e))
            .collect();

        // Collect redacted event IDs
        let mut redacted: HashSet<String> = HashSet::new();
        for event in &all_events {
            if event.event_type == "m.room.redaction"
                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
            {
                if let Some(target) = raw.get("redacts").and_then(|v| v.as_str()) {
                    redacted.insert(target.to_string());
                }
                // Also check content.redacts (newer spec)
                if let Some(target) = raw
                    .get("content")
                    .and_then(|c| c.get("redacts"))
                    .and_then(|v| v.as_str())
                {
                    redacted.insert(target.to_string());
                }
            }
        }

        // Collect reactions: target_event_id -> emoji -> count
        let mut reactions: HashMap<String, BTreeMap<String, u64>> = HashMap::new();
        for event in &all_events {
            if event.event_type == "m.reaction"
                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
                && let Some(relates) = raw.get("content").and_then(|c| c.get("m.relates_to"))
                && let (Some(target), Some(key)) = (
                    relates.get("event_id").and_then(|v| v.as_str()),
                    relates.get("key").and_then(|v| v.as_str()),
                )
            {
                *reactions
                    .entry(target.to_string())
                    .or_default()
                    .entry(key.to_string())
                    .or_default() += 1;
            }
        }

        // Collect edits: original_event_id -> latest edit body
        let mut edits: HashMap<String, String> = HashMap::new();
        for event in &all_events {
            if event.event_type == "m.room.message"
                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
                && let Some(relates) = raw.get("content").and_then(|c| c.get("m.relates_to"))
                && relates.get("rel_type").and_then(|v| v.as_str()) == Some("m.replace")
                && let Some(target) = relates.get("event_id").and_then(|v| v.as_str())
                && let Some(new_body) = raw
                    .get("content")
                    .and_then(|c| c.get("m.new_content"))
                    .and_then(|nc| nc.get("body"))
                    .and_then(|b| b.as_str())
            {
                edits.insert(target.to_string(), new_body.to_string());
            }
        }

        // Collect thread messages: root_event_id -> Vec<event>
        let mut threads: HashMap<String, Vec<&crate::archive::ArchivedMessage>> = HashMap::new();
        let mut thread_event_ids: HashSet<String> = HashSet::new();
        for event in &all_events {
            if event.event_type == "m.room.message"
                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
                && let Some(relates) = raw.get("content").and_then(|c| c.get("m.relates_to"))
                && relates.get("rel_type").and_then(|v| v.as_str()) == Some("m.thread")
                && let Some(root) = relates.get("event_id").and_then(|v| v.as_str())
            {
                threads.entry(root.to_string()).or_default().push(event);
                thread_event_ids.insert(event.event_id.clone());
            }
        }

        // Collect edit event IDs so we skip them in the main timeline
        let mut edit_event_ids: HashSet<String> = HashSet::new();
        for event in &all_events {
            if event.event_type == "m.room.message"
                && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
                && let Some(relates) = raw.get("content").and_then(|c| c.get("m.relates_to"))
                && relates.get("rel_type").and_then(|v| v.as_str()) == Some("m.replace")
            {
                edit_event_ids.insert(event.event_id.clone());
            }
        }

        // Build output
        let mut output: Vec<serde_json::Value> = Vec::new();

        for event in &all_events {
            // Only show m.room.message in the timeline
            if event.event_type != "m.room.message" {
                continue;
            }
            // Skip edits and thread messages
            if edit_event_ids.contains(&event.event_id)
                || thread_event_ids.contains(&event.event_id)
            {
                continue;
            }

            if redacted.contains(&event.event_id) {
                output.push(json!({
                    "ts": event.timestamp,
                    "sender": event.sender,
                    "deleted": true,
                }));
                continue;
            }

            let body = edits
                .get(&event.event_id)
                .cloned()
                .or_else(|| event.body.clone());
            let edited = edits.contains_key(&event.event_id);

            let mut msg = json!({
                "ts": event.timestamp,
                "sender": event.sender,
            });

            if let Some(body) = &body {
                msg["body"] = json!(body);
            }
            if edited {
                msg["edited"] = json!(true);
            }

            // Media
            if let Some(ref media) = event.media_path {
                msg["media"] = json!(media);
            }

            // Reactions
            if let Some(rxns) = reactions.get(&event.event_id) {
                msg["reactions"] = json!(rxns);
            }

            // Reply
            if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&event.raw_json)
                && let Some(reply_id) = raw
                    .get("content")
                    .and_then(|c| c.get("m.relates_to"))
                    .and_then(|r| r.get("m.in_reply_to"))
                    .and_then(|r| r.get("event_id"))
                    .and_then(|v| v.as_str())
            {
                if let Some(parent) = events_by_id.get(reply_id) {
                    let parent_body = edits.get(reply_id).cloned().or_else(|| parent.body.clone());
                    msg["reply_to"] = json!({
                        "sender": parent.sender,
                        "body": parent_body,
                    });
                } else {
                    msg["reply_to"] = json!(reply_id);
                }
            }

            // Thread
            if let Some(thread_msgs) = threads.get(event.event_id.as_str()) {
                let thread: Vec<serde_json::Value> = thread_msgs
                    .iter()
                    .map(|te| {
                        if redacted.contains(&te.event_id) {
                            return json!({
                                "ts": te.timestamp,
                                "sender": te.sender,
                                "deleted": true,
                            });
                        }
                        let tbody = edits.get(&te.event_id).cloned().or_else(|| te.body.clone());
                        let tedited = edits.contains_key(&te.event_id);
                        let mut tmsg = json!({
                            "ts": te.timestamp,
                            "sender": te.sender,
                        });
                        if let Some(tbody) = &tbody {
                            tmsg["body"] = json!(tbody);
                        }
                        if tedited {
                            tmsg["edited"] = json!(true);
                        }
                        if let Some(rxns) = reactions.get(&te.event_id) {
                            tmsg["reactions"] = json!(rxns);
                        }
                        if let Some(ref media) = te.media_path {
                            tmsg["media"] = json!(media);
                        }
                        tmsg
                    })
                    .collect();
                msg["thread"] = json!(thread);
            }

            output.push(msg);
        }

        let json = serde_json::to_string_pretty(&output)?;

        if use_cache {
            let _ = archive.set_chat_cache(&room_id, &json);
        }

        println!("{json}");
        Ok(())
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
