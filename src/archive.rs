use std::path::Path;

use anyhow::Context;
use rusqlite::{Connection, params};

pub struct Archive {
    conn: Connection,
}

impl Archive {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).context("Failed to open archive database")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                event_id       TEXT PRIMARY KEY,
                room_id        TEXT NOT NULL,
                room_name      TEXT,
                sender         TEXT NOT NULL,
                timestamp      TEXT NOT NULL,
                ts_millis      INTEGER NOT NULL,
                type           TEXT NOT NULL,
                msgtype        TEXT,
                body           TEXT,
                media_path     TEXT,
                thumbnail_path TEXT,
                raw_json       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_events_room_ts ON events (room_id, ts_millis);
            CREATE TABLE IF NOT EXISTS rooms (
                room_id            TEXT PRIMARY KEY,
                name               TEXT,
                topic              TEXT,
                avatar_url         TEXT,
                canonical_alias    TEXT,
                is_direct          INTEGER NOT NULL DEFAULT 0,
                is_encrypted       INTEGER NOT NULL DEFAULT 0,
                is_favourite       INTEGER NOT NULL DEFAULT 0,
                is_space           INTEGER NOT NULL DEFAULT 0,
                joined             INTEGER NOT NULL DEFAULT 1,
                joined_member_count INTEGER NOT NULL DEFAULT 0,
                room_type          TEXT,
                join_rule          TEXT,
                history_visibility TEXT,
                guest_access       TEXT,
                raw_json           TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pinned_events (
                room_id  TEXT NOT NULL,
                event_id TEXT NOT NULL,
                PRIMARY KEY (room_id, event_id)
            );
            CREATE TABLE IF NOT EXISTS members (
                room_id      TEXT NOT NULL,
                user_id      TEXT NOT NULL,
                display_name TEXT,
                avatar_url   TEXT,
                membership   TEXT NOT NULL,
                PRIMARY KEY (room_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS chat_cache (
                room_id         TEXT PRIMARY KEY,
                json            TEXT NOT NULL,
                materialized_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS archive_ranges (
                room_id  TEXT NOT NULL,
                start_ts INTEGER NOT NULL,
                end_ts   INTEGER NOT NULL,
                PRIMARY KEY (room_id, start_ts)
            );",
        )?;
        // Migrate existing DBs that lack media columns
        let _ = conn.execute_batch(
            "ALTER TABLE events ADD COLUMN media_path TEXT;
             ALTER TABLE events ADD COLUMN thumbnail_path TEXT;",
        );
        Ok(Self { conn })
    }

    /// Upsert a room's metadata.
    pub fn upsert_room(&self, room: &ArchivedRoom) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO rooms (room_id, name, topic, avatar_url, canonical_alias, is_direct, is_encrypted, is_favourite, is_space, joined, joined_member_count, room_type, join_rule, history_visibility, guest_access, raw_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                room.room_id,
                room.name,
                room.topic,
                room.avatar_url,
                room.canonical_alias,
                room.is_direct,
                room.is_encrypted,
                room.is_favourite,
                room.is_space,
                room.joined,
                room.joined_member_count,
                room.room_type,
                room.join_rule,
                room.history_visibility,
                room.guest_access,
                room.raw_json,
            ],
        )?;
        Ok(())
    }

    /// Replace all members for a room.
    pub fn set_members(&mut self, room_id: &str, members: &[RoomMemberRow]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM members WHERE room_id = ?1", params![room_id])?;
        for m in members {
            tx.execute(
                "INSERT INTO members (room_id, user_id, display_name, avatar_url, membership) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![room_id, m.user_id, m.display_name, m.avatar_url, m.membership],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Get all members grouped by user_id.
    pub fn all_members(&self) -> anyhow::Result<Vec<(String, Option<String>, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, display_name, room_id FROM members ORDER BY user_id, display_name",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }
        Ok(result)
    }

    /// Get cached chat JSON for a room.
    pub fn get_chat_cache(&self, room_id: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT json FROM chat_cache WHERE room_id = ?1")?;
        Ok(stmt.query_row(params![room_id], |row| row.get(0)).ok())
    }

    /// Store chat JSON cache for a room.
    pub fn set_chat_cache(&self, room_id: &str, json: &str) -> anyhow::Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            "INSERT OR REPLACE INTO chat_cache (room_id, json, materialized_at) VALUES (?1, ?2, ?3)",
            params![room_id, json, now],
        )?;
        Ok(())
    }

    /// Replace pinned events for a room.
    pub fn set_pinned_events(&mut self, room_id: &str, event_ids: &[String]) -> anyhow::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM pinned_events WHERE room_id = ?1",
            params![room_id],
        )?;
        for eid in event_ids {
            tx.execute(
                "INSERT OR IGNORE INTO pinned_events (room_id, event_id) VALUES (?1, ?2)",
                params![room_id, eid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Get pinned events for a room via join.
    pub fn pinned_events(&self, room_id: &str) -> anyhow::Result<Vec<ArchivedMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.event_id, e.room_id, e.room_name, e.sender, e.timestamp, e.ts_millis, e.type, e.msgtype, e.body, e.media_path, e.thumbnail_path, e.raw_json \
             FROM events e JOIN pinned_events p ON e.event_id = p.event_id \
             WHERE p.room_id = ?1 ORDER BY e.ts_millis ASC",
        )?;
        let rows = stmt.query_map(params![room_id], |row| {
            Ok(ArchivedMessage {
                event_id: row.get(0)?,
                room_id: row.get(1)?,
                room_name: row.get(2)?,
                sender: row.get(3)?,
                timestamp: row.get(4)?,
                ts_millis: row.get(5)?,
                event_type: row.get(6)?,
                msgtype: row.get(7)?,
                body: row.get(8)?,
                media_path: row.get(9)?,
                thumbnail_path: row.get(10)?,
                raw_json: row.get(11)?,
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Query rooms, optionally filtered.
    pub fn query_rooms(
        &self,
        favorites_only: bool,
        joined_only: bool,
    ) -> anyhow::Result<Vec<ArchivedRoom>> {
        let mut sql = String::from(
            "SELECT room_id, name, topic, avatar_url, canonical_alias, is_direct, is_encrypted, is_favourite, is_space, joined, joined_member_count, room_type, join_rule, history_visibility, guest_access, raw_json FROM rooms WHERE 1=1",
        );
        if favorites_only {
            sql.push_str(" AND is_favourite = 1");
        }
        if joined_only {
            sql.push_str(" AND joined = 1");
        }
        sql.push_str(" ORDER BY name");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(ArchivedRoom {
                room_id: row.get(0)?,
                name: row.get(1)?,
                topic: row.get(2)?,
                avatar_url: row.get(3)?,
                canonical_alias: row.get(4)?,
                is_direct: row.get(5)?,
                is_encrypted: row.get(6)?,
                is_favourite: row.get(7)?,
                is_space: row.get(8)?,
                joined: row.get(9)?,
                joined_member_count: row.get(10)?,
                room_type: row.get(11)?,
                join_rule: row.get(12)?,
                history_visibility: row.get(13)?,
                guest_access: row.get(14)?,
                raw_json: row.get(15)?,
            })
        })?;
        let mut rooms = Vec::new();
        for row in rows {
            rooms.push(row?);
        }
        Ok(rooms)
    }

    /// Transactionally insert a page of events and extend the given range.
    /// `range_start` is the start_ts of the range row to extend.
    /// On the first page of a gap, pass `None` to create a new range.
    /// Returns (new_count, updated_range_start_ts).
    pub fn insert_page(
        &mut self,
        events: &[ArchivedMessage],
        range_start: Option<i64>,
    ) -> anyhow::Result<(u64, Option<i64>)> {
        if events.is_empty() {
            return Ok((0, range_start));
        }

        let room_id = &events[0].room_id;
        let page_min = events.iter().map(|m| m.ts_millis).min().unwrap();
        let page_max = events.iter().map(|m| m.ts_millis).max().unwrap();

        let tx = self.conn.transaction()?;
        let mut count = 0u64;

        for msg in events {
            let result = tx.execute(
                "INSERT OR IGNORE INTO events (event_id, room_id, room_name, sender, timestamp, ts_millis, type, msgtype, body, media_path, thumbnail_path, raw_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    msg.event_id,
                    msg.room_id,
                    msg.room_name,
                    msg.sender,
                    msg.timestamp,
                    msg.ts_millis,
                    msg.event_type,
                    msg.msgtype,
                    msg.body,
                    msg.media_path,
                    msg.thumbnail_path,
                    msg.raw_json,
                ],
            )?;
            if result > 0 {
                count += 1;
            }
        }

        let new_start = match range_start {
            Some(existing) => {
                let old_end: i64 = tx.query_row(
                    "SELECT end_ts FROM archive_ranges WHERE room_id = ?1 AND start_ts = ?2",
                    params![room_id, existing],
                    |row| row.get(0),
                )?;
                let new_start = existing.min(page_min);
                let new_end = old_end.max(page_max);
                tx.execute(
                    "DELETE FROM archive_ranges WHERE room_id = ?1 AND start_ts = ?2",
                    params![room_id, existing],
                )?;
                tx.execute(
                    "INSERT INTO archive_ranges (room_id, start_ts, end_ts) VALUES (?1, ?2, ?3)",
                    params![room_id, new_start, new_end],
                )?;
                new_start
            }
            None => {
                tx.execute(
                    "INSERT INTO archive_ranges (room_id, start_ts, end_ts) VALUES (?1, ?2, ?3)",
                    params![room_id, page_min, page_max],
                )?;
                page_min
            }
        };

        tx.execute(
            "DELETE FROM chat_cache WHERE room_id = ?1",
            params![room_id],
        )?;

        tx.commit()?;
        Ok((count, Some(new_start)))
    }

    /// Get all archived ranges for a room, sorted by start_ts.
    pub fn ranges_for_room(&self, room_id: &str) -> anyhow::Result<Vec<(i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT start_ts, end_ts FROM archive_ranges WHERE room_id = ?1 ORDER BY start_ts",
        )?;
        let rows = stmt.query_map(params![room_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut ranges = Vec::new();
        for row in rows {
            ranges.push(row?);
        }
        Ok(ranges)
    }

    /// Query events for a room, optionally filtered by time range. Results ordered by ts_millis ascending.
    pub fn query_events(
        &self,
        room_id: &str,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        senders: &[String],
        msgtypes: &[String],
        event_types: &[String],
    ) -> anyhow::Result<Vec<ArchivedMessage>> {
        let mut sql = String::from(
            "SELECT event_id, room_id, room_name, sender, timestamp, ts_millis, type, msgtype, body, media_path, thumbnail_path, raw_json \
             FROM events WHERE room_id = ?1 AND ts_millis > ?2 AND ts_millis < ?3",
        );
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        param_values.push(Box::new(room_id.to_string()));
        param_values.push(Box::new(after_ms.unwrap_or(i64::MIN)));
        param_values.push(Box::new(before_ms.unwrap_or(i64::MAX)));

        let mut idx = 4;
        if !senders.is_empty() {
            let placeholders: Vec<String> = senders
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", idx + i))
                .collect();
            sql.push_str(&format!(" AND sender IN ({})", placeholders.join(",")));
            for s in senders {
                param_values.push(Box::new(s.clone()));
            }
            idx += senders.len();
        }
        if !msgtypes.is_empty() {
            let placeholders: Vec<String> = msgtypes
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", idx + i))
                .collect();
            sql.push_str(&format!(" AND msgtype IN ({})", placeholders.join(",")));
            for s in msgtypes {
                param_values.push(Box::new(s.clone()));
            }
            idx += msgtypes.len();
        }
        if !event_types.is_empty() {
            let placeholders: Vec<String> = event_types
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", idx + i))
                .collect();
            sql.push_str(&format!(" AND type IN ({})", placeholders.join(",")));
            for s in event_types {
                param_values.push(Box::new(s.clone()));
            }
            let _ = idx + event_types.len();
        }

        sql.push_str(" ORDER BY ts_millis ASC");

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();

        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(ArchivedMessage {
                event_id: row.get(0)?,
                room_id: row.get(1)?,
                room_name: row.get(2)?,
                sender: row.get(3)?,
                timestamp: row.get(4)?,
                ts_millis: row.get(5)?,
                event_type: row.get(6)?,
                msgtype: row.get(7)?,
                body: row.get(8)?,
                media_path: row.get(9)?,
                thumbnail_path: row.get(10)?,
                raw_json: row.get(11)?,
            })
        })?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Find a room_id by name or ID from archived events.
    pub fn find_room_id(&self, target: &str) -> anyhow::Result<Option<String>> {
        if target.starts_with('!') {
            let mut stmt = self
                .conn
                .prepare("SELECT room_id FROM events WHERE room_id = ?1 LIMIT 1")?;
            return Ok(stmt.query_row(params![target], |row| row.get(0)).ok());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT room_id FROM events WHERE room_name = ?1 LIMIT 1")?;
        Ok(stmt.query_row(params![target], |row| row.get(0)).ok())
    }

    /// Look up a single event by event_id.
    pub fn get_event(&self, event_id: &str) -> anyhow::Result<Option<ArchivedMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, room_id, room_name, sender, timestamp, ts_millis, type, msgtype, body, media_path, thumbnail_path, raw_json \
             FROM events WHERE event_id = ?1",
        )?;
        let result = stmt
            .query_row(params![event_id], |row| {
                Ok(ArchivedMessage {
                    event_id: row.get(0)?,
                    room_id: row.get(1)?,
                    room_name: row.get(2)?,
                    sender: row.get(3)?,
                    timestamp: row.get(4)?,
                    ts_millis: row.get(5)?,
                    event_type: row.get(6)?,
                    msgtype: row.get(7)?,
                    body: row.get(8)?,
                    media_path: row.get(9)?,
                    thumbnail_path: row.get(10)?,
                    raw_json: row.get(11)?,
                })
            })
            .ok();
        Ok(result)
    }

    /// Get all encrypted events (event_id, room_id, raw_json).
    pub fn encrypted_events(&self) -> anyhow::Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, room_id, raw_json FROM events WHERE type = 'm.room.encrypted'",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Update a decrypted event in the database.
    pub fn update_decrypted(
        &self,
        event_id: &str,
        event_type: &str,
        msgtype: Option<&str>,
        body: Option<&str>,
        raw_json: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE events SET type = ?1, msgtype = ?2, body = ?3, raw_json = ?4 WHERE event_id = ?5",
            params![event_type, msgtype, body, raw_json, event_id],
        )?;
        self.conn.execute(
            "DELETE FROM chat_cache WHERE room_id = (SELECT room_id FROM events WHERE event_id = ?1)",
            params![event_id],
        )?;
        Ok(())
    }

    /// Get events that should have media but don't have it downloaded yet.
    pub fn missing_media(
        &self,
        room_ids: &[String],
        after_ms: Option<i64>,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let placeholders: Vec<String> = room_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect();
        let cutoff_param = room_ids.len() + 1;
        let sql = format!(
            "SELECT event_id, raw_json FROM events \
             WHERE media_path IS NULL \
             AND msgtype IN ('m.image', 'm.video', 'm.audio', 'm.file') \
             AND room_id IN ({}) \
             AND ts_millis > ?{}",
            placeholders.join(","),
            cutoff_param,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = room_ids
            .iter()
            .map(|s| Box::new(s.clone()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        param_values.push(Box::new(after_ms.unwrap_or(i64::MIN)));
        let params: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Update media paths for an event.
    pub fn update_media_paths(
        &self,
        event_id: &str,
        media_path: Option<&str>,
        thumbnail_path: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE events SET media_path = ?1, thumbnail_path = ?2 WHERE event_id = ?3",
            params![media_path, thumbnail_path, event_id],
        )?;
        Ok(())
    }

    /// Get event_id for an event at a specific timestamp in a room.
    pub fn event_at_ts(&self, room_id: &str, ts_millis: i64) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT event_id FROM events WHERE room_id = ?1 AND ts_millis = ?2 LIMIT 1")?;
        let result: Option<String> = stmt
            .query_row(params![room_id, ts_millis], |row| row.get(0))
            .ok();
        Ok(result)
    }
}

pub struct RoomMemberRow {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub membership: String,
}

pub struct ArchivedRoom {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub is_direct: bool,
    pub is_encrypted: bool,
    pub is_favourite: bool,
    pub is_space: bool,
    pub joined: bool,
    pub joined_member_count: u64,
    pub room_type: Option<String>,
    pub join_rule: Option<String>,
    pub history_visibility: Option<String>,
    pub guest_access: Option<String>,
    pub raw_json: String,
}

pub struct ArchivedMessage {
    pub event_id: String,
    pub room_id: String,
    pub room_name: Option<String>,
    pub sender: String,
    pub timestamp: String,
    pub ts_millis: i64,
    pub event_type: String,
    pub msgtype: Option<String>,
    pub body: Option<String>,
    pub media_path: Option<String>,
    pub thumbnail_path: Option<String>,
    pub raw_json: String,
}
