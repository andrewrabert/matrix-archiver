use std::collections::BTreeMap;
use std::path::Path;

use crate::archive::Archive;

pub fn run(archive_path: &Path) -> anyhow::Result<()> {
    let archive = Archive::open(archive_path)?;
    let rows = archive.all_members()?;

    // user_id -> display_name -> [room_ids]
    let mut users: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();

    for (user_id, display_name, room_id) in rows {
        let name = display_name.unwrap_or_default();
        users
            .entry(user_id)
            .or_default()
            .entry(name)
            .or_default()
            .push(room_id);
    }

    println!("{}", serde_json::to_string_pretty(&users)?);
    Ok(())
}
