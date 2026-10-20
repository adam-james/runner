wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use std::path::Path;

use exports::local::agent::tool_api::{Guest, ToolDescription};

struct Files;

/// Lines returned when the model doesn't ask for a number, and the most it
/// can ask for. Long files are read a page at a time with `offset`.
const DEFAULT_LIMIT: u64 = 500;
const MAX_LIMIT: u64 = 2000;
/// Longest a single line gets, so one minified line can't fill the reply.
const MAX_LINE_CHARS: usize = 1000;
/// Most entries listed for one directory.
const MAX_ENTRIES: usize = 500;

impl Guest for Files {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "read_file".to_string(),
            description: "Read a text file, with each line numbered, or list a directory's \
                          contents. Paths start at /. Long files come back a page at a \
                          time: use `offset` to read on from where a page ends."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File or directory to read, e.g. '/' or '/notes/todo.md'",
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Line to start reading from. Defaults to 1.",
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIMIT,
                        "description": format!("Most lines to return. Defaults to {DEFAULT_LIMIT}."),
                    },
                },
                "required": ["path"],
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
    let offset = whole_number(&input["offset"], "offset", 1, u64::MAX)?.unwrap_or(1);
    let limit = whole_number(&input["limit"], "limit", 1, MAX_LIMIT)?.unwrap_or(DEFAULT_LIMIT);

    // The only directory there is is mounted at /, so a relative path can
    // only mean one under it.
    let path = Path::new("/").join(path);
    let metadata =
        std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        list_dir(&path)
    } else {
        read_file(&path, offset, limit)
    }
}

/// `value` as a whole number from `min` to `max`, or `None` if it's absent.
fn whole_number(
    value: &serde_json::Value,
    name: &str,
    min: u64,
    max: u64,
) -> anyhow::Result<Option<u64>> {
    match value {
        serde_json::Value::Null => Ok(None),
        value => match value.as_u64() {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ if max == u64::MAX => anyhow::bail!("'{name}' must be a whole number from {min}"),
            _ => anyhow::bail!("'{name}' must be a whole number from {min} to {max}"),
        },
    }
}

fn read_file(path: &Path, offset: u64, limit: u64) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let text = String::from_utf8(bytes).map_err(|e| {
        anyhow::anyhow!(
            "{} isn't a UTF-8 text file ({})",
            path.display(),
            size(e.as_bytes().len() as u64)
        )
    })?;
    Ok(numbered_lines(&text, offset, limit))
}

/// `text`'s lines from `offset` (counting from 1), at most `limit` of them,
/// each prefixed with its number, plus a note saying where to read on if
/// there's more.
fn numbered_lines(text: &str, offset: u64, limit: u64) -> String {
    let total = text.lines().count() as u64;
    if total == 0 {
        return "(empty file)".to_string();
    }
    if offset > total {
        return format!("(the file has {total} lines; offset {offset} is past the end)");
    }

    let mut out = String::new();
    let end = (offset - 1 + limit).min(total);
    for (n, line) in text
        .lines()
        .enumerate()
        .skip(offset as usize - 1)
        .take(limit as usize)
    {
        let line = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((cut, _)) => format!("{}… [line cut]", &line[..cut]),
            None => line.to_string(),
        };
        out.push_str(&format!("{:>6}\t{line}\n", n + 1));
    }
    if end < total {
        out.push_str(&format!(
            "[lines {offset}-{end} of {total}; read on with offset {}]\n",
            end + 1
        ));
    }
    out
}

/// One entry per line, sorted, directories marked with a trailing `/` and
/// files with their size.
fn list_dir(path: &Path) -> anyhow::Result<String> {
    let mut entries: Vec<(String, Option<u64>)> = std::fs::read_dir(path)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = entry.metadata().ok();
            Some(match metadata {
                Some(m) if m.is_dir() => (format!("{name}/"), None),
                Some(m) => (name, Some(m.len())),
                None => (name, None),
            })
        })
        .collect();
    entries.sort();
    Ok(format_listing(path, &entries))
}

fn format_listing(path: &Path, entries: &[(String, Option<u64>)]) -> String {
    if entries.is_empty() {
        return format!("{} is empty", path.display());
    }
    let mut out = String::new();
    for (name, len) in entries.iter().take(MAX_ENTRIES) {
        match len {
            Some(len) => out.push_str(&format!("{name}  {}\n", size(*len))),
            None => out.push_str(&format!("{name}\n")),
        }
    }
    if entries.len() > MAX_ENTRIES {
        out.push_str(&format!(
            "[{} more entries not shown]\n",
            entries.len() - MAX_ENTRIES
        ));
    }
    out
}

fn size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

export!(Files);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("runner-files-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn numbers_lines_from_offset() {
        assert_eq!(numbered_lines("a\nb\nc\n", 2, 10), "     2\tb\n     3\tc\n");
    }

    #[test]
    fn says_where_to_read_on() {
        let text = (1..=10).map(|n| format!("line {n}\n")).collect::<String>();
        let out = numbered_lines(&text, 3, 4);
        assert!(out.starts_with("     3\tline 3\n"));
        assert!(out.ends_with("[lines 3-6 of 10; read on with offset 7]\n"));
    }

    #[test]
    fn handles_empty_files_and_offsets_past_the_end() {
        assert_eq!(numbered_lines("", 1, 10), "(empty file)");
        assert_eq!(
            numbered_lines("a\nb\n", 5, 10),
            "(the file has 2 lines; offset 5 is past the end)"
        );
    }

    #[test]
    fn cuts_long_lines_on_char_boundaries() {
        let line = "é".repeat(MAX_LINE_CHARS + 5);
        let out = numbered_lines(&line, 1, 1);
        assert!(out.contains(&format!("{}… [line cut]", "é".repeat(MAX_LINE_CHARS))));
    }

    #[test]
    fn lists_directories_sorted_with_sizes() {
        let dir = temp_dir("list");
        std::fs::write(dir.join("b.txt"), "hello").unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        assert_eq!(list_dir(&dir).unwrap(), "a/\nb.txt  5 B\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_binary_files() {
        let dir = temp_dir("binary");
        let file = dir.join("blob.bin");
        std::fs::write(&file, [0xff, 0xfe, 0x00]).unwrap();
        let err = read_file(&file, 1, 10).unwrap_err().to_string();
        assert!(err.ends_with("isn't a UTF-8 text file (3 B)"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_number_inputs() {
        assert!(run(r#"{"path": "/", "limit": 0}"#).is_err());
        assert!(run(r#"{"path": "/", "offset": -1}"#).is_err());
        assert!(run(r#"{"offset": 1}"#).is_err());
    }
}
