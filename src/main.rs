//! home-forecast: the next three days at named locations from NOAA's RRFS
//! (HRRR as a fallback), as JSON -- made for a Home Assistant weather
//! entity.
//!
//!     home-forecast serve                         # HTTP on $PORT (8080)
//!     home-forecast print <name>                  # a LOCATIONS entry
//!     home-forecast print --lat 32.988 --lon -117.122
//!
//! Environment:
//!   LOCATIONS     name=lat,lon entries, `;`- or newline-separated
//!                 ("home=32.988,-117.122; mt-laguna=32.87,-116.42")
//!   ALLOW_ADHOC   true: also answer /forecast?lat=..&lon=.. for any point
//!   PORT          default 8080
//!   TZ            the zone times are given in (default America/Los_Angeles)
//!   CACHE_DIR     keep decoded point values here across restarts
//!   FORECAST_HOURS  how far ahead, 1 to 84 (default 72)
//!   MODEL         rrfs or hrrr only, instead of RRFS then HRRR, with the
//!                 other's clouds and rain blended in
//!   RUST_LOG      default info

mod forecast;
mod grid;
mod locations;
mod models;
mod server;
mod sun;
mod weather;

use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use chrono_tz::Tz;

use forecast::Target;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const USAGE: &str = "usage: home-forecast serve | print <name> | print --lat LAT --lon LON";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tz: Tz = env("TZ")
        .as_deref()
        .unwrap_or("America/Los_Angeles")
        .parse()
        .map_err(|e| format!("TZ: {e}"))?;
    let models = match env("MODEL") {
        Some(name) => vec![
            models::Model::parse(&name).ok_or_else(|| format!("MODEL: unknown model {name}"))?,
        ],
        None => vec![],
    };
    let hours = match env("FORECAST_HOURS") {
        Some(v) => v
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|h| (1..=forecast::MAX_HOURS).contains(h))
            .ok_or_else(|| {
                format!(
                    "FORECAST_HOURS: a number of hours from 1 to {}",
                    forecast::MAX_HOURS
                )
            })?,
        None => forecast::DEFAULT_HOURS,
    };
    let locations = locations::parse(&env("LOCATIONS").unwrap_or_default())?;
    let allow_adhoc = env("ALLOW_ADHOC")
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    let cache_dir = env("CACHE_DIR").map(PathBuf::from);
    if let Some(dir) = &cache_dir {
        std::fs::create_dir_all(dir)?;
    }
    let cache = forecast::ValueCache::new(cache_dir);

    match args.first().map(String::as_str) {
        Some("serve") => {
            if locations.is_empty() && !allow_adhoc {
                return Err("Nothing to serve: set LOCATIONS, or ALLOW_ADHOC=true".into());
            }
            let port = env("PORT").as_deref().unwrap_or("8080").parse()?;
            let config = server::Config {
                tz,
                hours,
                models,
                locations,
                allow_adhoc,
            };
            server::serve(port, server::Forecaster::new(config, cache))
        }
        Some("print") => {
            let target = match args.get(1).map(String::as_str) {
                Some(name) if !name.starts_with('-') => locations
                    .into_iter()
                    .find(|t| t.name.as_deref() == Some(name))
                    .ok_or_else(|| format!("{name} isn't in LOCATIONS"))?,
                _ => {
                    let flag = |name: &str| -> Result<f64> {
                        let at = args.iter().position(|a| a == name).ok_or(USAGE)?;
                        Ok(args.get(at + 1).ok_or(USAGE)?.parse()?)
                    };
                    Target {
                        name: None,
                        lat: flag("--lat")?,
                        lon: flag("--lon")?,
                    }
                }
            };
            let fetcher = models::Fetcher::new();
            let now = Utc::now();
            let plan = forecast::plan(&fetcher, &models, hours, now)?;
            let (mut forecasts, _) =
                forecast::forecast_all(&fetcher, &cache, &[target], tz, &plan, now)?;
            println!("{}", serde_json::to_string_pretty(&forecasts.remove(0)?)?);
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("home-forecast: {e}");
            ExitCode::FAILURE
        }
    }
}
