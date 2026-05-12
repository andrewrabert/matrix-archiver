use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use matrix_sdk::Client;
use matrix_sdk::deserialized_responses::TimelineEventKind;
use matrix_sdk::ruma::events::AnySyncTimelineEvent;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::serde::Raw;

use crate::archive::Archive;

pub async fn run(client: &Client, archive_path: &Path) -> anyhow::Result<()> {
    let archive = Archive::open(archive_path)?;
    let events = archive.encrypted_events()?;

    if events.is_empty() {
        eprintln!("No encrypted events to decrypt");
        return Ok(());
    }

    eprintln!("{} encrypted events found", events.len());

    let mut decrypted_count = 0u64;
    let mut failed_count = 0u64;
    let mut downloaded_rooms: HashSet<String> = HashSet::new();

    for (event_id, room_id, raw_json) in &events {
        let room_id_parsed = match matrix_sdk::ruma::RoomId::parse(room_id.as_str()) {
            Ok(id) => id,
            Err(_) => {
                failed_count += 1;
                continue;
            }
        };

        let room = match client.get_room(&room_id_parsed) {
            Some(room) => room,
            None => {
                eprintln!("\r  {event_id}: room not found {room_id}");
                failed_count += 1;
                continue;
            }
        };

        let raw_encrypted: Raw<OriginalSyncRoomEncryptedEvent> =
            match serde_json::from_str(raw_json) {
                Ok(raw) => raw,
                Err(e) => {
                    eprintln!("\r  {event_id}: parse error: {e}");
                    failed_count += 1;
                    continue;
                }
            };

        // First attempt
        let mut timeline_event = match room.decrypt_event(&raw_encrypted, None).await {
            Ok(te) => te,
            Err(e) => {
                eprintln!("\r  {event_id}: {e}");
                failed_count += 1;
                continue;
            }
        };

        // If missing session and we haven't downloaded keys for this room yet, try once
        if matches!(
            &timeline_event.kind,
            TimelineEventKind::UnableToDecrypt { .. }
        ) && !downloaded_rooms.contains(room_id)
        {
            downloaded_rooms.insert(room_id.clone());
            let _ = client
                .encryption()
                .backups()
                .download_room_keys_for_room(&room_id_parsed)
                .await;

            // Retry
            timeline_event = match room.decrypt_event(&raw_encrypted, None).await {
                Ok(te) => te,
                Err(e) => {
                    eprintln!("\r  {event_id}: {e}");
                    failed_count += 1;
                    continue;
                }
            };
        }

        if let TimelineEventKind::UnableToDecrypt { utd_info, .. } = &timeline_event.kind {
            eprintln!("\r  {event_id}: unable to decrypt: {:?}", utd_info.reason);
            failed_count += 1;
        } else if let TimelineEventKind::Decrypted(_) = &timeline_event.kind {
            let raw_decrypted = timeline_event.raw();
            let decrypted_json = raw_decrypted.json().to_string();

            if let Ok(deserialized) = raw_decrypted.deserialize() {
                let event: &AnySyncTimelineEvent = &deserialized;
                let event_type = event.event_type().to_string();

                let (body, msgtype) = match &deserialized {
                    AnySyncTimelineEvent::MessageLike(
                        matrix_sdk::ruma::events::AnySyncMessageLikeEvent::RoomMessage(msg),
                    ) => {
                        if let Some(original) = msg.as_original() {
                            (
                                Some(original.content.msgtype.body().to_string()),
                                Some(original.content.msgtype.msgtype().to_string()),
                            )
                        } else {
                            (None, None)
                        }
                    }
                    _ => (None, None),
                };

                archive.update_decrypted(
                    event_id,
                    &event_type,
                    msgtype.as_deref(),
                    body.as_deref(),
                    &decrypted_json,
                )?;

                decrypted_count += 1;
            } else {
                failed_count += 1;
            }
        } else {
            eprintln!("\r  {event_id}: unexpected kind");
            failed_count += 1;
        }

        let total = decrypted_count + failed_count;
        eprint!(
            "\r  {total}/{} ({decrypted_count} decrypted, {failed_count} failed)",
            events.len()
        );
        let _ = std::io::stderr().flush();
    }

    eprintln!();
    Ok(())
}
