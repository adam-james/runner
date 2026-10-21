wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use std::path::Path;

use exports::local::agent::tool_api::{Guest, ToolDescription};

struct WriteFile;

impl Guest for WriteFile {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "write_file".to_string(),
            description: "Write a text file, replacing everything in it if it already \
                          exists. Missing parent directories are created. Paths start at /. \
                          To change part of an existing file, use edit_file instead."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to write, e.g. '/notes/todo.md'",
                    },
                    "content": {
                        "type": "string",
                        "description": "The file's whole new contents.",
                    },
                },
                "required": ["path", "content"],
            })
            .to_string(),
        }
    }

    fn call(input_json: String) -> Result<String, String> {
        run(&input_json).map_err(|e| e.to_string())
    }
}

fn run(input_json: &str) -> anyhow::Result<String> {
    let input: serde_json::Value = serde_json::from_str(input_json)?;
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'path' field"))?;
    let content = input["content"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'content' field"))?;

    // The only directory there is is mounted at /, so a relative path can
    // only mean one under it.
    write_file(&Path::new("/").join(path), content)
}

fn write_file(path: &Path, content: &str) -> anyhow::Result<String> {
    let existed = match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => anyhow::bail!("{} is a directory", path.display()),
        Ok(_) => true,
        Err(_) => false,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow::anyhow!("{}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    let lines = content.lines().count();
    Ok(format!(
        "{} {} ({}, {lines} line{})",
        if existed { "Overwrote" } else { "Created" },
        path.display(),
        size(content.len() as u64),
        if lines == 1 { "" } else { "s" },
    ))
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

export!(WriteFile);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("runner-write-file-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_then_overwrites() {
        let dir = temp_dir("overwrite");
        let file = dir.join("a.txt");
        let out = write_file(&file, "one\ntwo\n").unwrap();
        assert!(out.starts_with("Created "), "{out}");
        assert!(out.ends_with("(8 B, 2 lines)"), "{out}");

        let out = write_file(&file, "three").unwrap();
        assert!(out.starts_with("Overwrote "), "{out}");
        assert!(out.ends_with("(5 B, 1 line)"), "{out}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "three");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn creates_parent_directories() {
        let dir = temp_dir("parents");
        let file = dir.join("x/y/z.md");
        write_file(&file, "hi").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hi");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_to_replace_a_directory() {
        let dir = temp_dir("dir");
        let err = write_file(&dir, "hi").unwrap_err().to_string();
        assert!(err.ends_with("is a directory"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_inputs() {
        assert!(run(r#"{"content": "hi"}"#).is_err());
        assert!(run(r#"{"path": "/a.txt"}"#).is_err());
    }
}
