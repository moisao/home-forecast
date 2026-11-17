//! HTTP:
//!
//!     GET /forecast/<name>          a location from LOCATIONS
//!     GET /forecast?lat=..&lon=..   any point, only with ALLOW_ADHOC
//!     GET /locations                the configured names (never coordinates)
//!     GET /healthz
//!
//! Every target -- the configured locations, and ad-hoc points asked for
//! in the last week -- is updated together whenever a newer model run
//! posts, so each GRIB2 message is downloaded once for all of them. The
//! configured locations are forecast from startup; a request that comes
//! before its first forecast is ready waits up to WAIT, then gets 503
//! "warming".
//!
//! Logs name locations and never print coordinates.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use log::{error, info, warn};
use serde_json::json;
use tiny_http::{Header, Request, Response, Server};

use crate::Result;
use crate::forecast::{self, Plan, Target, ValueCache};
use crate::models::{Fetcher, Model};

/// How long a request waits for a first forecast before answering 503.
const WAIT: StdDuration = StdDuration::from_secs(20);
/// How often the background loop checks for a newer run. A check that
/// finds nothing new is a handful of index lookups.
const REFRESH: StdDuration = StdDuration::from_secs(300);
/// An ad-hoc point not asked for in this long stops being kept current.
const FORGET_AFTER_DAYS: i64 = 7;

pub struct Config {
    pub tz: Tz,
    pub models: Vec<Model>,
    pub locations: Vec<Target>,
    pub allow_adhoc: bool,
}

fn named_key(name: &str) -> String {
    format!("name:{name}")
}

/// ~100 m: every request from one house lands on one entry.
fn adhoc_key(lat: f64, lon: f64) -> String {
    format!("point:{lat:.3},{lon:.3}")
}

#[derive(Default)]
struct State {
    /// The plan the current results were built from.
    plan: Option<Plan>,
    results: HashMap<String, Arc<String>>,
    /// The last failure, and the plan it happened under -- None when the
    /// planning or download failed rather than the target. One that failed
    /// under a plan (a location off the grid, say) isn't tried again until
    /// the plan moves on, rather than on every request.
    errors: HashMap<String, (String, Option<Plan>)>,
    /// Ad-hoc points and when each was last asked for.
    adhoc: HashMap<String, (Target, DateTime<Utc>)>,
}

pub struct Forecaster {
    fetcher: Fetcher,
    cache: ValueCache,
    config: Config,
    /// One update at a time: a second would only fetch the same messages.
    updating: Mutex<()>,
    state: Mutex<State>,
}

impl Forecaster {
    pub fn new(config: Config, cache: ValueCache) -> Forecaster {
        Forecaster {
            fetcher: Fetcher::new(),
            cache,
            config,
            updating: Mutex::new(()),
            state: Mutex::new(State::default()),
        }
    }

    fn targets(&self) -> Vec<(String, Target)> {
        let state = self.state.lock().unwrap();
        let named = self
            .config
            .locations
            .iter()
            .map(|t| (named_key(t.name.as_deref().unwrap_or_default()), t.clone()));
        named
            .chain(state.adhoc.iter().map(|(k, (t, _))| (k.clone(), t.clone())))
            .collect()
    }

    /// Brings every target up to the newest plan: all of them when the
    /// plan has moved on, else only those without a forecast yet.
    fn update(&self) {
        let _one_at_a_time = self.updating.lock().unwrap();
        let now = Utc::now();
        let plan = match forecast::plan(&self.fetcher, &self.config.models, now) {
            Ok(plan) => plan,
            Err(e) => {
                error!("Planning failed: {e}");
                // Recorded for targets with nothing to show yet; one that
                // has a forecast keeps serving it.
                let keys: Vec<String> = self.targets().into_iter().map(|(k, _)| k).collect();
                let mut state = self.state.lock().unwrap();
                for key in keys {
                    if !state.results.contains_key(&key) {
                        state.errors.insert(key, (e.to_string(), None));
                    }
                }
                return;
            }
        };
        let todo: Vec<(String, Target)> = {
            let targets = self.targets();
            let state = self.state.lock().unwrap();
            if state.plan.as_ref() == Some(&plan) {
                targets
                    .into_iter()
                    .filter(|(k, _)| {
                        !state.results.contains_key(k)
                            && state
                                .errors
                                .get(k)
                                .is_none_or(|(_, under)| under.as_ref() != Some(&plan))
                    })
                    .collect()
            } else {
                targets
            }
        };
        if todo.is_empty() {
            return;
        }
        let started = Instant::now();
        let targets: Vec<Target> = todo.iter().map(|(_, t)| t.clone()).collect();
        match forecast::forecast_all(
            &self.fetcher,
            &self.cache,
            &targets,
            self.config.tz,
            &plan,
            now,
        ) {
            Ok((forecasts, downloaded)) => {
                let mut state = self.state.lock().unwrap();
                let mut runs = Vec::new();
                for ((key, target), result) in todo.iter().zip(forecasts) {
                    let label = target
                        .name
                        .clone()
                        .unwrap_or_else(|| "an ad-hoc point".into());
                    match result.and_then(|f| {
                        runs = f.runs.clone();
                        Ok(serde_json::to_string(&f)?)
                    }) {
                        Ok(json) => {
                            state.results.insert(key.clone(), Arc::new(json));
                            state.errors.remove(key);
                        }
                        Err(e) => {
                            warn!("No forecast for {label}: {e}");
                            state.results.remove(key);
                            state
                                .errors
                                .insert(key.clone(), (e.to_string(), Some(plan.clone())));
                        }
                    }
                }
                state.plan = Some(plan.clone());
                info!(
                    "Updated {} location(s): {}{}, runs {}, {:.0} MB downloaded, in {:.1}s",
                    todo.len(),
                    plan.model.label(),
                    plan.blend
                        .as_ref()
                        .map(|b| format!(" + {} clouds and rain", b.model.label()))
                        .unwrap_or_default(),
                    runs.join(", "),
                    downloaded as f64 / 1e6,
                    started.elapsed().as_secs_f64()
                );
            }
            Err(e) => {
                error!("Update failed: {e}");
                // The download's failure, not the targets': retried next time.
                let mut state = self.state.lock().unwrap();
                for (key, _) in &todo {
                    state.errors.insert(key.clone(), (e.to_string(), None));
                }
            }
        }
    }

    fn kick(self: &Arc<Self>) {
        let this = self.clone();
        thread::spawn(move || this.update());
    }

    /// A target's forecast, waiting up to WAIT for a first one.
    /// Err(None): still warming up. Err(Some): the last attempt failed.
    fn wait_for(self: &Arc<Self>, key: &str) -> std::result::Result<Arc<String>, Option<String>> {
        {
            let mut state = self.state.lock().unwrap();
            if let Some(json) = state.results.get(key) {
                return Ok(json.clone());
            }
            match state.errors.get(key) {
                // Failed under the current plan: it would fail again.
                Some((e, Some(_))) => return Err(Some(e.clone())),
                // A planning or download failure: worth another go.
                Some((_, None)) => {
                    state.errors.remove(key);
                }
                None => {}
            }
        }
        self.kick();
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            {
                let state = self.state.lock().unwrap();
                if let Some(json) = state.results.get(key) {
                    return Ok(json.clone());
                }
                if let Some((e, _)) = state.errors.get(key) {
                    return Err(Some(e.clone()));
                }
            }
            thread::sleep(StdDuration::from_millis(250));
        }
        Err(None)
    }

    fn named(
        self: &Arc<Self>,
        name: &str,
    ) -> Option<std::result::Result<Arc<String>, Option<String>>> {
        self.config
            .locations
            .iter()
            .any(|t| t.name.as_deref() == Some(name))
            .then(|| self.wait_for(&named_key(name)))
    }

    fn adhoc(
        self: &Arc<Self>,
        lat: f64,
        lon: f64,
    ) -> std::result::Result<Arc<String>, Option<String>> {
        let key = adhoc_key(lat, lon);
        let target = Target {
            name: None,
            lat: (lat * 1000.).round() / 1000.,
            lon: (lon * 1000.).round() / 1000.,
        };
        self.state
            .lock()
            .unwrap()
            .adhoc
            .insert(key.clone(), (target, Utc::now()));
        self.wait_for(&key)
    }

    fn refresh_loop(self: Arc<Self>) {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                let cutoff = Utc::now() - Duration::days(FORGET_AFTER_DAYS);
                let stale: Vec<String> = state
                    .adhoc
                    .iter()
                    .filter(|(_, (_, asked))| *asked < cutoff)
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in stale {
                    state.adhoc.remove(&key);
                    state.results.remove(&key);
                    state.errors.remove(&key);
                }
            }
            self.update();
            thread::sleep(REFRESH);
        }
    }
}

fn json_response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

/// (lat, lon) from a query string, if both are valid degrees.
fn parse_point(query: &str) -> Option<(f64, f64)> {
    let mut lat = None;
    let mut lon = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        match name {
            "lat" => lat = value.parse::<f64>().ok(),
            "lon" => lon = value.parse::<f64>().ok(),
            _ => {}
        }
    }
    let (lat, lon) = (lat?, lon?);
    ((-90. ..=90.).contains(&lat) && (-180. ..=180.).contains(&lon)).then_some((lat, lon))
}

fn forecast_response(
    result: std::result::Result<Arc<String>, Option<String>>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    match result {
        Ok(json) => json_response(200, json.as_str().to_string()),
        Err(None) => json_response(
            503,
            json!({"status": "warming", "error": "The first forecast is still downloading; try again shortly"})
                .to_string(),
        )
        .with_header(Header::from_bytes("Retry-After", "60").unwrap()),
        Err(Some(e)) => json_response(503, json!({"status": "error", "error": e}).to_string()),
    }
}

fn handle(forecaster: &Arc<Forecaster>, request: Request) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let not_found = || json_response(404, json!({"error": "not found"}).to_string());
    let response = match path {
        "/healthz" => json_response(200, json!({"status": "ok"}).to_string()),
        "/locations" => {
            let names: Vec<&str> = forecaster.config.locations.iter().filter_map(|t| t.name.as_deref()).collect();
            json_response(200, json!({"locations": names, "adhoc": forecaster.config.allow_adhoc}).to_string())
        }
        "/forecast" if !forecaster.config.allow_adhoc => json_response(
            403,
            json!({"error": "Ad-hoc points are turned off (ALLOW_ADHOC); ask for /forecast/<name>"}).to_string(),
        ),
        "/forecast" => match parse_point(query) {
            None => json_response(400, json!({"error": "lat and lon are required, in degrees"}).to_string()),
            Some((lat, lon)) => forecast_response(forecaster.adhoc(lat, lon)),
        },
        _ => match path.strip_prefix("/forecast/").and_then(|name| forecaster.named(name)) {
            Some(result) => forecast_response(result),
            None => not_found(),
        },
    };
    if let Err(e) = request.respond(response) {
        warn!("Couldn't answer {path}: {e}");
    }
}

pub fn serve(port: u16, forecaster: Forecaster) -> Result<()> {
    let forecaster = Arc::new(forecaster);
    let server = Server::http(("0.0.0.0", port))?;
    let names: Vec<&str> = forecaster
        .config
        .locations
        .iter()
        .filter_map(|t| t.name.as_deref())
        .collect();
    info!(
        "Serving on :{port}: locations [{}], ad-hoc points {}",
        names.join(", "),
        if forecaster.config.allow_adhoc {
            "on"
        } else {
            "off"
        }
    );
    let refresher = forecaster.clone();
    thread::spawn(move || refresher.refresh_loop());
    for request in server.incoming_requests() {
        let forecaster = forecaster.clone();
        thread::spawn(move || handle(&forecaster, request));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        assert_eq!(
            parse_point("lat=32.988&lon=-117.122"),
            Some((32.988, -117.122))
        );
        assert_eq!(
            parse_point("lon=-117.122&lat=32.988&x=1"),
            Some((32.988, -117.122))
        );
        assert_eq!(parse_point("lat=abc&lon=1"), None);
        assert_eq!(parse_point("lat=91&lon=0"), None);
        assert_eq!(parse_point("lat=32"), None);
        assert_eq!(parse_point(""), None);
    }
}
