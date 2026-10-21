//! Deterministic tool exercising host capability restrictions and interruptions.
wit_bindgen::generate!({ world: "tool-guest", path: "../../../wit" });
use exports::local::agent::tool_api::{Guest, ToolDescription};
struct Tool;

impl Guest for Tool {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "fixture".into(),
            description: "runtime fixture".into(),
            input_schema_json: "{}".into(),
        }
    }

    fn call(input: String) -> Result<String, String> {
        match input.as_str() {
            "spin" => loop {
                std::hint::black_box(42);
            },
            "sleep" => {
                std::thread::sleep(std::time::Duration::from_secs(60));
                Ok("slept".into())
            }
            "probe" => Ok(serde_json::json!({
                "env_count": std::env::vars().count(),
                "inside_read": std::fs::read_to_string("/allowed.txt").is_ok(),
                "outside_read": std::fs::read_to_string("/etc/passwd").is_ok(),
                "parent_read": std::fs::read_to_string("/../etc/passwd").is_ok(),
                "symlink_read": std::fs::read_to_string("/escape").is_ok(),
                "write": std::fs::write("/changed.txt", "bad").is_ok(),
                "socket": std::net::TcpStream::connect("127.0.0.1:1").is_ok(),
            })
            .to_string()),
            _ => Ok(input),
        }
    }
}
export!(Tool);
