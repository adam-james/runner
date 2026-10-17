wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use wstd::http::{Body, Client, Request};

struct Search;

const DEFAULT_MAX_RESULTS: u64 = 5;
const MAX_MAX_RESULTS: u64 = 10;

impl Guest for Search {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "web_search".to_string(),
            description: "Search the web. Returns the top results, each with a title, \
                          URL, and a relevant snippet of the page."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for",
                    },
                    "topic": {
                        "type": "string",
                        "enum": ["general", "news"],
                        "description": "Use 'news' for recent events and current affairs. Defaults to 'general'.",
                    },
                    "max_results": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_MAX_RESULTS,
                        "description": "How many results to return. Defaults to 5.",
                    },
                },
                "required": ["query"],
            })
            .to_string(),
        }
    }

    fn call(input_json: String) -> Result<String, String> {
        wstd::runtime::block_on(run(input_json)).map_err(|e| e.to_string())
    }
}

async fn run(input_json: String) -> anyhow::Result<String> {
    let input: serde_json::Value = serde_json::from_str(&input_json)?;
    let query = input["query"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'query' field"))?;
    let topic = match input["topic"].as_str() {
        None | Some("general") => "general",
        Some("news") => "news",
        Some(other) => anyhow::bail!("unsupported topic '{other}'"),
    };
    let max_results = input["max_results"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_RESULTS)
        .clamp(1, MAX_MAX_RESULTS);

    // No credentials here -- the host adds `authorization` to this request on
    // its way out, per this tool's `headers` in the runner config.
    let results = post_json(
        "https://api.tavily.com/search",
        &serde_json::json!({
            "query": query,
            "topic": topic,
            "max_results": max_results,
            "search_depth": "basic",
        }),
    )
    .await?;

    Ok(format_results(query, &results))
}

/// e.g. "1. Title (2026-10-17)\n   https://...\n   snippet...", one block per result.
fn format_results(query: &str, response: &serde_json::Value) -> String {
    let results = response["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return format!("No results for '{query}'.");
    }
    results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let title = result["title"].as_str().unwrap_or("(untitled)");
            let url = result["url"].as_str().unwrap_or("");
            let content = result["content"].as_str().unwrap_or("").trim();
            let date = result["published_date"]
                .as_str()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            format!("{}. {title}{date}\n   {url}\n   {content}", i + 1)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn post_json(url: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let request = Request::post(url)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))?;
    let response = Client::new().send(request).await?;
    let status = response.status();
    let mut body = response.into_body();
    let bytes = body.contents().await?;
    if !status.is_success() {
        anyhow::bail!(
            "tavily returned {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(bytes)
        );
    }
    Ok(serde_json::from_slice(bytes)?)
}

export!(Search);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_results() {
        let response = serde_json::json!({
            "query": "rust 1.100",
            "results": [
                {
                    "title": "Announcing Rust 1.100",
                    "url": "https://blog.rust-lang.org/rust-1.100",
                    "content": "  The Rust team is happy to announce...  ",
                    "score": 0.98,
                    "published_date": "2026-11-26",
                },
                {
                    "title": "wasm32-wasip3 - The rustc book",
                    "url": "https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip3.html",
                    "content": "Tier: 2",
                    "score": 0.71,
                },
            ],
            "response_time": 1.2,
        });
        assert_eq!(
            format_results("rust 1.100", &response),
            "1. Announcing Rust 1.100 (2026-11-26)\n   \
             https://blog.rust-lang.org/rust-1.100\n   \
             The Rust team is happy to announce...\n\n\
             2. wasm32-wasip3 - The rustc book\n   \
             https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip3.html\n   \
             Tier: 2"
        );
    }

    #[test]
    fn reports_no_results() {
        let response = serde_json::json!({ "query": "zzzz", "results": [] });
        assert_eq!(format_results("zzzz", &response), "No results for 'zzzz'.");
    }
}
