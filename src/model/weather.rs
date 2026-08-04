//! Weather, from the US National Weather Service.
//!
//! `api.weather.gov` needs no key and no account, its data is US Government
//! work in the public domain with no attribution obligation, and it is the only
//! free source that also carries **alerts**. Everything here is the shape of
//! those requests and the parsing of their replies; the requests themselves are
//! made in `ui/`, the way `documents.rs` builds poppler command lines it does
//! not run.
//!
//! GNOME's own `libgweather` was the obvious candidate and is not usable: it
//! does not ship in `org.gnome.Sdk` or `org.gnome.Platform` 50 — GNOME Weather
//! bundles it in its own manifest — so it would be a third-party C dependency
//! rather than a platform built-in. It also has no alerts, its location
//! database contains no postal codes, and its met.no provider is compiled
//! against a hostname allowlisted to GNOME.
//!
//! **It is the United States only.** That is a real limit and the tool says so
//! rather than returning nothing for Paris. Open-Meteo would cover the rest of
//! the world, and was left out on purpose: its free tier is non-commercial and
//! CC-BY, which would put an attribution obligation on an app that currently
//! has none.
//!
//! Four things about this API cost a bug each if you take the documentation at
//! its word, and all four are handled here:
//!
//! 1. **Coordinates must be four decimal places.** Anything longer gets a 301
//!    to the truncated form: `/points/40.052914,-83.092507` redirects to
//!    `/points/40.0529,-83.0925`.
//! 2. **The first station in a grid's list is not necessarily reporting.** For
//!    grid `ILN 74,80` the list starts `KOSU`, which answers 404 for its latest
//!    observation; `KCMH`, the second, works. So the station is *found* by
//!    walking the list, and only then cached.
//! 3. **Observations are metric and forecasts are imperial**, in the same
//!    document set. The forecast says `74 F` and `13 mph`; the observation says
//!    `21` °C. Converting is ours.
//! 4. **Every observation value may be null.** Stations drop fields routinely,
//!    and a missing humidity is not an error.

use std::fmt::Write as _;

/// Where the weather is wanted.
///
/// Latitude and longitude rather than a place name, because that is what the
/// API takes and because a coordinate is the one form that is never ambiguous.
/// Preferences resolves a postcode into one of these once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub latitude: f64,
    pub longitude: f64,
}

impl Point {
    /// Truncated to four decimal places, which is what the API accepts.
    ///
    /// More than four is a 301 to exactly this string, so sending it unrounded
    /// costs a redirect on every call — and a client that does not follow
    /// redirects sees a failure it cannot explain. Four decimal places is about
    /// 11 metres, which is far finer than a 2.5 km forecast grid.
    pub fn as_query(&self) -> String {
        format!("{:.4},{:.4}", self.latitude, self.longitude)
    }

    /// Whether this is a coordinate at all. Rejects the (0, 0) that an empty
    /// preference field parses into as readily as a real place.
    pub fn is_plausible(&self) -> bool {
        (-90.0..=90.0).contains(&self.latitude)
            && (-180.0..=180.0).contains(&self.longitude)
            && !(self.latitude == 0.0 && self.longitude == 0.0)
    }
}

/// What `/points` said about a place: which office and grid cell cover it, and
/// which zone its alerts come under.
///
/// Worth caching — it changes rarely, and it is two requests before any weather
/// can be asked for. NWS warns the office and grid can change, so it is
/// re-resolved rather than kept forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grid {
    pub office: String,
    pub x: u32,
    pub y: u32,
    /// The forecast zone, for alerts: `OHZ055`.
    pub zone: String,
    /// The nearest named place, so an answer can say where it is talking about.
    pub place: Option<String>,
}

/// One observing station's latest report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observation {
    pub station: String,
    pub description: Option<String>,
    pub temperature_f: Option<f64>,
    pub feels_like_f: Option<f64>,
    pub humidity: Option<f64>,
    pub wind_mph: Option<f64>,
    pub wind_direction: Option<String>,
    pub taken: Option<String>,
}

/// One period of the forecast — "Today", "Tonight", "Saturday".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Period {
    pub name: String,
    pub temperature: Option<i64>,
    pub unit: String,
    pub wind: String,
    pub precipitation: Option<i64>,
    pub short: String,
    pub detailed: String,
}

/// One hour of the hourly forecast.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hour {
    /// Already formatted for reading — `14:00`, or `Sun 02:00` once it crosses
    /// midnight. The API gives an offset-aware timestamp and the offset is the
    /// *forecast's* zone, which is the one the answer should be in.
    pub label: String,
    pub temperature: Option<i64>,
    pub unit: String,
    pub precipitation: Option<i64>,
    pub short: String,
    pub wind: String,
}

/// An active watch, warning or advisory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Alert {
    pub event: String,
    pub severity: String,
    pub headline: Option<String>,
    pub description: Option<String>,
    pub ends: Option<String>,
}

pub const BASE: &str = "https://api.weather.gov";

/// Identifies the app, which the API requires. A request without one is
/// refused, and the documentation asks for contact information.
pub const USER_AGENT: &str = "familiar (https://github.com/mhagrelius/familiar)";

/// How many stations to try before giving up on current conditions.
///
/// The first is often not reporting; five covers every grid I have seen without
/// turning one question into a dozen requests.
pub const MAX_STATIONS: usize = 5;

/// How many forecast periods go to the model. Fourteen is the whole week in
/// day/night pairs, which is more than any question needs.
pub const MAX_PERIODS: usize = 8;

/// How many hours of the hourly forecast go to the model.
///
/// The endpoint returns 156 of them — every hour for a week. Twelve answers
/// "will I need a coat this afternoon" and "is it going to rain before I get
/// home", which is what hourly detail is *for*; the rest of the week is the
/// day/night periods' job and repeating it hour by hour would bury them.
pub const MAX_HOURS: usize = 12;

pub fn points_url(at: &Point) -> String {
    format!("{BASE}/points/{}", at.as_query())
}

pub fn forecast_url(grid: &Grid) -> String {
    format!(
        "{BASE}/gridpoints/{}/{},{}/forecast",
        grid.office, grid.x, grid.y
    )
}

pub fn stations_url(grid: &Grid) -> String {
    format!(
        "{BASE}/gridpoints/{}/{},{}/stations",
        grid.office, grid.x, grid.y
    )
}

pub fn observation_url(station: &str) -> String {
    format!("{BASE}/stations/{station}/observations/latest")
}

pub fn alerts_url(grid: &Grid) -> String {
    format!("{BASE}/alerts/active?zone={}", grid.zone)
}

/// Read `/points`.
pub fn parse_points(body: &str) -> Option<Grid> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let properties = value.get("properties")?;
    let place = properties
        .pointer("/relativeLocation/properties")
        .and_then(|near| {
            let city = near.get("city")?.as_str()?;
            let state = near.get("state")?.as_str()?;
            Some(format!("{city}, {state}"))
        });
    Some(Grid {
        office: properties.get("gridId")?.as_str()?.to_string(),
        x: properties.get("gridX")?.as_u64()? as u32,
        y: properties.get("gridY")?.as_u64()? as u32,
        zone: properties
            .get("forecastZone")?
            .as_str()?
            .rsplit('/')
            .next()?
            .to_string(),
        place,
    })
}

/// The stations covering a grid, in the order the API ranks them.
pub fn parse_stations(body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("features")
        .and_then(serde_json::Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(|feature| {
                    Some(
                        feature
                            .pointer("/properties/stationIdentifier")?
                            .as_str()?
                            .to_string(),
                    )
                })
                .take(MAX_STATIONS)
                .collect()
        })
        .unwrap_or_default()
}

/// Read one station's latest observation.
///
/// Returns `None` for a 404 body or anything without properties, which is what
/// a station that is not reporting looks like — the caller moves to the next.
pub fn parse_observation(station: &str, body: &str) -> Option<Observation> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let properties = value.get("properties")?;

    // Every value is `{unitCode, value, qualityControl}` and `value` is null
    // whenever the station did not report it.
    let measure =
        |key: &str| -> Option<f64> { properties.pointer(&format!("/{key}/value"))?.as_f64() };

    let temperature = measure("temperature");
    Some(Observation {
        station: station.to_string(),
        description: properties
            .get("textDescription")
            .and_then(serde_json::Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string),
        temperature_f: temperature.map(celsius_to_fahrenheit),
        // Whichever of the two the station reported; they are never both set.
        feels_like_f: measure("windChill")
            .or_else(|| measure("heatIndex"))
            .map(celsius_to_fahrenheit),
        humidity: measure("relativeHumidity"),
        wind_mph: measure("windSpeed").map(kilometres_to_miles),
        wind_direction: measure("windDirection").map(compass),
        taken: properties
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Read `/forecast`. Already imperial, unlike the observations.
pub fn parse_forecast(body: &str) -> Vec<Period> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .pointer("/properties/periods")
        .and_then(serde_json::Value::as_array)
        .map(|periods| {
            periods
                .iter()
                .take(MAX_PERIODS)
                .map(|period| Period {
                    name: text(period, "name"),
                    temperature: period
                        .get("temperature")
                        .and_then(serde_json::Value::as_i64),
                    unit: text(period, "temperatureUnit"),
                    wind: format!(
                        "{} {}",
                        text(period, "windSpeed"),
                        text(period, "windDirection")
                    )
                    .trim()
                    .to_string(),
                    precipitation: period
                        .pointer("/probabilityOfPrecipitation/value")
                        .and_then(serde_json::Value::as_i64),
                    short: text(period, "shortForecast"),
                    detailed: text(period, "detailedForecast"),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn hourly_url(grid: &Grid) -> String {
    format!(
        "{BASE}/gridpoints/{}/{},{}/forecast/hourly",
        grid.office, grid.x, grid.y
    )
}

/// Read `/forecast/hourly`, keeping the next [`MAX_HOURS`].
///
/// The times come back offset-aware in the forecast's own zone, which is the
/// zone the answer should be in — a forecast for Ohio reads in Eastern time
/// whatever the machine is set to. So the offset is kept rather than converted.
pub fn parse_hourly(body: &str) -> Vec<Hour> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let Some(periods) = value
        .pointer("/properties/periods")
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };

    // The first period's date, so only the hours that cross midnight need to
    // say which day they are.
    let first_day = periods
        .first()
        .and_then(parse_time)
        .map(|when| when.date_naive());

    periods
        .iter()
        .take(MAX_HOURS)
        .map(|period| {
            let label = match (parse_time(period), first_day) {
                (Some(when), Some(first)) if when.date_naive() != first => {
                    when.format("%a %H:%M").to_string()
                }
                (Some(when), _) => when.format("%H:%M").to_string(),
                (None, _) => text(period, "startTime"),
            };
            Hour {
                label,
                temperature: period
                    .get("temperature")
                    .and_then(serde_json::Value::as_i64),
                unit: text(period, "temperatureUnit"),
                precipitation: period
                    .pointer("/probabilityOfPrecipitation/value")
                    .and_then(serde_json::Value::as_i64),
                short: text(period, "shortForecast"),
                wind: format!(
                    "{} {}",
                    text(period, "windSpeed"),
                    text(period, "windDirection")
                )
                .trim()
                .to_string(),
            }
        })
        .collect()
}

fn parse_time(period: &serde_json::Value) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(period.get("startTime")?.as_str()?).ok()
}

pub fn parse_alerts(body: &str) -> Vec<Alert> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    value
        .get("features")
        .and_then(serde_json::Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(|feature| {
                    let properties = feature.get("properties")?;
                    Some(Alert {
                        event: text(properties, "event"),
                        severity: text(properties, "severity"),
                        headline: properties
                            .get("headline")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        description: properties
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(|text| text.chars().take(600).collect()),
                        ends: properties
                            .get("ends")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The whole answer, as the model reads it.
///
/// Alerts first: a tornado warning is the answer to "what is the weather"
/// whatever was actually asked. Then now, then the days.
pub fn frame(
    place: Option<&str>,
    observation: Option<&Observation>,
    hours: &[Hour],
    periods: &[Period],
    alerts: &[Alert],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## Weather — {}\n",
        place.unwrap_or("the requested location")
    );

    // Alerts first and under their own heading: a tornado warning is the answer
    // to "what is the weather" whatever was actually asked.
    if !alerts.is_empty() {
        out.push_str("### Active alerts\n\n");
        for alert in alerts {
            let _ = write!(out, "**{}** ({})", alert.event, alert.severity);
            if let Some(ends) = &alert.ends {
                let _ = write!(out, " — until {ends}");
            }
            out.push('\n');
            if let Some(headline) = &alert.headline {
                let _ = writeln!(out, "{headline}");
            }
            if let Some(description) = &alert.description {
                let _ = writeln!(out, "{}", description.replace('\n', " "));
            }
            out.push('\n');
        }
    }

    match observation {
        Some(now) => {
            let mut said = Vec::new();
            if let Some(description) = &now.description {
                said.push(description.clone());
            }
            if let Some(temperature) = now.temperature_f {
                said.push(format!("{temperature:.0}°F"));
            }
            if let Some(feels) = now.feels_like_f {
                said.push(format!("feels like {feels:.0}°F"));
            }
            if let Some(humidity) = now.humidity {
                said.push(format!("{humidity:.0}% humidity"));
            }
            if let Some(wind) = now.wind_mph {
                match &now.wind_direction {
                    Some(direction) if wind > 0.0 => {
                        said.push(format!("wind {wind:.0} mph {direction}"))
                    }
                    _ if wind > 0.0 => said.push(format!("wind {wind:.0} mph")),
                    _ => said.push("wind calm".to_string()),
                }
            }
            if said.is_empty() {
                said.push("the station reported no readings".to_string());
            }
            let _ = writeln!(out, "**Now:** {}", said.join(", "));
        }
        None => out.push_str(
            "**Now:** no nearby station is reporting, so there are no current conditions. The \
             forecast below is unaffected.\n",
        ),
    }

    // A table, because twelve hours as prose is twelve sentences nobody reads —
    // and because the answer is rendered as Markdown, so this is a table on
    // screen as well as an easy thing for the model to scan.
    if !hours.is_empty() {
        let _ = write!(out, "\n### Next {} hours\n\n", hours.len());
        out.push_str("| Time | Temp | Rain | Conditions | Wind |\n");
        out.push_str("|---|---|---|---|---|\n");
        for hour in hours {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                hour.label,
                hour.temperature
                    .map(|t| format!("{t}°{}", hour.unit))
                    .unwrap_or_else(|| "—".into()),
                hour.precipitation
                    .map(|chance| format!("{chance}%"))
                    .unwrap_or_else(|| "—".into()),
                if hour.short.is_empty() {
                    "—"
                } else {
                    &hour.short
                },
                if hour.wind.is_empty() {
                    "—"
                } else {
                    &hour.wind
                },
            );
        }
    }

    if !periods.is_empty() {
        out.push_str("\n### This week\n\n");
        for period in periods {
            let _ = write!(out, "- **{}:** {}", period.name, period.short);
            if let Some(temperature) = period.temperature {
                let _ = write!(out, ", {temperature}°{}", period.unit);
            }
            if let Some(chance) = period.precipitation.filter(|chance| *chance > 0) {
                let _ = write!(out, ", {chance}% chance of rain");
            }
            if !period.wind.trim().is_empty() {
                let _ = write!(out, ", wind {}", period.wind);
            }
            out.push('\n');
        }
    }

    if observation.is_none() && periods.is_empty() && hours.is_empty() && alerts.is_empty() {
        out.push_str("\nNothing came back for this location.\n");
    }

    // The source line goes on every answer, including the empty one: knowing
    // that nothing came back *from the National Weather Service* is what lets
    // the model say "no data for there" rather than "I could not reach it".
    let observed = observation.and_then(|now| {
        now.taken
            .as_deref()
            .map(|taken| (taken, now.station.as_str()))
    });
    match observed {
        Some((taken, station)) => {
            let _ = write!(
                out,
                "\n_US National Weather Service. Observed {taken} at station {station}._"
            );
        }
        None => out.push_str("\n_US National Weather Service._"),
    }
    out.trim_end().to_string()
}

/// What the model is told about the tool.
pub fn guidance() -> String {
    "You can get the weather with `weather`: current conditions, a seven-day forecast and any \
     active watches or warnings, from the US National Weather Service. It defaults to the \
     user's configured location, so `weather` with no arguments is usually right.\n\n\
     It covers the **United States only**. For anywhere else, say so plainly rather than \
     guessing — and do not fall back to `web_search` for a forecast unless the user asks, \
     since a search result is a page someone wrote at some point rather than current data.\n\n\
     Lead with an alert if there is one. Answer the question that was asked: \"is it going to \
     rain?\" wants a sentence, not the whole week."
        .to_string()
}

fn text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn celsius_to_fahrenheit(celsius: f64) -> f64 {
    celsius * 9.0 / 5.0 + 32.0
}

fn kilometres_to_miles(kilometres: f64) -> f64 {
    kilometres * 0.621_371
}

/// Degrees to a compass point, which is how a person says a wind direction.
fn compass(degrees: f64) -> String {
    const POINTS: [&str; 16] = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    let normalised = degrees.rem_euclid(360.0);
    // Each point spans 22.5°, so the boundary sits half a step either side.
    let index = ((normalised + 11.25) / 22.5).floor() as usize % 16;
    POINTS[index].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point() -> Point {
        Point {
            latitude: 40.052_914,
            longitude: -83.092_507,
        }
    }

    #[test]
    fn a_coordinate_is_cut_to_the_four_places_the_api_accepts() {
        // Anything longer answers 301 to the truncated form, so sending it
        // unrounded costs a redirect on every call — and a client that does not
        // follow redirects sees a failure with no explanation.
        //
        // *Which* way the last place rounds does not matter, and asserting on
        // it would be asserting on Rust's round-half-to-even rather than on
        // anything the API cares about: 40.0529,-83.0925 and 40.0529,-83.0925
        // both answer 200 and both resolve to grid ILN 74,80. The invariant is
        // the number of decimal places.
        let query = point().as_query();
        for part in query.split(',') {
            let places = part
                .split_once('.')
                .map(|(_, rest)| rest.len())
                .unwrap_or(0);
            assert_eq!(places, 4, "{query} has a part with {places} decimal places");
        }
        assert!(query.starts_with("40.0529,-83.092"), "{query}");
        assert_eq!(points_url(&point()), format!("{BASE}/points/{query}"));
    }

    #[test]
    fn a_southern_or_western_coordinate_keeps_its_sign() {
        let sydney = Point {
            latitude: -33.868_8,
            longitude: 151.209_3,
        };
        assert_eq!(sydney.as_query(), "-33.8688,151.2093");
    }

    #[test]
    fn an_empty_preference_is_not_a_location() {
        // A blank field parses to 0.0, which is a real coordinate in the Gulf
        // of Guinea and would silently return nothing useful.
        assert!(point().is_plausible());
        assert!(!Point {
            latitude: 0.0,
            longitude: 0.0
        }
        .is_plausible());
        assert!(!Point {
            latitude: 91.0,
            longitude: 0.0
        }
        .is_plausible());
    }

    fn grid() -> Grid {
        Grid {
            office: "ILN".into(),
            x: 74,
            y: 80,
            zone: "OHZ055".into(),
            place: Some("Bexley, OH".into()),
        }
    }

    #[test]
    fn the_points_reply_gives_a_grid_a_zone_and_a_place() {
        // Recorded from the real API for 40.0529,-83.0925.
        let body = r#"{"properties":{"gridId":"ILN","gridX":74,"gridY":80,
            "forecastZone":"https://api.weather.gov/zones/forecast/OHZ055",
            "timeZone":"America/New_York",
            "relativeLocation":{"properties":{"city":"Bexley","state":"OH"}}}}"#;
        assert_eq!(parse_points(body), Some(grid()));
    }

    #[test]
    fn every_url_is_built_from_the_grid() {
        assert_eq!(
            forecast_url(&grid()),
            "https://api.weather.gov/gridpoints/ILN/74,80/forecast"
        );
        assert_eq!(
            stations_url(&grid()),
            "https://api.weather.gov/gridpoints/ILN/74,80/stations"
        );
        assert_eq!(
            alerts_url(&grid()),
            "https://api.weather.gov/alerts/active?zone=OHZ055"
        );
        assert_eq!(
            observation_url("KCMH"),
            "https://api.weather.gov/stations/KCMH/observations/latest"
        );
    }

    #[test]
    fn stations_come_back_in_order_and_capped() {
        // The order matters: it is the order they get tried in, and the first
        // is not necessarily reporting.
        let body = r#"{"features":[
            {"properties":{"stationIdentifier":"KOSU"}},
            {"properties":{"stationIdentifier":"KCMH"}},
            {"properties":{"stationIdentifier":"KY70"}},
            {"properties":{"stationIdentifier":"KFFX"}},
            {"properties":{"stationIdentifier":"KAMN"}},
            {"properties":{"stationIdentifier":"KMKG"}}]}"#;
        let found = parse_stations(body);
        assert_eq!(found[0], "KOSU");
        assert_eq!(found.len(), MAX_STATIONS);
    }

    #[test]
    fn a_station_that_is_not_reporting_parses_as_nothing_to_try_the_next() {
        // `KOSU` is first in the list for grid ILN 74,80 and answers 404. If
        // that were treated as an error the tool would fail on a grid whose
        // second station works perfectly.
        let not_found = r#"{"correlationId":"64000df4","title":"Not Found",
            "type":"https://api.weather.gov/problems/NotFound","status":404}"#;
        assert_eq!(parse_observation("KOSU", not_found), None);
        assert_eq!(parse_observation("KOSU", "not json at all"), None);
    }

    #[test]
    fn an_observation_is_converted_out_of_the_metric_the_api_reports() {
        // The same document set gives the forecast in Fahrenheit and the
        // observation in Celsius. Reporting 21°F for a cloudy afternoon would
        // be a memorable bug.
        let body = r#"{"properties":{"timestamp":"2026-08-01T14:53:00+00:00",
            "textDescription":"Cloudy",
            "temperature":{"unitCode":"wmoUnit:degC","value":21.0},
            "relativeHumidity":{"value":64.0},
            "windSpeed":{"unitCode":"wmoUnit:km_h-1","value":16.0},
            "windDirection":{"value":70.0},
            "windChill":{"value":null},"heatIndex":{"value":null}}}"#;
        let now = parse_observation("KCMH", body).expect("an observation");

        assert_eq!(now.description.as_deref(), Some("Cloudy"));
        assert_eq!(now.temperature_f.map(|t| t.round()), Some(70.0));
        assert_eq!(now.wind_mph.map(|w| w.round()), Some(10.0));
        assert_eq!(now.wind_direction.as_deref(), Some("ENE"));
        assert_eq!(now.humidity, Some(64.0));
        assert_eq!(now.feels_like_f, None, "neither was reported");
    }

    #[test]
    fn a_station_that_dropped_every_reading_is_still_an_observation() {
        // Stations drop fields routinely; a missing humidity is not an error,
        // and treating it as one would throw away the fields that did arrive.
        let body = r#"{"properties":{"textDescription":"",
            "temperature":{"value":null},"relativeHumidity":{"value":null},
            "windSpeed":{"value":null},"windDirection":{"value":null}}}"#;
        let now = parse_observation("KY70", body).expect("an observation");
        assert_eq!(now.temperature_f, None);
        assert_eq!(
            now.description, None,
            "an empty string is not a description"
        );

        // And it frames as a sentence rather than an empty line.
        let framed = frame(None, Some(&now), &[], &[], &[]);
        assert!(framed.contains("no readings"), "{framed}");
    }

    #[test]
    fn a_forecast_period_keeps_the_imperial_units_it_arrived_in() {
        let body = r#"{"properties":{"periods":[
            {"name":"Today","temperature":74,"temperatureUnit":"F",
             "probabilityOfPrecipitation":{"value":63},
             "windSpeed":"13 mph","windDirection":"ENE",
             "shortForecast":"Showers And Thunderstorms",
             "detailedForecast":"A chance of showers."}]}}"#;
        let periods = parse_forecast(body);
        assert_eq!(periods[0].name, "Today");
        assert_eq!(periods[0].temperature, Some(74));
        assert_eq!(periods[0].unit, "F");
        assert_eq!(periods[0].wind, "13 mph ENE");
        assert_eq!(periods[0].precipitation, Some(63));
    }

    #[test]
    fn a_null_chance_of_precipitation_is_left_out_rather_than_read_as_zero() {
        let body = r#"{"properties":{"periods":[
            {"name":"Tonight","temperature":58,"temperatureUnit":"F",
             "probabilityOfPrecipitation":{"value":null},
             "windSpeed":"5 mph","windDirection":"N",
             "shortForecast":"Clear","detailedForecast":"Clear."}]}}"#;
        let periods = parse_forecast(body);
        assert_eq!(periods[0].precipitation, None);
        let framed = frame(None, None, &[], &periods, &[]);
        assert!(!framed.contains("chance of precipitation"), "{framed}");
    }

    fn hourly_body(count: usize, start_hour: u32) -> String {
        let periods: Vec<String> = (0..count)
            .map(|step| {
                // 1 August 2026 was a Saturday; stepping past 23:00 rolls into
                // Sunday, which is the case the label has to handle.
                let hour = start_hour as usize + step;
                let (day, hour) = (1 + hour / 24, hour % 24);
                format!(
                    r#"{{"startTime":"2026-08-{day:02}T{hour:02}:00:00-04:00",
                        "temperature":{},"temperatureUnit":"F",
                        "probabilityOfPrecipitation":{{"value":36}},
                        "windSpeed":"13 mph","windDirection":"ENE",
                        "shortForecast":"Chance Rain Showers"}}"#,
                    69 + step
                )
            })
            .collect();
        format!(r#"{{"properties":{{"periods":[{}]}}}}"#, periods.join(","))
    }

    #[test]
    fn the_hourly_forecast_is_cut_to_the_next_few_hours() {
        // The endpoint returns 156 periods — a week, hour by hour. All of it
        // would bury the daily forecast it is meant to add detail to.
        let hours = parse_hourly(&hourly_body(156, 11));
        assert_eq!(hours.len(), MAX_HOURS);
        assert_eq!(hours[0].label, "11:00");
        assert_eq!(hours[0].temperature, Some(69));
        assert_eq!(hours[0].unit, "F");
        assert_eq!(hours[0].precipitation, Some(36));
        assert_eq!(hours[0].wind, "13 mph ENE");
    }

    #[test]
    fn an_hour_past_midnight_says_which_day_it_is() {
        // "02:00" under a table that started at 22:00 is ambiguous, and the
        // question hourly detail answers is usually about tonight.
        let hours = parse_hourly(&hourly_body(6, 22));
        assert_eq!(hours[0].label, "22:00");
        assert_eq!(hours[1].label, "23:00");
        assert_eq!(hours[2].label, "Sun 00:00", "it crossed midnight");
        assert_eq!(hours[3].label, "Sun 01:00");
    }

    #[test]
    fn the_hourly_forecast_is_a_table_rather_than_twelve_sentences() {
        // It is rendered as Markdown for the user and scanned as text by the
        // model; a table serves both, and twelve sentences serve neither.
        let hours = parse_hourly(&hourly_body(12, 11));
        let framed = frame(Some("Bexley, OH"), None, &hours, &[], &[]);
        assert!(framed.contains("### Next 12 hours"), "{framed}");
        assert!(
            framed.contains("| Time | Temp | Rain | Conditions | Wind |"),
            "{framed}"
        );
        assert!(framed.contains("| 11:00 | 69°F | 36% |"), "{framed}");
        // One header, one delimiter, twelve rows.
        assert_eq!(
            framed.lines().filter(|line| line.starts_with("| ")).count(),
            13
        );
    }

    #[test]
    fn a_missing_hourly_value_is_a_dash_rather_than_a_gap() {
        // A ragged table row shifts every column after it, which is worse than
        // saying nothing is known.
        let body = r#"{"properties":{"periods":[
            {"startTime":"2026-08-01T11:00:00-04:00","temperature":null,
             "temperatureUnit":"F","probabilityOfPrecipitation":{"value":null},
             "windSpeed":"","windDirection":"","shortForecast":""}]}}"#;
        let hours = parse_hourly(body);
        let framed = frame(None, None, &hours, &[], &[]);
        let row = framed
            .lines()
            .find(|line| line.starts_with("| 11:00"))
            .expect("the row");
        assert_eq!(row.matches('|').count(), 6, "{row}");
        assert!(row.contains("—"), "{row}");
    }

    #[test]
    fn an_unparseable_timestamp_falls_back_to_what_arrived() {
        let body = r#"{"properties":{"periods":[
            {"startTime":"not a time","temperature":70,"temperatureUnit":"F",
             "windSpeed":"5 mph","windDirection":"N","shortForecast":"Clear"}]}}"#;
        assert_eq!(parse_hourly(body)[0].label, "not a time");
    }

    #[test]
    fn no_alerts_is_an_empty_list_and_not_a_failure() {
        assert!(parse_alerts(r#"{"features":[]}"#).is_empty());
        assert!(parse_alerts("garbage").is_empty());
    }

    #[test]
    fn an_alert_leads_the_answer() {
        // A tornado warning is the answer to "what is the weather" whatever was
        // actually asked, so it goes above the conditions rather than under the
        // seven-day forecast.
        let alerts = parse_alerts(
            r#"{"features":[{"properties":{"event":"Tornado Warning","severity":"Extreme",
               "headline":"Tornado Warning issued August 1","description":"Take shelter now.",
               "ends":"2026-08-01T16:00:00-04:00"}}]}"#,
        );
        assert_eq!(alerts.len(), 1);

        let periods = parse_forecast(
            r#"{"properties":{"periods":[{"name":"Today","temperature":74,
               "temperatureUnit":"F","windSpeed":"13 mph","windDirection":"ENE",
               "shortForecast":"Storms","detailedForecast":"Storms."}]}}"#,
        );
        let framed = frame(Some("Bexley, OH"), None, &[], &periods, &alerts);
        let alert_at = framed.find("Tornado Warning").expect("the alert");
        let forecast_at = framed.find("### This week").expect("the forecast");
        assert!(alert_at < forecast_at, "{framed}");
        assert!(framed.contains("Take shelter now."), "{framed}");
    }

    #[test]
    fn the_framing_names_the_place_and_the_source() {
        let framed = frame(Some("Bexley, OH"), None, &[], &[], &[]);
        assert!(framed.contains("Bexley, OH"), "{framed}");
        assert!(framed.contains("National Weather Service"), "{framed}");
    }

    #[test]
    fn a_grid_with_no_reporting_station_still_gives_the_forecast() {
        // Every station 404ing is a real state, and losing the forecast to it
        // would be losing the useful half.
        let periods = parse_forecast(
            r#"{"properties":{"periods":[{"name":"Today","temperature":74,
               "temperatureUnit":"F","windSpeed":"13 mph","windDirection":"ENE",
               "shortForecast":"Sunny","detailedForecast":"Sunny."}]}}"#,
        );
        let framed = frame(Some("Bexley, OH"), None, &[], &periods, &[]);
        assert!(
            framed.contains("no nearby station is reporting"),
            "{framed}"
        );
        assert!(framed.contains("Sunny"), "{framed}");
    }

    #[test]
    fn degrees_become_the_compass_point_a_person_would_say() {
        assert_eq!(compass(0.0), "N");
        assert_eq!(compass(70.0), "ENE");
        assert_eq!(compass(180.0), "S");
        assert_eq!(compass(350.0), "N", "it wraps");
        assert_eq!(compass(-10.0), "N", "and wraps the other way");
        assert_eq!(compass(360.0), "N");
    }

    #[test]
    fn the_conversions_are_the_right_way_round() {
        assert_eq!(celsius_to_fahrenheit(0.0), 32.0);
        assert_eq!(celsius_to_fahrenheit(100.0), 212.0);
        assert!((kilometres_to_miles(16.0) - 9.94).abs() < 0.01);
    }

    #[test]
    fn the_guidance_admits_the_country_it_covers() {
        // A tool that returns nothing for Paris without saying why teaches the
        // model that the tool is broken.
        let guidance = guidance();
        assert!(guidance.contains("United States only"), "{guidance}");
    }
}
