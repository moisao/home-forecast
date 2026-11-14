//! Which NOAA model run answers each hour, and fetching single GRIB2
//! messages out of it by byte range.
//!
//! Both models publish one GRIB2 file per (run, lead) on S3 with a
//! wgrib2-style `.idx` sidecar giving each message's byte offset, so a
//! field is a ~1-2 MB range request instead of a ~350 MB file. The files
//! are the whole CONUS grid: NOAA offers no area subsetting for these.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, DurationRound, Utc};

use crate::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Model {
    Rrfs,
    Hrrr,
}

/// Tried in this order: one model for all 24 hours, the first that has
/// every hour, else whichever has the most. GFS is left out on purpose:
/// at 0.25 degrees it is too coarse for one house.
pub const DEFAULT_MODELS: [Model; 2] = [Model::Rrfs, Model::Hrrr];

impl Model {
    pub fn parse(name: &str) -> Option<Model> {
        match name.to_ascii_lowercase().as_str() {
            "rrfs" => Some(Model::Rrfs),
            "hrrr" => Some(Model::Hrrr),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Model::Rrfs => "rrfs",
            Model::Hrrr => "hrrr",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Model::Rrfs => "RRFS 3 km",
            Model::Hrrr => "HRRR 3 km",
        }
    }

    /// The longest lead any run reaches (RRFS f84 and HRRR f48 from the
    /// 00/06/12/18Z cycles; shorter otherwise, which only the bucket says).
    pub fn max_lead(self) -> i64 {
        match self {
            Model::Rrfs => 84,
            Model::Hrrr => 48,
        }
    }

    /// The 2-D (surface) product of a run at a lead.
    pub fn grib_url(self, run: DateTime<Utc>, lead: i64) -> String {
        let (day, hour) = (run.format("%Y%m%d"), run.format("%H"));
        match self {
            Model::Rrfs => format!(
                "https://noaa-rrfs-ops-pds.s3.amazonaws.com/rrfs.{day}/{hour}/rrfs.t{hour}z.2dfld.3km.f{lead:03}.conus.grib2"
            ),
            Model::Hrrr => format!(
                "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.{day}/conus/hrrr.t{hour}z.wrfsfcf{lead:02}.grib2"
            ),
        }
    }
}

/// How a field is timed in the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Timing {
    /// As it is at the valid time ("6 hour fcst", "anl").
    Instant,
    /// Total over the hour ending at the valid time ("5-6 hour acc fcst").
    HourTotal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Field {
    pub var: &'static str,
    /// Matched as a prefix: HRRR and RRFS spell whole-column levels
    /// differently ("entire atmosphere" vs "entire atmosphere (considered
    /// as a single layer)").
    pub level: &'static str,
    pub timing: Timing,
}

impl Field {
    pub const fn new(var: &'static str, level: &'static str, timing: Timing) -> Field {
        Field { var, level, timing }
    }

    pub fn key(&self) -> String {
        format!("{}:{}", self.var, self.level)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdxEntry {
    pub var: String,
    pub level: String,
    pub timing: String,
    pub start: u64,
    /// Inclusive; None for the file's last message.
    pub end: Option<u64>,
}

/// Parses a wgrib2 inventory: `n:start:d=YYYYMMDDHH:VAR:LEVEL:TIMING:...`.
pub fn parse_idx(text: &str) -> Vec<IdxEntry> {
    let mut entries: Vec<IdxEntry> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(7, ':');
            let _n = parts.next()?;
            let start = parts.next()?.parse().ok()?;
            let _date = parts.next()?;
            Some(IdxEntry {
                var: parts.next()?.to_string(),
                level: parts.next()?.to_string(),
                timing: parts.next()?.to_string(),
                start,
                end: None,
            })
        })
        .collect();
    for i in 1..entries.len() {
        let next = entries[i].start;
        entries[i - 1].end = Some(next - 1);
    }
    entries
}

/// The index entry for a field at this lead, if the file has it.
pub fn find<'a>(entries: &'a [IdxEntry], field: &Field, lead: i64) -> Option<&'a IdxEntry> {
    entries.iter().find(|e| {
        e.var == field.var
            && e.level.starts_with(field.level)
            && match field.timing {
                Timing::Instant => {
                    e.timing == format!("{lead} hour fcst") || (lead == 0 && e.timing == "anl")
                }
                Timing::HourTotal => {
                    lead >= 1 && e.timing == format!("{}-{lead} hour acc fcst", lead - 1)
                }
            }
    })
}

/// A run that hasn't reached a lead hasn't reached any later one either:
/// one miss answers for the rest, until it ages out (a run still being
/// posted reaches further by then).
const RUN_MISS_TTL: StdDuration = StdDuration::from_secs(600);
/// How many runs back to look for one valid time.
const MAX_STEP_BACK: i64 = 12;

/// (valid, run, lead) for each hour a model answers, in time order.
pub type Resolved = Vec<(DateTime<Utc>, DateTime<Utc>, i64)>;
type Index = Arc<Vec<IdxEntry>>;

pub struct Fetcher {
    agent: ureq::Agent,
    indexes: Mutex<HashMap<(Model, i64, i64), Index>>,
    missing_from: Mutex<HashMap<(Model, i64), (i64, Instant)>>,
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher {
    pub fn new() -> Fetcher {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(StdDuration::from_secs(60)))
            .user_agent("home-forecast")
            .build()
            .into();
        Fetcher {
            agent,
            indexes: Mutex::new(HashMap::new()),
            missing_from: Mutex::new(HashMap::new()),
        }
    }

    /// GET with a few retries on server errors and dropped connections.
    /// Ok(None) for a file that isn't there (S3 says 403 or 404).
    fn get(&self, url: &str, range: Option<String>) -> Result<Option<Vec<u8>>> {
        let mut last_error = None;
        for attempt in 0..4 {
            if attempt > 0 {
                thread::sleep(StdDuration::from_millis(500 << attempt));
            }
            let mut request = self.agent.get(url);
            if let Some(range) = &range {
                request = request.header("Range", range);
            }
            match request.call() {
                Ok(mut response) => {
                    match response
                        .body_mut()
                        .with_config()
                        .limit(64 << 20)
                        .read_to_vec()
                    {
                        Ok(body) => return Ok(Some(body)),
                        Err(e) => last_error = Some(e.to_string()),
                    }
                }
                Err(ureq::Error::StatusCode(403 | 404)) => return Ok(None),
                Err(ureq::Error::StatusCode(code)) if code < 500 && code != 429 => {
                    return Err(format!("{url}: HTTP {code}").into());
                }
                Err(e) => last_error = Some(e.to_string()),
            }
        }
        Err(format!("{url}: {}", last_error.unwrap_or_default()).into())
    }

    /// The index of a run's file at a lead, or None if it isn't posted.
    /// A posted index stays posted, so it is kept; the GRIB2 goes up before
    /// its index, so an index means the data is there.
    pub fn index(&self, model: Model, run: DateTime<Utc>, lead: i64) -> Result<Option<Index>> {
        let key = (model, run.timestamp(), lead);
        if let Some(entries) = self.indexes.lock().unwrap().get(&key) {
            return Ok(Some(entries.clone()));
        }
        if let Some(&(missing_lead, when)) = self.missing_from.lock().unwrap().get(&(model, key.1))
            && lead >= missing_lead
            && when.elapsed() < RUN_MISS_TTL
        {
            return Ok(None);
        }
        let url = format!("{}.idx", model.grib_url(run, lead));
        match self.get(&url, None)? {
            Some(body) => {
                let entries = Arc::new(parse_idx(&String::from_utf8_lossy(&body)));
                let mut indexes = self.indexes.lock().unwrap();
                if indexes.len() > 2000 {
                    indexes.clear();
                }
                indexes.insert(key, entries.clone());
                Ok(Some(entries))
            }
            None => {
                let mut missing = self.missing_from.lock().unwrap();
                let entry = missing
                    .entry((model, key.1))
                    .or_insert((lead, Instant::now()));
                if lead < entry.0 || entry.1.elapsed() >= RUN_MISS_TTL {
                    *entry = (lead, Instant::now());
                }
                Ok(None)
            }
        }
    }

    /// One message's bytes.
    pub fn message(
        &self,
        model: Model,
        run: DateTime<Utc>,
        lead: i64,
        entry: &IdxEntry,
    ) -> Result<Vec<u8>> {
        let range = match entry.end {
            Some(end) => format!("bytes={}-{end}", entry.start),
            None => format!("bytes={}-", entry.start),
        };
        self.get(&model.grib_url(run, lead), Some(range))?
            .ok_or_else(|| {
                format!("{} {run} f{lead:02} vanished from the bucket", model.name()).into()
            })
    }

    /// The newest run with its analysis posted, stepping back from now.
    pub fn latest_run(&self, model: Model, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        let mut run = now.duration_trunc(Duration::hours(1))?;
        for _ in 0..MAX_STEP_BACK {
            if self.index(model, run, 0)?.is_some() {
                return Ok(run);
            }
            run -= Duration::hours(1);
        }
        Err(format!("No recent {} run found", model.name().to_uppercase()).into())
    }

    /// {valid time: (run, lead)} for every hour this model can answer: the
    /// freshest run that reaches each hour -- a past hour from the run at
    /// that hour, as its own analysis -- else an older run at a longer
    /// lead, so the valid time never moves, the forecast only ages. Hours
    /// no run within reach covers are left out.
    pub fn resolve(
        &self,
        model: Model,
        valid_times: &[DateTime<Utc>],
        now: DateTime<Utc>,
    ) -> Result<Resolved> {
        let Some(&first) = valid_times.iter().min() else {
            return Ok(vec![]);
        };
        if first - now > Duration::hours(model.max_lead()) {
            return Ok(vec![]);
        }
        let latest = self.latest_run(model, now)?;
        // Every hour's walk at once: each step is a round trip to the
        // bucket, and in turn a day of them took seconds.
        type Answer = Result<Option<(DateTime<Utc>, i64)>>;
        let answers: Vec<Answer> = thread::scope(|scope| {
            let handles: Vec<_> = valid_times
                .iter()
                .map(|&valid| {
                    scope.spawn(move || {
                        let mut run = latest.min(valid);
                        for _ in 0..MAX_STEP_BACK {
                            let lead = (valid - run).num_hours();
                            if (0..=model.max_lead()).contains(&lead)
                                && self.index(model, run, lead)?.is_some()
                            {
                                return Ok(Some((run, lead)));
                            }
                            run -= Duration::hours(1);
                        }
                        Ok(None)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut resolved = Vec::new();
        for (&valid, answer) in valid_times.iter().zip(answers) {
            if let Some((run, lead)) = answer? {
                resolved.push((valid, run, lead));
            }
        }
        resolved.sort();
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const IDX: &str = "\
1:0:d=2026111418:REFC:entire atmosphere (considered as a single layer):6 hour fcst:
2:1000:d=2026111418:TMP:2 m above ground:6 hour fcst:
3:2500:d=2026111418:APCP:surface:0-6 hour acc fcst:
4:2600:d=2026111418:APCP:surface:5-6 hour acc fcst:
5:2700:d=2026111418:TCDC:entire atmosphere (considered as a single layer):5-6 hour ave fcst:
6:3700:d=2026111418:TCDC:entire atmosphere (considered as a single layer):6 hour fcst:
";

    #[test]
    fn parses_offsets() {
        let entries = parse_idx(IDX);
        assert_eq!(entries.len(), 6);
        assert_eq!((entries[1].start, entries[1].end), (1000, Some(2499)));
        assert_eq!(entries[5].end, None);
    }

    #[test]
    fn picks_instant_and_hour_total() {
        let entries = parse_idx(IDX);
        let tmp = find(
            &entries,
            &Field::new("TMP", "2 m above ground", Timing::Instant),
            6,
        )
        .unwrap();
        assert_eq!(tmp.start, 1000);
        let rain = find(
            &entries,
            &Field::new("APCP", "surface", Timing::HourTotal),
            6,
        )
        .unwrap();
        assert_eq!(rain.start, 2600);
        let cloud = find(
            &entries,
            &Field::new("TCDC", "entire atmosphere", Timing::Instant),
            6,
        )
        .unwrap();
        assert_eq!(cloud.start, 3700);
        assert!(
            find(
                &entries,
                &Field::new("TMP", "2 m above ground", Timing::Instant),
                5
            )
            .is_none()
        );
    }

    #[test]
    fn urls() {
        let run = Utc.with_ymd_and_hms(2026, 11, 14, 18, 0, 0).unwrap();
        assert_eq!(
            Model::Rrfs.grib_url(run, 6),
            "https://noaa-rrfs-ops-pds.s3.amazonaws.com/rrfs.20261114/18/rrfs.t18z.2dfld.3km.f006.conus.grib2"
        );
        assert_eq!(
            Model::Hrrr.grib_url(run, 6),
            "https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20261114/conus/hrrr.t18z.wrfsfcf06.grib2"
        );
    }
}
