use std::path::Path;

use serde_json::json;

use crate::archive::Archive;

pub struct Rooms {
    pub favorites: bool,
    pub joined: bool,
    pub raw: bool,
}

impl Rooms {
    pub fn run(&self, archive_path: &Path) -> anyhow::Result<()> {
        let archive = Archive::open(archive_path)?;
        let rooms = archive.query_rooms(self.favorites, self.joined)?;

        for room in &rooms {
            if self.raw {
                println!("{}", room.raw_json);
            } else {
                let obj = json!({
                    "room_id": room.room_id,
                    "name": room.name,
                    "topic": room.topic,
                    "is_direct": room.is_direct,
                    "is_encrypted": room.is_encrypted,
                    "is_favourite": room.is_favourite,
                    "is_space": room.is_space,
                    "joined": room.joined,
                    "joined_member_count": room.joined_member_count,
                });
                println!("{}", serde_json::to_string(&obj).unwrap());
            }
        }

        Ok(())
    }
}
