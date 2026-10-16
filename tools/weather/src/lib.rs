wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use wstd::http::{Body, Client, Request};

struct Weather;

impl Guest for Weather {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "get_weather".to_string(),
            description: "Get the current weather for a named location.".to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "City name, e.g. 'Seattle'",
                    },
                },
                "required": ["location"],
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
    let location = input["location"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing 'location' field"))?;

    let geo_url = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1",
        urlencoding::encode(location)
    );
    let geo = get_json(&geo_url).await?;
    let result = &geo["results"][0];
    let lat = result["latitude"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("location '{location}' not found"))?;
    let lon = result["longitude"].as_f64().unwrap_or_default();
    let name = result["name"].as_str().unwrap_or(location);

    let weather_url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true"
    );
    let weather = get_json(&weather_url).await?;
    let temp = weather["current_weather"]["temperature"]
        .as_f64()
        .unwrap_or(f64::NAN);
    let wind = weather["current_weather"]["windspeed"]
        .as_f64()
        .unwrap_or(f64::NAN);

    Ok(format!(
        "Current weather in {name}: {temp}\u{b0}C, wind {wind} km/h"
    ))
}

async fn get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let request = Request::get(url).body(Body::empty())?;
    let response = Client::new().send(request).await?;
    let mut body = response.into_body();
    let bytes = body.contents().await?;
    Ok(serde_json::from_slice(bytes)?)
}

export!(Weather);
