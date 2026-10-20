wit_bindgen::generate!({
    world: "tool-guest",
    path: "../../wit",
});

use exports::local::agent::tool_api::{Guest, ToolDescription};
use wstd::http::{Body, Client, Request};

struct Weather;

/// Open-Meteo forecasts up to 16 days ahead, but two days of hourly lines
/// is already plenty to hand a model.
const MAX_HOURS: u64 = 48;

impl Guest for Weather {
    fn describe() -> ToolDescription {
        ToolDescription {
            name: "get_weather".to_string(),
            description: "Get the current weather for a named location, and optionally an \
                          hourly forecast for the hours ahead, in the location's local time."
                .to_string(),
            input_schema_json: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "City name, e.g. 'Seattle'",
                    },
                    "hours": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_HOURS,
                        "description": "Hours of hourly forecast to include, starting with the \
                                        current hour. Omit for current conditions only.",
                    },
                    "units": {
                        "type": "string",
                        "enum": ["celsius", "fahrenheit"],
                        "description": "Temperature units. Defaults to celsius.",
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
    let hours = match &input["hours"] {
        serde_json::Value::Null => None,
        value => match value.as_u64() {
            Some(h @ 1..=MAX_HOURS) => Some(h),
            _ => anyhow::bail!("'hours' must be a whole number from 1 to {MAX_HOURS}"),
        },
    };
    let unit = match &input["units"] {
        serde_json::Value::Null => Unit::Celsius,
        value => match value.as_str() {
            Some("celsius") => Unit::Celsius,
            Some("fahrenheit") => Unit::Fahrenheit,
            _ => anyhow::bail!("'units' must be \"celsius\" or \"fahrenheit\""),
        },
    };

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

    let mut weather_url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current_weather=true&temperature_unit={}",
        unit.param()
    );
    if let Some(hours) = hours {
        weather_url.push_str(&format!(
            "&hourly={HOURLY_FIELDS}&forecast_hours={hours}&timezone=auto"
        ));
    }
    let weather = get_json(&weather_url).await?;
    let current = &weather["current_weather"];
    let temp = current["temperature"].as_f64().unwrap_or(f64::NAN);
    let wind = current["windspeed"].as_f64().unwrap_or(f64::NAN);
    let conditions = current["weathercode"]
        .as_u64()
        .map_or("unknown conditions", describe_code);

    let mut out = format!(
        "Current weather in {name}: {temp}{}, {conditions}, wind {wind} km/h",
        unit.symbol()
    );
    if hours.is_some() {
        out.push_str("\n\n");
        out.push_str(&format_hourly(&weather["hourly"], unit)?);
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum Unit {
    Celsius,
    Fahrenheit,
}

impl Unit {
    /// Open-Meteo's `temperature_unit` value.
    fn param(self) -> &'static str {
        match self {
            Unit::Celsius => "celsius",
            Unit::Fahrenheit => "fahrenheit",
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Unit::Celsius => "\u{b0}C",
            Unit::Fahrenheit => "\u{b0}F",
        }
    }
}

const HOURLY_FIELDS: &str = "temperature_2m,weather_code,precipitation_probability,wind_speed_10m";

/// One line per hour from Open-Meteo's `hourly` block, which holds a `time`
/// array (local, since the request asks for `timezone=auto`) and one
/// parallel array per field.
fn format_hourly(hourly: &serde_json::Value, unit: Unit) -> anyhow::Result<String> {
    let times = hourly["time"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("forecast is missing hourly data"))?;
    let field = |name: &str, i: usize| hourly[name][i].as_f64();

    let mut out = String::from("Hourly forecast (local time):");
    for (i, time) in times.iter().enumerate() {
        let time = time.as_str().unwrap_or("?").replace('T', " ");
        let temp =
            field("temperature_2m", i).map_or("?".to_string(), |t| format!("{t}{}", unit.symbol()));
        let conditions = hourly["weather_code"][i]
            .as_u64()
            .map_or("unknown conditions", describe_code);
        let rain =
            field("precipitation_probability", i).map_or("?".to_string(), |p| format!("{p}%"));
        let wind = field("wind_speed_10m", i).map_or("?".to_string(), |w| format!("{w} km/h"));
        out.push_str(&format!(
            "\n{time}: {temp}, {conditions}, {rain} chance of precipitation, wind {wind}"
        ));
    }
    Ok(out)
}

/// A WMO weather interpretation code, as Open-Meteo reports them.
fn describe_code(code: u64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 => "light rain",
        63 => "rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "light snow",
        73 => "snow",
        75 => "heavy snow",
        77 => "snow grains",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown conditions",
    }
}

async fn get_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let request = Request::get(url).body(Body::empty())?;
    let response = Client::new().send(request).await?;
    let mut body = response.into_body();
    let bytes = body.contents().await?;
    Ok(serde_json::from_slice(bytes)?)
}

export!(Weather);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hourly_has_one_line_per_hour() {
        let hourly = serde_json::json!({
            "time": ["2026-10-17T14:00", "2026-10-17T15:00"],
            "temperature_2m": [14.5, 13.9],
            "weather_code": [3, 61],
            "precipitation_probability": [20, 65],
            "wind_speed_10m": [10.1, 12.4],
        });
        assert_eq!(
            format_hourly(&hourly, Unit::Celsius).unwrap(),
            "Hourly forecast (local time):\n\
             2026-10-17 14:00: 14.5\u{b0}C, overcast, 20% chance of precipitation, wind 10.1 km/h\n\
             2026-10-17 15:00: 13.9\u{b0}C, light rain, 65% chance of precipitation, wind 12.4 km/h"
        );
    }

    #[test]
    fn hourly_marks_missing_values() {
        // Open-Meteo sends null where a model has no value for that hour.
        let hourly = serde_json::json!({
            "time": ["2026-10-17T14:00"],
            "temperature_2m": [14.5],
            "weather_code": [0],
            "precipitation_probability": [null],
            "wind_speed_10m": [5.0],
        });
        assert!(
            format_hourly(&hourly, Unit::Celsius)
                .unwrap()
                .contains("? chance of precipitation")
        );
    }

    #[test]
    fn hourly_uses_requested_unit() {
        let hourly = serde_json::json!({
            "time": ["2026-10-17T14:00"],
            "temperature_2m": [58.1],
            "weather_code": [0],
            "precipitation_probability": [0],
            "wind_speed_10m": [5.0],
        });
        assert!(
            format_hourly(&hourly, Unit::Fahrenheit)
                .unwrap()
                .contains("58.1\u{b0}F, clear sky")
        );
    }

    #[test]
    fn hourly_requires_times() {
        assert!(format_hourly(&serde_json::json!({}), Unit::Celsius).is_err());
    }
}
