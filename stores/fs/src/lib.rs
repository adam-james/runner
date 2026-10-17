wit_bindgen::generate!({
    world: "session-store-guest",
    path: "../../wit",
    // The on-disk format is these types' serde representation.
    additional_derives: [serde::Serialize, serde::Deserialize],
});

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use exports::local::agent::session_store::Guest;
use local::agent::messages::Message;

/// This component's own root, not the host's: the host mounts the
/// directory named by `dir` in its config here (so `/trip.json` is
/// `<dir>/trip.json` on the host), and nothing outside it is visible.
const ROOT: &str = "/";
const EXTENSION: &str = "json";

struct FsStore;

impl Guest for FsStore {
    fn load(id: String) -> Result<Option<Vec<Message>>, String> {
        let text = match fs::read_to_string(path(&id)) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("reading session {id}: {e}")),
        };
        serde_json::from_str(&text).map_err(|e| format!("parsing session {id}: {e}"))
    }

    fn save(id: String, messages: Vec<Message>) -> Result<(), String> {
        let text = serde_json::to_string_pretty(&messages).map_err(|e| e.to_string())?;
        // Written to the side, then renamed over the old file, so a crash
        // mid-write never leaves a half-written session behind.
        let path = path(&id);
        let tmp = path.with_extension(format!("{EXTENSION}.tmp"));
        fs::write(&tmp, text).map_err(|e| format!("writing session {id}: {e}"))?;
        fs::rename(&tmp, &path).map_err(|e| format!("writing session {id}: {e}"))
    }

    fn ids() -> Result<Vec<String>, String> {
        let entries = fs::read_dir(ROOT).map_err(|e| format!("listing sessions: {e}"))?;
        let mut ids: Vec<String> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension()? != EXTENSION {
                    return None;
                }
                Some(path.file_stem()?.to_str()?.to_string())
            })
            .collect();
        ids.sort();
        Ok(ids)
    }
}

fn path(id: &str) -> PathBuf {
    PathBuf::from(ROOT).join(format!("{id}.{EXTENSION}"))
}

export!(FsStore);
