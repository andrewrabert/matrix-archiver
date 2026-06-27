use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::bail;
use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
use clap::ValueEnum;
use regex::{Regex, RegexBuilder};
use serde_json::json;

use crate::archive::{Archive, ArchivedMessage};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Order {
    Newest,
    Oldest,
}

const ALL_ROOMS_PATTERN_CAP: usize = 100;

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
    pub patterns: Vec<String>,
    pub not_patterns: Vec<String>,
    pub case_sensitive: bool,
    pub favorites: bool,
    pub order: Order,
    pub max_count: Option<usize>,
    pub before_context: usize,
    pub after_context: usize,
}

impl Messages {
    pub fn run(&self, archive_path: &Path) -> anyhow::Result<()> {
        let archive = Archive::open(archive_path)?;

        let include = compile_patterns(&self.patterns, self.case_sensitive)?;
        let exclude = compile_patterns(&self.not_patterns, self.case_sensitive)?;
        let context_on = self.before_context > 0 || self.after_context > 0;

        if !self.event_ids.is_empty() {
            let mut anchors = Vec::new();
            for eid in &self.event_ids {
                if let Some(msg) = archive.get_event(eid)? {
                    anchors.push(msg);
                }
            }
            if context_on {
                self.render_with_context(&archive, anchors)?;
            } else {
                for m in &anchors {
                    self.print_event(m, false, false);
                }
            }
            return Ok(());
        }

        // Empty `resolved` means "no match" when scoped, but "all rooms" otherwise.
        let scoped =
            !self.room_ids.is_empty() || !self.room_names.is_empty() || self.favorites;
        let mut resolved: Vec<String> = self.room_ids.clone();
        for name in &self.room_names {
            match archive.find_room_id(name)? {
                Some(id) => resolved.push(id),
                None => bail!("Room not found: {name}"),
            }
        }
        if self.favorites {
            let fav: Vec<String> = archive
                .query_rooms(true, false)?
                .into_iter()
                .map(|r| r.room_id)
                .collect();
            if resolved.is_empty() {
                resolved = fav;
            } else {
                resolved.retain(|id| fav.contains(id));
            }
        }
        if scoped && resolved.is_empty() {
            return Ok(());
        }

        let after_ms = self.after.map(datetime_to_ms);
        let before_ms = self.before.map(datetime_to_ms);

        let candidates = if self.pinned {
            if resolved.is_empty() {
                bail!("--pinned requires a room");
            }
            let mut v = Vec::new();
            for rid in &resolved {
                v.extend(archive.pinned_events(rid)?);
            }
            v
        } else {
            archive.query_events_multi(
                &resolved,
                after_ms,
                before_ms,
                &self.sender,
                &self.msgtype,
                &self.event_type,
            )?
        };

        let all_rooms = !scoped;
        let max_count = self.max_count.or(if !include.is_empty() && all_rooms {
            Some(ALL_ROOMS_PATTERN_CAP)
        } else {
            None
        });

        let hits = select(candidates, &include, &exclude, self.order, max_count);

        if context_on {
            self.render_with_context(&archive, hits)?;
        } else {
            for m in &hits {
                self.print_event(m, false, false);
            }
        }

        Ok(())
    }

    fn render_with_context(
        &self,
        archive: &Archive,
        hits: Vec<ArchivedMessage>,
    ) -> anyhow::Result<()> {
        let hit_ids: HashSet<String> = hits.iter().map(|m| m.event_id.clone()).collect();
        // event_id in the key: ts alone would drop distinct same-instant events.
        let mut merged: BTreeMap<(i64, String), ArchivedMessage> = BTreeMap::new();
        for h in &hits {
            let ctx = archive.events_around(
                &h.room_id,
                h.ts_millis,
                self.before_context,
                self.after_context,
            )?;
            for c in ctx {
                merged.entry((c.ts_millis, c.event_id.clone())).or_insert(c);
            }
        }
        for h in hits {
            merged.entry((h.ts_millis, h.event_id.clone())).or_insert(h);
        }
        for ((_, eid), m) in &merged {
            self.print_event(m, true, hit_ids.contains(eid));
        }
        Ok(())
    }

    fn print_event(&self, msg: &ArchivedMessage, context_active: bool, is_hit: bool) {
        if self.raw {
            println!("{}", msg.raw_json);
            return;
        }
        let mut obj = json!({
            "event_id": msg.event_id,
            "room_id": msg.room_id,
            "room_name": msg.room_name,
            "sender": msg.sender,
            "timestamp": msg.timestamp,
            "type": msg.event_type,
            "msgtype": msg.msgtype,
            "body": msg.body,
        });
        if context_active && is_hit {
            obj["hit"] = json!(true);
        }
        println!("{}", serde_json::to_string(&obj).unwrap());
    }
}

fn compile_patterns(patterns: &[String], case_sensitive: bool) -> anyhow::Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| {
            RegexBuilder::new(p)
                .case_insensitive(!case_sensitive)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid pattern {p:?}: {e}"))
        })
        .collect()
}

fn select(
    mut msgs: Vec<ArchivedMessage>,
    include: &[Regex],
    exclude: &[Regex],
    order: Order,
    max_count: Option<usize>,
) -> Vec<ArchivedMessage> {
    msgs.retain(|m| {
        let body = m.body.as_deref().unwrap_or("");
        let included = include.is_empty() || include.iter().any(|r| r.is_match(body));
        included && !exclude.iter().any(|r| r.is_match(body))
    });
    msgs.sort_by_key(|m| m.ts_millis);
    if let Some(n) = max_count
        && msgs.len() > n
    {
        match order {
            Order::Newest => {
                msgs.drain(0..msgs.len() - n);
            }
            Order::Oldest => msgs.truncate(n),
        }
    }
    msgs
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, ts: i64, body: &str) -> ArchivedMessage {
        ArchivedMessage {
            event_id: id.into(),
            room_id: "!r".into(),
            room_name: None,
            sender: "@u".into(),
            timestamp: String::new(),
            ts_millis: ts,
            event_type: "m.room.message".into(),
            msgtype: Some("m.text".into()),
            body: Some(body.into()),
            media_path: None,
            thumbnail_path: None,
            raw_json: "{}".into(),
        }
    }

    fn re(p: &str) -> Vec<Regex> {
        compile_patterns(&[p.to_string()], false).unwrap()
    }

    fn ids(v: &[ArchivedMessage]) -> Vec<&str> {
        v.iter().map(|m| m.event_id.as_str()).collect()
    }

    fn sample() -> Vec<ArchivedMessage> {
        vec![
            msg("a", 1, "deploy started"),
            msg("b", 2, "deploy failed"),
            msg("c", 3, "all green"),
            msg("d", 4, "ploybackground noise"),
            msg("e", 5, "foo and bar"),
        ]
    }

    #[test]
    fn include_matches_any() {
        let out = select(sample(), &re("deploy"), &[], Order::Oldest, None);
        assert_eq!(ids(&out), ["a", "b"]);
    }

    #[test]
    fn multi_pattern_ors() {
        let inc = compile_patterns(&["green".into(), "foo".into()], false).unwrap();
        let out = select(sample(), &inc, &[], Order::Oldest, None);
        assert_eq!(ids(&out), ["c", "e"]);
    }

    #[test]
    fn not_pattern_subtracts() {
        let out = select(sample(), &re("deploy"), &re("failed"), Order::Oldest, None);
        assert_eq!(ids(&out), ["a"]);
    }

    #[test]
    fn no_include_returns_all_minus_excluded() {
        let out = select(sample(), &[], &re("noise"), Order::Oldest, None);
        assert_eq!(ids(&out), ["a", "b", "c", "e"]);
    }

    #[test]
    fn regex_metachars() {
        let out = select(sample(), &re("a.*b"), &[], Order::Oldest, None);
        assert_eq!(ids(&out), ["e"]);
    }

    #[test]
    fn midword_substring() {
        let out = select(sample(), &re("ploy"), &[], Order::Oldest, None);
        assert_eq!(ids(&out), ["a", "b", "d"]);
    }

    #[test]
    fn max_count_keeps_newest_end() {
        let out = select(sample(), &[], &[], Order::Newest, Some(2));
        assert_eq!(ids(&out), ["d", "e"]);
    }

    #[test]
    fn max_count_keeps_oldest_end() {
        let out = select(sample(), &[], &[], Order::Oldest, Some(2));
        assert_eq!(ids(&out), ["a", "b"]);
    }

    #[test]
    fn case_insensitive_default_vs_sensitive() {
        let insensitive = compile_patterns(&["DEPLOY".into()], false).unwrap();
        assert_eq!(ids(&select(sample(), &insensitive, &[], Order::Oldest, None)), ["a", "b"]);
        let sensitive = compile_patterns(&["DEPLOY".into()], true).unwrap();
        assert!(select(sample(), &sensitive, &[], Order::Oldest, None).is_empty());
    }
}
