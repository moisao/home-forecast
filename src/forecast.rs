//! The next 24 hours at a point.
//!
//! Each hour comes from the freshest run that reaches it (models.rs), and
//! each field is one byte-range request for a whole-grid message, decoded
//! to the values wanted and thrown away. Only those values are kept, in
//! memory and in a small JSON file, so a repeat costs nothing and a
//! restart doesn't download everything again.
//!
//! One model gives each hour's temperature, humidity and wind. Clouds and
//! rain are averaged with the other model's, and read over the area within
//! AREA_RADIUS_M rather than at one grid point: showers and cloud edges are
//! where 3 km models go wrong most, and where one is often right when the
//! other isn't (on 2026-11-16 RRFS dropped an evening downpour HRRR kept).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use chrono::{DateTime, Duration, DurationRound, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use log::warn;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::grid::{Area, areas_at, earth_wind, values_at};
use crate::models::{DEFAULT_MODELS, Fetcher, Field, Model, Timing};
use crate::sun::{cos_zenith, sun_times};
use crate::weather::{condition, relative_humidity, round, uv_index};

pub const HOURS: i64 = 24;

const TEMPERATURE: Field = Field::new("TMP", "2 m above ground", Timing::Instant);
const DEWPOINT: Field = Field::new("DPT", "2 m above ground", Timing::Instant);
const U10: Field = Field::new("UGRD", "10 m above ground", Timing::Instant);
const V10: Field = Field::new("VGRD", "10 m above ground", Timing::Instant);
const GUST: Field = Field::new("GUST", "surface", Timing::Instant);
const RAIN: Field = Field::new("APCP", "surface", Timing::HourTotal);
const TOTAL_CLOUD: Field = Field::new("TCDC", "entire atmosphere", Timing::Instant);
/// Every hour.
const HOURLY: [Field; 7] = [TEMPERATURE, DEWPOINT, U10, V10, GUST, RAIN, TOTAL_CLOUD];
/// While the sun is up, for the UV index: shortwave reaching the ground,
/// and what would under a clear sky.
const SOLAR: Field = Field::new("DSWRF", "surface", Timing::Instant);
const CLEAR_SKY_SOLAR: Field = Field::new("CSDSF", "surface", Timing::Instant);
/// Without these an hour is left out.
const REQUIRED: [Field; 4] = [TEMPERATURE, DEWPOINT, U10, V10];
/// Read from both models, over the area around the point.
const BLENDED: [Field; 2] = [RAIN, TOTAL_CLOUD];
/// The area clouds and rain are read over.
const AREA_RADIUS_M: f64 = 10_000.;
/// An hour with this much rain (mm) at a grid point counts it wet, for the
/// chance of rain: 0.01 in, the least a gauge reports.
const WET_MM: f64 = 0.25;

fn by_area(field: &Field) -> bool {
    BLENDED.contains(field)
}

/// Where a field read over the area is cached: its mean there...
fn area_key(field: &Field) -> String {
    format!("{}@{}km", field.key(), AREA_RADIUS_M / 1000.)
}

/// ...and the share of the area at or above WET_MM.
fn wet_key(field: &Field) -> String {
    format!("{}>wet", area_key(field))
}

/// What the cache is checked for to know whether a field is held yet.
fn held_key(field: &Field) -> String {
    if by_area(field) {
        area_key(field)
    } else {
        field.key()
    }
}

/// Parallel byte-range fetches. Each holds ~2 MB compressed and ~7 MB
/// decoded at once.
const WORKERS: usize = 8;

/// Which model answers the next 24 hours, and the run and lead of each,
/// plus the other model's for clouds and rain where it has them. Each
/// covers one more hour than it shows: an hour's rain is the hour after
/// its time, which the next valid time's file holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub model: Model,
    pub hours: crate::models::Resolved,
    pub blend: Option<Blend>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Blend {
    pub model: Model,
    pub hours: crate::models::Resolved,
}

/// The current whole hour and the 23 after it.
pub fn valid_times(now: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let start = now
        .duration_trunc(Duration::hours(1))
        .expect("hour truncation");
    (0..HOURS).map(|h| start + Duration::hours(h)).collect()
}

/// The first model with every hour, else whichever has the most -- one
/// model throughout, since a change of model mid-forecast reads as weather
/// -- and the other model to blend clouds and rain with. Given `models`
/// (MODEL), only those, with no blend.
pub fn plan(fetcher: &Fetcher, models: &[Model], now: DateTime<Utc>) -> Result<Plan> {
    let mut times = valid_times(now);
    times.push(times[times.len() - 1] + Duration::hours(1));
    let mut best: Option<Plan> = None;
    let mut errors = Vec::new();
    for &model in if models.is_empty() {
        &DEFAULT_MODELS[..]
    } else {
        models
    } {
        match fetcher.resolve(model, &times, now) {
            Ok(hours) => {
                let complete = hours.len() == times.len();
                if best.as_ref().is_none_or(|b| hours.len() > b.hours.len()) {
                    best = Some(Plan {
                        model,
                        hours,
                        blend: None,
                    });
                }
                if complete {
                    break;
                }
            }
            Err(e) => errors.push(format!("{}: {e}", model.name().to_uppercase())),
        }
    }
    match best {
        Some(mut plan) if !plan.hours.is_empty() => {
            if models.is_empty()
                && let Some(&other) = DEFAULT_MODELS.iter().find(|&&m| m != plan.model)
            {
                match fetcher.resolve(other, &times, now) {
                    Ok(hours) if !hours.is_empty() => {
                        plan.blend = Some(Blend {
                            model: other,
                            hours,
                        })
                    }
                    Ok(_) => {}
                    Err(e) => warn!(
                        "No {} to blend clouds and rain with: {e}",
                        other.name().to_uppercase()
                    ),
                }
            }
            Ok(plan)
        }
        _ => Err(format!(
            "No model has the next 24 hours{}",
            if errors.is_empty() {
                String::new()
            } else {
                format!(": {}", errors.join("; "))
            }
        )
        .into()),
    }
}

/// Point values already decoded: (model, run, lead, point) -> field ->
/// value, None where the file doesn't have the field.
#[derive(Default, Serialize, Deserialize)]
struct Store {
    values: HashMap<String, HashMap<String, Option<f64>>>,
}

pub struct ValueCache {
    store: Mutex<Store>,
    path: Option<PathBuf>,
}

impl ValueCache {
    /// Loaded from (and saved to) `dir/values.json` when a directory is given.
    pub fn new(dir: Option<PathBuf>) -> ValueCache {
        let path = dir.map(|d| d.join("values.json"));
        let store = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        ValueCache {
            store: Mutex::new(store),
            path,
        }
    }

    fn key(model: Model, run: DateTime<Utc>, lead: i64, lat: f64, lon: f64) -> String {
        format!(
            "{}|{}|{lead}|{lat:.3},{lon:.3}",
            model.name(),
            run.format("%Y%m%d%H")
        )
    }

    fn get(&self, key: &str, field: &str) -> Option<Option<f64>> {
        self.store
            .lock()
            .unwrap()
            .values
            .get(key)?
            .get(field)
            .copied()
    }

    fn put(&self, key: &str, field: &str, value: Option<f64>) {
        self.store
            .lock()
            .unwrap()
            .values
            .entry(key.to_string())
            .or_default()
            .insert(field.to_string(), value);
    }

    /// Drops runs more than two days old and writes the rest out.
    fn save(&self, now: DateTime<Utc>) {
        let cutoff = (now - Duration::days(2)).format("%Y%m%d%H").to_string();
        let json = {
            let mut store = self.store.lock().unwrap();
            store.values.retain(|key, _| {
                key.split('|')
                    .nth(1)
                    .is_some_and(|run| run >= cutoff.as_str())
            });
            serde_json::to_vec(&*store)
        };
        if let (Some(path), Ok(json)) = (&self.path, json) {
            let tmp = path.with_extension("json.part");
            if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, path)) {
                warn!("Couldn't save {}: {e}", path.display());
            }
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Hour {
    pub time: String,
    pub run: String,
    pub temperature_c: f64,
    pub dew_point_c: f64,
    pub humidity: f64,
    pub wind_speed_ms: f64,
    pub wind_gust_ms: Option<f64>,
    pub wind_bearing: f64,
    /// Over the hour starting at `time`.
    pub precipitation_mm: Option<f64>,
    /// The share of the area around the point, in both models, that gets
    /// measurable rain (0.01 in) in that hour, %.
    pub precipitation_probability: Option<f64>,
    pub cloud_coverage: Option<f64>,
    pub uv_index: f64,
    pub daylight: bool,
    pub condition: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct Forecast {
    /// The configured location's name; None for an ad-hoc point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    pub model: &'static str,
    pub generated: String,
    pub runs: Vec<String>,
    /// The model clouds and rain are averaged with, and its runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blend_model: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blend_runs: Vec<String>,
    /// Where the forecast is for, and the grid point it was read at: given
    /// only for an ad-hoc point, whose asker knows it anyway. A named
    /// location's coordinates stay in the service's configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_latitude: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid_longitude: Option<f64>,
    /// Downloaded for this update, shared by every location in it.
    pub downloaded_mb: f64,
    pub sunrise: Option<String>,
    pub sunset: Option<String>,
    pub sunrise_tomorrow: Option<String>,
    pub sunset_tomorrow: Option<String>,
    pub next_sunrise: Option<String>,
    pub next_sunset: Option<String>,
    pub hours: Vec<Hour>,
}

fn iso<T: TimeZone>(t: DateTime<T>) -> String
where
    T::Offset: std::fmt::Display,
{
    t.to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Grid point and wind rotation, stored beside the fields under these names.
const GRID_LAT: &str = "_grid_lat";
const GRID_LON: &str = "_grid_lon";
const WIND_ROTATION: &str = "_wind_rotation";

struct Task {
    run: DateTime<Utc>,
    lead: i64,
    field: Field,
    entry: crate::models::IdxEntry,
    /// Indexes into the points still lacking this field.
    points: Vec<usize>,
}

/// Fetches and decodes whatever the cache doesn't hold yet of `fields` for
/// one model's hours, at every point at once: each message is downloaded
/// and decoded once however many points need it. Returns bytes downloaded.
fn fill(
    fetcher: &Fetcher,
    cache: &ValueCache,
    model: Model,
    hours: &crate::models::Resolved,
    fields: &[Field],
    points: &[(f64, f64)],
) -> Result<u64> {
    let mut tasks = Vec::new();
    for &(valid, run, lead) in hours {
        let mut wanted: Vec<(Field, Vec<usize>)> = Vec::new();
        for field in fields {
            let needing: Vec<usize> = points
                .iter()
                .enumerate()
                .filter(|&(_, &(lat, lon))| {
                    let daylight_only = field.var == SOLAR.var || field.var == CLEAR_SKY_SOLAR.var;
                    (!daylight_only || cos_zenith(lat, lon, valid) > 0.)
                        && cache
                            .get(
                                &ValueCache::key(model, run, lead, lat, lon),
                                &held_key(field),
                            )
                            .is_none()
                })
                .map(|(i, _)| i)
                .collect();
            if !needing.is_empty() {
                wanted.push((*field, needing));
            }
        }
        if wanted.is_empty() {
            continue;
        }
        let index = fetcher
            .index(model, run, lead)?
            .ok_or_else(|| format!("{} {run} f{lead:02} is no longer posted", model.name()))?;
        for (field, needing) in wanted {
            match crate::models::find(&index, &field, lead) {
                Some(entry) => tasks.push(Task {
                    run,
                    lead,
                    field,
                    entry: entry.clone(),
                    points: needing,
                }),
                None => {
                    for i in needing {
                        let (lat, lon) = points[i];
                        cache.put(
                            &ValueCache::key(model, run, lead, lat, lon),
                            &held_key(&field),
                            None,
                        );
                    }
                }
            }
        }
    }

    let downloaded = AtomicU64::new(0);
    let queue = Mutex::new(tasks);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                loop {
                    let Some(task) = queue.lock().unwrap().pop() else {
                        break;
                    };
                    let wanted: Vec<(f64, f64)> = task.points.iter().map(|&i| points[i]).collect();
                    let result = fetcher
                        .message(model, task.run, task.lead, &task.entry)
                        .and_then(|bytes| {
                            downloaded.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                            if by_area(&task.field) {
                                areas_at(bytes, &wanted, AREA_RADIUS_M, WET_MM)
                            } else {
                                Ok(values_at(bytes, &wanted)?
                                    .into_iter()
                                    .map(|found| {
                                        found.map(|(value, point)| {
                                            (
                                                Area {
                                                    mean: value,
                                                    share: 0.,
                                                },
                                                point,
                                            )
                                        })
                                    })
                                    .collect())
                            }
                        });
                    match result {
                        Ok(values) => {
                            for (&(lat, lon), found) in wanted.iter().zip(values) {
                                let key = ValueCache::key(model, task.run, task.lead, lat, lon);
                                let held = held_key(&task.field);
                                match found {
                                    Some((area, point)) => {
                                        let value = area.mean;
                                        cache.put(&key, &held, value.is_finite().then_some(value));
                                        if task.field == RAIN {
                                            cache.put(&key, &wet_key(&RAIN), Some(area.share));
                                        }
                                        cache.put(&key, GRID_LAT, Some(point.lat));
                                        cache.put(&key, GRID_LON, Some(point.lon));
                                        cache.put(&key, WIND_ROTATION, Some(point.wind_rotation));
                                    }
                                    // Off the grid: nothing to read, ever.
                                    None => cache.put(&key, &held, None),
                                }
                            }
                        }
                        Err(e) => {
                            let mut first = first_error.lock().unwrap();
                            if first.is_none() {
                                *first = Some(format!(
                                    "{} f{:02} {}: {e}",
                                    task.run,
                                    task.lead,
                                    task.field.key()
                                ));
                            }
                        }
                    }
                }
            });
        }
    });
    if let Some(e) = first_error.into_inner().unwrap() {
        return Err(e.into());
    }
    Ok(downloaded.into_inner())
}

/// Where a forecast is for.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// A configured location's name; None for an ad-hoc point.
    pub name: Option<String>,
    pub lat: f64,
    pub lon: f64,
}

/// Fetches what the plan needs for every target in one pass, then builds
/// each one's forecast: Err for a target that couldn't be read (off the
/// grid, say), without failing the rest.
pub fn forecast_all(
    fetcher: &Fetcher,
    cache: &ValueCache,
    targets: &[Target],
    tz: Tz,
    plan: &Plan,
    now: DateTime<Utc>,
) -> Result<(Vec<Result<Forecast>>, u64)> {
    let points: Vec<(f64, f64)> = targets.iter().map(|t| (t.lat, t.lon)).collect();
    let all: Vec<Field> = HOURLY
        .iter()
        .chain([&SOLAR, &CLEAR_SKY_SOLAR])
        .copied()
        .collect();
    let mut downloaded = fill(fetcher, cache, plan.model, &plan.hours, &all, &points)?;
    if let Some(blend) = &plan.blend {
        // Without it the forecast is one model's, as before: worth serving.
        match fill(fetcher, cache, blend.model, &blend.hours, &BLENDED, &points) {
            Ok(bytes) => downloaded += bytes,
            Err(e) => warn!("Couldn't read {} to blend: {e}", blend.model.name()),
        }
    }
    cache.save(now);
    let forecasts = targets
        .iter()
        .map(|t| build(cache, t, tz, plan, now, downloaded))
        .collect();
    Ok((forecasts, downloaded))
}

/// One target's forecast from values already in the cache.
fn build(
    cache: &ValueCache,
    target: &Target,
    tz: Tz,
    plan: &Plan,
    now: DateTime<Utc>,
    downloaded: u64,
) -> Result<Forecast> {
    let (lat, lon) = (target.lat, target.lon);
    let end = valid_times(now)[0] + Duration::hours(HOURS);
    // valid time -> cache key, for each model.
    let keys = |model: Model, hours: &crate::models::Resolved| -> HashMap<DateTime<Utc>, String> {
        hours
            .iter()
            .map(|&(valid, run, lead)| (valid, ValueCache::key(model, run, lead, lat, lon)))
            .collect()
    };
    let primary = keys(plan.model, &plan.hours);
    let blend = plan
        .blend
        .as_ref()
        .map(|b| keys(b.model, &b.hours))
        .unwrap_or_default();
    // A value's mean over both models at a valid time, or the one there is.
    let blended = |valid: DateTime<Utc>, name: &str| {
        mean(
            [&primary, &blend]
                .iter()
                .filter_map(|keys| cache.get(keys.get(&valid)?, name).flatten()),
        )
    };
    let mut hours = Vec::new();
    let mut grid = None;
    let mut skipped = 0;
    for &(valid, run, lead) in plan.hours.iter().filter(|h| h.0 < end) {
        let key = ValueCache::key(plan.model, run, lead, lat, lon);
        let get = |field: &Field| cache.get(&key, &field.key()).flatten();
        if REQUIRED.iter().any(|f| get(f).is_none()) {
            skipped += 1;
            continue;
        }
        let rotation = cache.get(&key, WIND_ROTATION).flatten().unwrap_or(0.);
        grid = cache
            .get(&key, GRID_LAT)
            .flatten()
            .zip(cache.get(&key, GRID_LON).flatten());
        let t_c = get(&TEMPERATURE).unwrap() - 273.15;
        let td_c = get(&DEWPOINT).unwrap() - 273.15;
        let (wind, bearing) = earth_wind(get(&U10).unwrap(), get(&V10).unwrap(), rotation);
        let cloud = blended(valid, &area_key(&TOTAL_CLOUD));
        // The hour starting now, as Home Assistant reads an hour: the total
        // over the hour ending at the next valid time.
        let next = valid + Duration::hours(1);
        let rain = blended(next, &area_key(&RAIN));
        let rain_chance = blended(next, &wet_key(&RAIN));
        let mu = cos_zenith(lat, lon, valid);
        hours.push(Hour {
            time: iso(valid.with_timezone(&tz)),
            run: iso(run),
            temperature_c: round(t_c, 1),
            dew_point_c: round(td_c, 1),
            humidity: round(relative_humidity(t_c, td_c), 0),
            wind_speed_ms: round(wind, 1),
            wind_gust_ms: get(&GUST).map(|g| round(g, 1)),
            wind_bearing: round(bearing, 0),
            precipitation_mm: rain.map(|r| round(r, 2)),
            precipitation_probability: rain_chance.map(|p| round(p * 100., 0)),
            cloud_coverage: cloud.map(|c| round(c, 0)),
            uv_index: round(uv_index(mu, get(&SOLAR), get(&CLEAR_SKY_SOLAR), cloud), 1),
            daylight: mu > 0.,
            condition: condition(rain, cloud, wind, mu > 0.),
        });
    }
    if skipped > 0 && grid.is_some() {
        warn!("{skipped} hour(s) lacked a required field and were left out");
    }
    let Some((grid_lat, grid_lon)) = grid else {
        return Err(
            "No hour of the forecast could be read (is the point inside the model's CONUS grid?)"
                .into(),
        );
    };

    let runs_of = |hours: &crate::models::Resolved| {
        let mut runs: Vec<String> = hours
            .iter()
            .map(|&(_, run, _)| iso(run))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        runs.sort();
        runs
    };
    let sun = SunEvents::new(lat, lon, tz, now);
    let adhoc = target.name.is_none();
    Ok(Forecast {
        location: target.name.clone(),
        model: plan.model.label(),
        generated: iso(now.with_timezone(&tz)),
        runs: runs_of(&plan.hours),
        blend_model: plan.blend.as_ref().map(|b| b.model.label()),
        blend_runs: plan
            .blend
            .as_ref()
            .map(|b| runs_of(&b.hours))
            .unwrap_or_default(),
        latitude: adhoc.then_some(lat),
        longitude: adhoc.then_some(lon),
        grid_latitude: adhoc.then_some(round(grid_lat, 4)),
        grid_longitude: adhoc.then_some(round(grid_lon, 4)),
        downloaded_mb: round(downloaded as f64 / 1e6, 1),
        sunrise: sun.today.map(|s| iso(s.0)),
        sunset: sun.today.map(|s| iso(s.1)),
        sunrise_tomorrow: sun.tomorrow.map(|s| iso(s.0)),
        sunset_tomorrow: sun.tomorrow.map(|s| iso(s.1)),
        next_sunrise: sun.next(now, |s| s.0),
        next_sunset: sun.next(now, |s| s.1),
        hours,
    })
}

/// The mean of whatever values there are, None for none.
fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0., 0), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

type SunPair = (DateTime<Tz>, DateTime<Tz>);

/// Today's and tomorrow's sunrise and sunset, local time.
struct SunEvents {
    today: Option<SunPair>,
    tomorrow: Option<SunPair>,
}

impl SunEvents {
    fn new(lat: f64, lon: f64, tz: Tz, now: DateTime<Utc>) -> SunEvents {
        let today = now.with_timezone(&tz).date_naive();
        let local = |day| {
            sun_times(lat, lon, day).map(|(r, s)| (r.with_timezone(&tz), s.with_timezone(&tz)))
        };
        SunEvents {
            today: local(today),
            tomorrow: today.succ_opt().and_then(local),
        }
    }

    fn next(&self, now: DateTime<Utc>, pick: impl Fn(&SunPair) -> DateTime<Tz>) -> Option<String> {
        [self.today, self.tomorrow]
            .iter()
            .flatten()
            .map(&pick)
            .find(|t| *t > now)
            .map(iso)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_starts_at_the_current_hour() {
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 21, 34, 4).unwrap();
        let times = valid_times(now);
        assert_eq!(times.len(), 24);
        assert_eq!(
            times[0],
            Utc.with_ymd_and_hms(2026, 11, 14, 21, 0, 0).unwrap()
        );
        assert_eq!(
            times[23],
            Utc.with_ymd_and_hms(2026, 11, 15, 20, 0, 0).unwrap()
        );
    }

    #[test]
    fn next_sun_events() {
        let tz: Tz = "America/Los_Angeles".parse().unwrap();
        // 13:34 PST: today's sunset is still to come, sunrise is tomorrow's.
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 21, 34, 4).unwrap();
        let sun = SunEvents::new(32.988, -117.122, tz, now);
        assert_eq!(sun.next(now, |s| s.1).unwrap(), "2026-11-14T16:48:25-08:00");
        assert_eq!(sun.next(now, |s| s.0).unwrap(), "2026-11-15T06:18:58-08:00");
    }

    /// RRFS for everything, HRRR blended into clouds and rain; each hour's
    /// rain is the hour after its time.
    #[test]
    fn blends_clouds_and_rain_and_rain_is_the_hour_ahead() {
        let tz: Tz = "America/Los_Angeles".parse().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 17, 1, 20, 0).unwrap();
        let run = Utc.with_ymd_and_hms(2026, 11, 16, 21, 0, 0).unwrap();
        let (lat, lon) = (32.988, -117.122);
        let mut times = valid_times(now);
        times.push(times[23] + Duration::hours(1));
        let resolved: crate::models::Resolved = times
            .iter()
            .map(|&t| (t, run, (t - run).num_hours()))
            .collect();
        let cache = ValueCache::new(None);
        for &(_, run, lead) in &resolved {
            let rrfs = ValueCache::key(Model::Rrfs, run, lead, lat, lon);
            for (field, value) in [(TEMPERATURE, 290.), (DEWPOINT, 285.), (U10, 1.), (V10, 1.)] {
                cache.put(&rrfs, &field.key(), Some(value));
            }
            cache.put(&rrfs, GRID_LAT, Some(lat));
            cache.put(&rrfs, GRID_LON, Some(lon));
            cache.put(&rrfs, &area_key(&TOTAL_CLOUD), Some(20.));
            cache.put(&rrfs, &area_key(&RAIN), Some(0.));
            cache.put(&rrfs, &wet_key(&RAIN), Some(0.));
            let hrrr = ValueCache::key(Model::Hrrr, run, lead, lat, lon);
            cache.put(&hrrr, &area_key(&TOTAL_CLOUD), Some(100.));
            // HRRR rains only in the hour ending at 03Z.
            let wet = run + Duration::hours(lead) == times[2];
            cache.put(&hrrr, &area_key(&RAIN), Some(if wet { 6. } else { 0. }));
            cache.put(&hrrr, &wet_key(&RAIN), Some(if wet { 0.9 } else { 0. }));
        }
        let mut plan = Plan {
            model: Model::Rrfs,
            hours: resolved.clone(),
            blend: Some(Blend {
                model: Model::Hrrr,
                hours: resolved,
            }),
        };
        let target = Target {
            name: Some("home".into()),
            lat,
            lon,
        };
        let f = build(&cache, &target, tz, &plan, now, 0).unwrap();
        assert_eq!(f.hours.len(), 24);
        assert_eq!(f.blend_model, Some("HRRR 3 km"));
        assert_eq!(f.hours[0].cloud_coverage, Some(60.));
        // 18:00 PST is 02Z: the 02Z-03Z rain, half of HRRR's 6 mm.
        assert!(f.hours[1].time.starts_with("2026-11-16T18:00"));
        assert_eq!(f.hours[1].precipitation_mm, Some(3.));
        assert_eq!(f.hours[1].precipitation_probability, Some(45.));
        assert_eq!(f.hours[1].condition, "rainy");
        assert_eq!(f.hours[2].precipitation_mm, Some(0.));
        assert_eq!(f.hours[0].precipitation_mm, Some(0.));
        // The 24th hour's rain comes from the extra 25th valid time.
        assert_eq!(f.hours[23].precipitation_mm, Some(0.));

        // Without the blend: RRFS alone.
        plan.blend = None;
        let f = build(&cache, &target, tz, &plan, now, 0).unwrap();
        assert_eq!(f.hours[0].cloud_coverage, Some(20.));
        assert_eq!(f.hours[1].precipitation_mm, Some(0.));
        assert_eq!(f.blend_model, None);
    }

    #[test]
    fn cache_round_trip_and_pruning() {
        let dir = std::env::temp_dir().join(format!("hf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 11, 14, 21, 0, 0).unwrap();
        let cache = ValueCache::new(Some(dir.clone()));
        let fresh = ValueCache::key(Model::Rrfs, now, 3, 32.988, -117.122);
        let old = ValueCache::key(Model::Rrfs, now - Duration::days(3), 3, 32.988, -117.122);
        cache.put(&fresh, "TMP:2 m above ground", Some(290.));
        cache.put(&fresh, "CSDSF:surface", None);
        cache.put(&old, "TMP:2 m above ground", Some(280.));
        cache.save(now);
        let reloaded = ValueCache::new(Some(dir.clone()));
        assert_eq!(
            reloaded.get(&fresh, "TMP:2 m above ground"),
            Some(Some(290.))
        );
        assert_eq!(reloaded.get(&fresh, "CSDSF:surface"), Some(None));
        assert_eq!(reloaded.get(&old, "TMP:2 m above ground"), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
