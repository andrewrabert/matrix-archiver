use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use matrix_sdk::Client;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{EventId, uint};

use matrix_sdk::RoomMemberships;

use crate::archive::{Archive, ArchivedMessage, ArchivedRoom, RoomMemberRow};

pub struct SyncCmd {
    pub room_ids: Vec<String>,
    pub room_names: Vec<String>,
    pub batch: u32,
    pub backfill_days: u32,
    pub media_backfill_days: Option<u32>,
    pub all: bool,
    pub favorites: bool,
    pub metadata: bool,
    pub exclude_room_ids: Vec<String>,
}

enum GapKind {
    /// No archive data — paginate backward from now
    Fresh,
    /// Gap after the last range — paginate forward from end_ts
    AfterLast { end_ts: i64 },
    /// Gap before the first range — paginate backward from start_ts
    BeforeFirst { start_ts: i64 },
    /// Gap between two ranges — paginate forward from earlier end_ts, stop at later start_ts
    Between { end_ts: i64, next_start_ts: i64 },
}

struct RoomCtx<'a> {
    room: &'a matrix_sdk::Room,
    room_id: &'a str,
    room_name: Option<&'a str>,
    archive: &'a mut Archive,
    client: &'a Client,
    media_dir: &'a Path,
    media_cutoff: Option<i64>,
    downloaded_keys: &'a mut HashSet<String>,
}

impl SyncCmd {
    pub async fn run(
        &self,
        client: &Client,
        archive_path: &Path,
        media_dir: &Path,
        offline: bool,
    ) -> anyhow::Result<()> {
        if !offline {
            eprintln!("Syncing room state...");
            let settings = SyncSettings::default().timeout(Duration::ZERO);
            client.sync_once(settings).await?;
        }

        let mut archive = Archive::open(archive_path)?;
        std::fs::create_dir_all(media_dir)?;

        // Always sync room metadata for all joined rooms
        let all_rooms = client.joined_rooms();
        for room in &all_rooms {
            let room_id = room.room_id().to_string();
            let room_name = room.display_name().await.ok().map(|n| n.to_string());
            let is_joined = room.state() == matrix_sdk::RoomState::Joined;
            let archived_room =
                build_archived_room(room, &room_id, room_name.as_deref(), is_joined).await;
            archive.upsert_room(&archived_room)?;
        }
        eprintln!("Synced {} rooms", all_rooms.len());

        // Determine selected rooms for members/pinned/events
        let has_specific = !self.room_ids.is_empty() || !self.room_names.is_empty();
        let selected: Vec<matrix_sdk::Room> = if has_specific {
            let mut found = Vec::new();
            for target in &self.room_ids {
                found.push(find_room(client, target).await?);
            }
            for target in &self.room_names {
                found.push(find_room(client, target).await?);
            }
            found
        } else if self.favorites {
            client
                .joined_rooms()
                .into_iter()
                .filter(|r| r.is_favourite())
                .collect()
        } else if self.all {
            client.joined_rooms()
        } else {
            return Ok(());
        };

        for room in &selected {
            let room_id = room.room_id().to_string();
            let room_name = room.display_name().await.ok().map(|n| n.to_string());
            let display = room_name.as_deref().unwrap_or(&room_id);

            if self.exclude_room_ids.iter().any(|e| e == &room_id) {
                continue;
            }
            eprintln!("Syncing: {display}");

            match room.pinned_event_ids() {
                Some(pinned) => {
                    let pinned_strs: Vec<String> = pinned.iter().map(|id| id.to_string()).collect();
                    archive.set_pinned_events(&room_id, &pinned_strs)?;
                    eprintln!("  {} pinned events", pinned_strs.len());
                }
                None => {
                    eprintln!("  no pinned events");
                }
            }

            if let Ok(members) = room.members(RoomMemberships::all()).await {
                let rows: Vec<RoomMemberRow> = members
                    .iter()
                    .map(|m| RoomMemberRow {
                        user_id: m.user_id().to_string(),
                        display_name: m.display_name().map(String::from),
                        avatar_url: m.avatar_url().map(|u| u.to_string()),
                        membership: format!("{:?}", m.membership()),
                    })
                    .collect();
                archive.set_members(&room_id, &rows)?;
                eprintln!("  {} members", rows.len());
            }

            if self.metadata {
                continue;
            }

            let media_days = self.media_backfill_days.unwrap_or(self.backfill_days);
            let media_cutoff = if media_days > 0 {
                let cutoff = chrono::Utc::now() - chrono::Duration::days(media_days as i64);
                Some(cutoff.timestamp_millis())
            } else {
                None
            };
            let mut downloaded_keys: HashSet<String> = HashSet::new();

            let ranges = archive.ranges_for_room(&room_id)?;
            let gaps = compute_gaps(&ranges);

            if gaps.is_empty() {
                eprintln!("  up to date");
                continue;
            }

            let mut total = 0u64;

            for gap in &gaps {
                let mut ctx = RoomCtx {
                    room,
                    room_id: &room_id,
                    room_name: room_name.as_deref(),
                    archive: &mut archive,
                    client,
                    media_dir,
                    media_cutoff,
                    downloaded_keys: &mut downloaded_keys,
                };
                let count = self.fill_gap(gap, &mut ctx).await?;

                if count > 0 {
                    eprintln!();
                }

                total += count;
            }

            if total == 0 {
                eprintln!("  up to date");
            }
        }

        if self.metadata {
            return Ok(());
        }

        // Download missing media for synced rooms
        let media_days = self.media_backfill_days.unwrap_or(self.backfill_days);
        let media_cutoff = if media_days > 0 {
            let cutoff = chrono::Utc::now() - chrono::Duration::days(media_days as i64);
            Some(cutoff.timestamp_millis())
        } else {
            None
        };
        let room_ids: Vec<String> = selected.iter().map(|r| r.room_id().to_string()).collect();
        let missing = archive.missing_media(&room_ids, media_cutoff)?;
        if !missing.is_empty() {
            eprintln!("Downloading missing media: {} events", missing.len());
            for (i, (event_id, raw_json)) in missing.iter().enumerate() {
                eprint!("\r  {}/{} downloading {event_id}...", i + 1, missing.len());
                let _ = std::io::stderr().flush();

                let parsed: Result<serde_json::Value, _> = serde_json::from_str(raw_json);
                let (media_path, thumbnail_path) = if let Ok(val) = parsed {
                    if let Some(content) = val.get("content") {
                        let content_str = content.to_string();
                        if let Ok(msgtype) = serde_json::from_str::<MessageType>(&content_str) {
                            download_event_media(client, &msgtype, media_dir).await
                        } else {
                            (None, None)
                        }
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
                };

                if media_path.is_some() {
                    archive.update_media_paths(
                        event_id,
                        media_path.as_deref(),
                        thumbnail_path.as_deref(),
                    )?;
                }
            }
            eprintln!();
        }

        Ok(())
    }

    async fn fill_gap(&self, gap: &GapKind, ctx: &mut RoomCtx<'_>) -> anyhow::Result<u64> {
        let backfill_cutoff = if self.backfill_days > 0 {
            let cutoff = chrono::Utc::now() - chrono::Duration::days(self.backfill_days as i64);
            Some(cutoff.timestamp_millis())
        } else {
            None
        };

        match gap {
            GapKind::Fresh => {
                self.paginate_backward(ctx, None, backfill_cutoff, "full")
                    .await
            }
            GapKind::AfterLast { end_ts } => {
                let event_id = ctx
                    .archive
                    .event_at_ts(ctx.room_id, *end_ts)?
                    .context("No event found at range boundary")?;
                let eid = EventId::parse(&event_id).context("Invalid event ID")?;
                let ectx = ctx
                    .room
                    .event_with_context(&eid, false, uint!(0), None)
                    .await?;
                if let Some(token) = ectx.next_batch_token {
                    self.paginate_forward(ctx, &token, None, "new").await
                } else {
                    Ok(0)
                }
            }
            GapKind::BeforeFirst { start_ts } => {
                if backfill_cutoff.is_some_and(|cutoff| *start_ts <= cutoff) {
                    return Ok(0);
                }
                let event_id = ctx
                    .archive
                    .event_at_ts(ctx.room_id, *start_ts)?
                    .context("No event found at range boundary")?;
                let eid = EventId::parse(&event_id).context("Invalid event ID")?;
                let ectx = ctx
                    .room
                    .event_with_context(&eid, false, uint!(0), None)
                    .await?;
                if let Some(token) = ectx.prev_batch_token {
                    self.paginate_backward(ctx, Some(&token), backfill_cutoff, "backfill")
                        .await
                } else {
                    Ok(0)
                }
            }
            GapKind::Between {
                end_ts,
                next_start_ts,
            } => {
                let event_id = ctx
                    .archive
                    .event_at_ts(ctx.room_id, *end_ts)?
                    .context("No event found at range boundary")?;
                let eid = EventId::parse(&event_id).context("Invalid event ID")?;
                let ectx = ctx
                    .room
                    .event_with_context(&eid, false, uint!(0), None)
                    .await?;
                if let Some(token) = ectx.next_batch_token {
                    self.paginate_forward(ctx, &token, Some(*next_start_ts), "gap")
                        .await
                } else {
                    Ok(0)
                }
            }
        }
    }

    async fn paginate_backward(
        &self,
        ctx: &mut RoomCtx<'_>,
        from_token: Option<&str>,
        cutoff_ts: Option<i64>,
        label: &str,
    ) -> anyhow::Result<u64> {
        let mut count = 0u64;
        let mut range_start: Option<i64> = None;
        let mut options = MessagesOptions::backward();
        options.limit = self.batch.into();
        if let Some(token) = from_token {
            options = options.from(token);
        }

        loop {
            let response = ctx.room.messages(options).await?;
            let mut page = Vec::new();
            for event in &response.chunk {
                if let Some(msg) = process_event(event, ctx).await {
                    count += 1;
                    let ts_display = chrono::DateTime::from_timestamp_millis(msg.ts_millis)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    eprint!("\r  {label}: {count} messages ({ts_display})");
                    let _ = std::io::stderr().flush();
                    page.push(msg);
                }
            }

            let hit_cutoff =
                cutoff_ts.is_some_and(|cutoff| page.iter().any(|m| m.ts_millis < cutoff));

            let (_page_count, new_range_start) = ctx.archive.insert_page(&page, range_start)?;
            range_start = new_range_start;

            if hit_cutoff {
                break;
            }

            match response.end {
                Some(token) => {
                    options = MessagesOptions::backward();
                    options.limit = self.batch.into();
                    options = options.from(token.as_str());
                }
                None => break,
            }
        }

        Ok(count)
    }

    async fn paginate_forward(
        &self,
        ctx: &mut RoomCtx<'_>,
        from_token: &str,
        stop_ts: Option<i64>,
        label: &str,
    ) -> anyhow::Result<u64> {
        let mut count = 0u64;
        let mut range_start: Option<i64> = None;
        let mut options = MessagesOptions::forward();
        options.limit = self.batch.into();
        options = options.from(from_token);

        loop {
            let response = ctx.room.messages(options).await?;

            let mut page = Vec::new();
            let mut hit_boundary = false;

            for event in &response.chunk {
                if let Some(stop) = stop_ts
                    && let Some(ts) = event.timestamp()
                {
                    let ts_millis: i64 = u64::from(ts.0) as i64;
                    if ts_millis >= stop {
                        hit_boundary = true;
                        break;
                    }
                }

                if let Some(msg) = process_event(event, ctx).await {
                    count += 1;
                    let ts_display = chrono::DateTime::from_timestamp_millis(msg.ts_millis)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default();
                    eprint!("\r  {label}: {count} messages ({ts_display})");
                    let _ = std::io::stderr().flush();
                    page.push(msg);
                }
            }

            let (_page_count, new_range_start) = ctx.archive.insert_page(&page, range_start)?;
            range_start = new_range_start;

            if hit_boundary {
                break;
            }

            match response.end {
                Some(token) => {
                    options = MessagesOptions::forward();
                    options.limit = self.batch.into();
                    options = options.from(token.as_str());
                }
                None => break,
            }
        }

        Ok(count)
    }
}

async fn process_event(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
    ctx: &mut RoomCtx<'_>,
) -> Option<ArchivedMessage> {
    let ts = event.timestamp()?;
    let ts_millis: i64 = u64::from(ts.0) as i64;

    let mut deserialized = event.raw().deserialize().ok()?;

    // If encrypted, try downloading keys and re-decrypting
    if deserialized.event_type().to_string() == "m.room.encrypted"
        && !ctx.downloaded_keys.contains(ctx.room_id)
    {
        ctx.downloaded_keys.insert(ctx.room_id.to_string());
        let _ = ctx
            .client
            .encryption()
            .backups()
            .download_room_keys_for_room(ctx.room.room_id())
            .await;

        // Re-decrypt using room.decrypt_event
        if let Ok(raw_encrypted) = serde_json::from_str(event.raw().json().get())
            && let Ok(redecrypted) = ctx.room.decrypt_event(&raw_encrypted, None).await
            && let Ok(d) = redecrypted.raw().deserialize()
        {
            deserialized = d;
        }
    }

    build_archived(
        ctx.client,
        &deserialized,
        ctx.room_id,
        ctx.room_name,
        ts_millis,
        event.raw(),
        ctx.media_dir,
        ctx.media_cutoff,
    )
    .await
}

async fn build_archived_room(
    room: &matrix_sdk::Room,
    room_id: &str,
    name: Option<&str>,
    joined: bool,
) -> ArchivedRoom {
    let topic = room.topic();
    let avatar_url = room.avatar_url().map(|u| u.to_string());
    let canonical_alias = room.canonical_alias().map(|a| a.to_string());
    let is_direct = room.is_direct().await.unwrap_or(false);
    let is_encrypted = room.encryption_state().is_encrypted();
    let is_favourite = room.is_favourite();
    let is_space = room.is_space();
    let joined_member_count = room.joined_members_count();
    let room_type = room.room_type().map(|t| t.to_string());
    let join_rule = room.join_rule().map(|r| format!("{r:?}"));
    let history_visibility = room.history_visibility().map(|h| format!("{h:?}"));
    let guest_access = {
        let g = room.guest_access();
        format!("{g:?}")
    };

    let raw = serde_json::json!({
        "room_id": room_id,
        "name": name,
        "topic": topic,
        "avatar_url": avatar_url,
        "canonical_alias": canonical_alias,
        "is_direct": is_direct,
        "is_encrypted": is_encrypted,
        "is_favourite": is_favourite,
        "is_space": is_space,
        "joined": joined,
        "joined_member_count": joined_member_count,
        "room_type": room_type,
        "join_rule": join_rule,
        "history_visibility": history_visibility,
        "guest_access": &guest_access,
    });

    ArchivedRoom {
        room_id: room_id.to_string(),
        name: name.map(String::from),
        topic,
        avatar_url,
        canonical_alias,
        is_direct,
        is_encrypted,
        is_favourite,
        is_space,
        joined,
        joined_member_count,
        room_type,
        join_rule,
        history_visibility,
        guest_access: Some(guest_access),
        raw_json: serde_json::to_string(&raw).unwrap(),
    }
}

fn compute_gaps(ranges: &[(i64, i64)]) -> Vec<GapKind> {
    if ranges.is_empty() {
        return vec![GapKind::Fresh];
    }

    let mut gaps = Vec::new();

    // Gap after last range (newest messages)
    gaps.push(GapKind::AfterLast {
        end_ts: ranges.last().unwrap().1,
    });

    // Gaps between ranges
    for pair in ranges.windows(2) {
        if pair[0].1 < pair[1].0 {
            gaps.push(GapKind::Between {
                end_ts: pair[0].1,
                next_start_ts: pair[1].0,
            });
        }
    }

    // Gap before first range (oldest messages)
    gaps.push(GapKind::BeforeFirst {
        start_ts: ranges.first().unwrap().0,
    });

    gaps
}

fn mxc_to_path(media_dir: &Path, source: &MediaSource, suffix: &str) -> PathBuf {
    let uri = match source {
        MediaSource::Plain(uri) => uri.as_str(),
        MediaSource::Encrypted(file) => file.url.as_str(),
    };
    let stripped = uri.strip_prefix("mxc://").unwrap_or(uri);
    let path = media_dir.join(stripped);
    if suffix.is_empty() {
        path
    } else {
        let mut p = path.into_os_string();
        p.push(suffix);
        PathBuf::from(p)
    }
}

async fn download_media(client: &Client, source: &MediaSource, dest: &Path) -> anyhow::Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let request = MediaRequestParameters {
        source: source.clone(),
        format: MediaFormat::File,
    };
    let bytes = client.media().get_media_content(&request, true).await?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, &bytes)?;
    Ok(())
}

async fn download_thumbnail(
    client: &Client,
    source: &MediaSource,
    dest: &Path,
) -> anyhow::Result<()> {
    if dest.exists() {
        return Ok(());
    }
    let request = MediaRequestParameters {
        source: source.clone(),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(uint!(800), uint!(600))),
    };
    let bytes = client.media().get_media_content(&request, true).await?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, &bytes)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn build_archived(
    client: &Client,
    event: &AnySyncTimelineEvent,
    room_id: &str,
    room_name: Option<&str>,
    ts_millis: i64,
    raw: &matrix_sdk::ruma::serde::Raw<AnySyncTimelineEvent>,
    media_dir: &Path,
    media_cutoff: Option<i64>,
) -> Option<ArchivedMessage> {
    let event_id = event.event_id().to_string();
    let sender = event.sender().to_string();
    let event_type = event.event_type().to_string();
    let timestamp = chrono::DateTime::from_timestamp_millis(ts_millis)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    let raw_json = raw.json().to_string();

    let skip_media = media_cutoff.is_some_and(|cutoff| ts_millis < cutoff);

    let (body, msgtype_str, media_path, thumbnail_path) = match event {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) => {
            if let Some(original) = msg.as_original() {
                let body = original.content.msgtype.body().to_string();
                let msgtype_str = original.content.msgtype.msgtype().to_string();
                let (media_path, thumbnail_path) = if skip_media {
                    (None, None)
                } else {
                    download_event_media(client, &original.content.msgtype, media_dir).await
                };
                (Some(body), Some(msgtype_str), media_path, thumbnail_path)
            } else {
                (None, None, None, None)
            }
        }
        _ => (None, None, None, None),
    };

    Some(ArchivedMessage {
        event_id,
        room_id: room_id.to_string(),
        room_name: room_name.map(String::from),
        sender,
        timestamp,
        ts_millis,
        event_type,
        msgtype: msgtype_str,
        body,
        media_path,
        thumbnail_path,
        raw_json,
    })
}

async fn download_event_media(
    client: &Client,
    msgtype: &MessageType,
    media_dir: &Path,
) -> (Option<String>, Option<String>) {
    let (source, thumb_source) = match msgtype {
        MessageType::Image(c) => (
            Some(&c.source),
            c.info.as_ref().and_then(|i| i.thumbnail_source.as_ref()),
        ),
        MessageType::Video(c) => (
            Some(&c.source),
            c.info.as_ref().and_then(|i| i.thumbnail_source.as_ref()),
        ),
        MessageType::Audio(c) => (Some(&c.source), None),
        MessageType::File(c) => (
            Some(&c.source),
            c.info.as_ref().and_then(|i| i.thumbnail_source.as_ref()),
        ),
        _ => return (None, None),
    };

    let media_path = if let Some(source) = source {
        let dest = mxc_to_path(media_dir, source, "");
        match download_media(client, source, &dest).await {
            Ok(()) => Some(dest.to_string_lossy().to_string()),
            Err(_) => None,
        }
    } else {
        None
    };

    let thumbnail_path = if let Some(thumb) = thumb_source {
        let dest = mxc_to_path(media_dir, thumb, ".thumb");
        match download_thumbnail(client, thumb, &dest).await {
            Ok(()) => Some(dest.to_string_lossy().to_string()),
            Err(_) => None,
        }
    } else {
        None
    };

    (media_path, thumbnail_path)
}

async fn find_room(client: &Client, target: &str) -> anyhow::Result<matrix_sdk::Room> {
    if target.starts_with('!') {
        let room_id = matrix_sdk::ruma::RoomId::parse(target).context("Invalid room ID")?;
        return client
            .get_room(&room_id)
            .context(format!("Room not found: {target}"));
    }

    for room in client.rooms() {
        if let Ok(name) = room.display_name().await
            && name.to_string() == target
        {
            return Ok(room);
        }
    }

    anyhow::bail!("Room not found: {target}")
}
