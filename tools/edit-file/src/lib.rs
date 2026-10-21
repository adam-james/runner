wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use std::path::Path;

use exports::local::agent::tool_api::{Guest, ToolDescription};

struct EditFile;

impl Guest for EditFile {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "edit_file".to_string(),
            description: "Change part of a text file by replacing an exact piece of its text. \
                          `old_string` must match the file exactly, whitespace included, and \
                          appear in it only once, unless `replace_all` is set; include enough \
                          surrounding lines to make it unique. Paths start at /. Read the file \
                          first, and leave out the line numbers read_file adds."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to edit, e.g. '/notes/todo.md'",
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Text to replace, exactly as it is in the file.",
                    },
                    "new_string": {
                        "type": "string",
                        "description": "What to replace it with. Empty deletes it.",
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence, not just one. Defaults to false.",
                    },
                },
                "required": ["path", "old_string", "new_string"],
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
    let field = |name: &str| {
        input[name]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing '{name}' field"))
    };
    let path = field("path")?;
    let old = field("old_string")?;
    let new = field("new_string")?;
    let replace_all = match &input["replace_all"] {
        serde_json::Value::Null => false,
        value => value
            .as_bool()
            .ok_or_else(|| anyhow::anyhow!("'replace_all' must be true or false"))?,
    };

    // The only directory there is is mounted at /, so a relative path can
    // only mean one under it.
    edit_file(&Path::new("/").join(path), old, new, replace_all)
}

fn edit_file(path: &Path, old: &str, new: &str, replace_all: bool) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow::anyhow!("{} isn't a UTF-8 text file", path.display()))?;
    let (edited, lines) = replace(&text, old, new, replace_all)?;
    std::fs::write(path, edited).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    let lines = lines
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Ok(match lines.contains(',') {
        true => format!("Edited {}: replaced at lines {lines}", path.display()),
        false => format!("Edited {}: replaced at line {lines}", path.display()),
    })
}

/// `text` with `old` replaced by `new`, and the line (counting from 1) each
/// replacement starts on in the original. Without `replace_all`, `old` must
/// appear exactly once, so the model can't change a place it didn't mean to.
fn replace(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> anyhow::Result<(String, Vec<usize>)> {
    if old.is_empty() {
        anyhow::bail!("'old_string' is empty; use write_file to write a whole file");
    }
    if old == new {
        anyhow::bail!("'old_string' and 'new_string' are the same, so nothing would change");
    }
    let starts: Vec<usize> = text.match_indices(old).map(|(i, _)| i).collect();
    match starts.len() {
        0 => anyhow::bail!(
            "'old_string' isn't in the file; it must match exactly, whitespace included"
        ),
        1 => {}
        n if !replace_all => anyhow::bail!(
            "'old_string' appears {n} times; add surrounding lines to pick one, or set \
             'replace_all' to change them all"
        ),
        _ => {}
    }
    let lines = starts
        .iter()
        .map(|&i| text[..i].matches('\n').count() + 1)
        .collect();
    Ok((text.replace(old, new), lines))
}

export!(EditFile);

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("runner-edit-file-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_a_unique_match() {
        let (out, lines) = replace("a\nb\nc\n", "b\n", "B\n", false).unwrap();
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(lines, [2]);
    }

    #[test]
    fn needs_replace_all_for_several_matches() {
        let err = replace("x x x", "x", "y", false).unwrap_err().to_string();
        assert!(err.contains("appears 3 times"), "{err}");

        let (out, lines) = replace("x\nx\nx", "x", "y", true).unwrap();
        assert_eq!(out, "y\ny\ny");
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn rejects_missing_empty_and_unchanged_strings() {
        assert!(replace("abc", "z", "y", false).is_err());
        assert!(replace("abc", "", "y", false).is_err());
        assert!(replace("abc", "a", "a", false).is_err());
    }

    #[test]
    fn edits_the_file_in_place() {
        let dir = temp_dir("edit");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        let out = edit_file(&file, "two", "2", false).unwrap();
        assert!(out.ends_with("replaced at line 2"), "{out}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n2\nthree\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checks_inputs() {
        assert!(run(r#"{"path": "/a", "old_string": "x"}"#).is_err());
        assert!(
            run(r#"{"path": "/a", "old_string": "x", "new_string": "y", "replace_all": 1}"#)
                .is_err()
        );
    }
}
