//! The sun: NOAA's low-accuracy solar equations, good to a minute or two
//! at these latitudes and no network call to be down.

use std::f64::consts::PI;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};

/// (equation of time, minutes; declination, radians) for fractional year
/// `gamma`.
fn solar_terms(gamma: f64) -> (f64, f64) {
    let eqtime = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2. * gamma).cos()
            - 0.040849 * (2. * gamma).sin());
    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2. * gamma).cos()
        + 0.000907 * (2. * gamma).sin()
        - 0.002697 * (3. * gamma).cos()
        + 0.00148 * (3. * gamma).sin();
    (eqtime, decl)
}

/// (sunrise, sunset) on a calendar day at (lat, lon), as UTC times, or
/// None where the sun doesn't do both (polar day or night).
pub fn sun_times(lat: f64, lon: f64, day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    // Fraction of the year at this longitude's solar noon, in UTC.
    let gamma = 2. * PI / 365. * (day.ordinal0() as f64 - lon / 360.);
    let (eqtime, decl) = solar_terms(gamma);
    // 90.833 deg: the sun's upper limb on the horizon, with refraction.
    let phi = lat.to_radians();
    let cos_ha = 90.833_f64.to_radians().cos() / (phi.cos() * decl.cos()) - phi.tan() * decl.tan();
    if !(-1. ..=1.).contains(&cos_ha) {
        return None;
    }
    let ha = cos_ha.acos().to_degrees();
    let midnight = Utc.from_utc_datetime(&day.and_hms_opt(0, 0, 0)?);
    let at = |minutes: f64| midnight + Duration::milliseconds((minutes * 60_000.).round() as i64);
    Some((
        at(720. - 4. * (lon + ha) - eqtime),
        at(720. - 4. * (lon - ha) - eqtime),
    ))
}

/// Cosine of the sun's zenith angle at (lat, lon) at time t; negative
/// with the sun below the horizon.
pub fn cos_zenith(lat: f64, lon: f64, t: DateTime<Utc>) -> f64 {
    let hour = t.hour() as f64 + t.minute() as f64 / 60.;
    let gamma = 2. * PI / 365. * (t.ordinal0() as f64 + (hour - 12.) / 24.);
    let (eqtime, decl) = solar_terms(gamma);
    let true_solar_minutes =
        t.hour() as f64 * 60. + t.minute() as f64 + t.second() as f64 / 60. + eqtime + 4. * lon;
    let hour_angle = (true_solar_minutes / 4. - 180.).to_radians();
    let phi = lat.to_radians();
    phi.sin() * decl.sin() + phi.cos() * decl.cos() * hour_angle.cos()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn little_black_mid_november() {
        // As the Python version (point_forecast.sun_times) has it:
        // 2026-11-14 at 32.988, -117.122: 06:18:02 and 16:48:25 PST.
        let (rise, set) = sun_times(
            32.988,
            -117.122,
            NaiveDate::from_ymd_opt(2026, 11, 14).unwrap(),
        )
        .unwrap();
        let expect_rise = Utc.with_ymd_and_hms(2026, 11, 14, 14, 18, 2).unwrap();
        let expect_set = Utc.with_ymd_and_hms(2026, 11, 15, 0, 48, 25).unwrap();
        assert!((rise - expect_rise).num_seconds().abs() <= 1, "{rise}");
        assert!((set - expect_set).num_seconds().abs() <= 1, "{set}");
    }

    #[test]
    fn polar_night() {
        assert!(sun_times(80., 0., NaiveDate::from_ymd_opt(2026, 12, 21).unwrap()).is_none());
    }

    #[test]
    fn zenith_day_and_night() {
        let noon = Utc.with_ymd_and_hms(2026, 11, 14, 19, 45, 0).unwrap(); // ~solar noon
        let midnight = Utc.with_ymd_and_hms(2026, 11, 15, 7, 45, 0).unwrap();
        let mu = cos_zenith(32.988, -117.122, noon);
        // Noon zenith ~51 deg in mid-November at 33 N.
        assert!((mu - 51_f64.to_radians().cos()).abs() < 0.02, "{mu}");
        assert!(cos_zenith(32.988, -117.122, midnight) < 0.);
    }
}
