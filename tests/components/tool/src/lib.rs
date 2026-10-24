//! Deterministic tool exercising host capability restrictions and interruptions.
wit_bindgen::generate!({ world: "tool-guest", path: "../../../wit" });
use exports::local::agent::tool_api::{Guest, ToolDescription};
struct Tool;
static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
            "counter" => {
                Ok((CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1).to_string())
            }
            "trap" => panic!("fixture tool trap"),
            // Touches every byte, so the memory is really grown.
            alloc if alloc.starts_with("alloc:") => {
                let mb: usize = alloc["alloc:".len()..].parse().map_err(|_| "bad size")?;
                Ok(vec![1u8; mb << 20]
                    .iter()
                    .map(|&b| b as usize)
                    .sum::<usize>()
                    .to_string())
            }
            "read-allowed" => std::fs::read_to_string("/allowed.txt").map_err(|e| e.to_string()),
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
