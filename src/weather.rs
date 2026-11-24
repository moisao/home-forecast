//! What an hour's raw model fields come to: humidity, a UV index and Home
//! Assistant's weather condition.

/// %, from temperature and dewpoint, degC (Magnus, over water).
pub fn relative_humidity(t_c: f64, td_c: f64) -> f64 {
    let es = |t: f64| (17.625 * t / (t + 243.04)).exp();
    (100. * es(td_c) / es(t_c)).clamp(0., 100.)
}

/// UV index from the sun's height and the cloud -- an estimate: the
/// models publish no UV.
///
/// Clear sky: 12.5 mu^2.42 (Madronich 2007, for typical mid-latitude
/// ozone), mu being the cosine of the zenith angle. Cloud scales that by
/// how much of the clear-sky shortwave the model lets through
/// (solar / clear_sky_solar) to the 0.6, cloud dimming UV less than the
/// whole shortwave: on 2026-11-14 under 57 % cloud letting 11 % of the
/// shortwave through, the plain ratio gave UV 0.3 where Open-Meteo had
/// 0.85, and 0.11^0.6 gives 0.9. Without the shortwave, Kasten &
/// Czeplak's factor for the total cloud cover. In sun it is the clear-sky
/// value either way.
pub fn uv_index(
    mu: f64,
    solar: Option<f64>,
    clear_sky_solar: Option<f64>,
    cloud_pct: Option<f64>,
) -> f64 {
    if mu <= 0. {
        return 0.;
    }
    let clear = 12.5 * mu.powf(2.42);
    let factor = match (solar, clear_sky_solar, cloud_pct) {
        (Some(s), Some(c), _) if c > 20. => (s / c).max(0.).powf(0.6),
        (_, _, Some(cloud)) => 1. - 0.75 * (cloud / 100.).powf(3.4),
        _ => 1.,
    };
    clear * factor.clamp(0., 1.)
}

/// Home Assistant's weather condition for an hour.
pub fn condition(
    rain_mm: Option<f64>,
    cloud_pct: Option<f64>,
    wind_ms: f64,
    daylight: bool,
) -> &'static str {
    let rain = rain_mm.unwrap_or(0.);
    if rain >= 4. {
        return "pouring";
    }
    if rain >= 0.2 {
        return "rainy";
    }
    let cloud = cloud_pct.unwrap_or(0.);
    if wind_ms >= 10.8 {
        // Beaufort 6, a strong breeze.
        return if cloud > 75. {
            "windy-variant"
        } else {
            "windy"
        };
    }
    if cloud > 75. {
        "cloudy"
    } else if cloud > 25. {
        "partlycloudy"
    } else if daylight {
        "sunny"
    } else {
        "clear-night"
    }
}

/// A day's condition from its hours: rain first (pouring if any hour
/// pours, rainy at 1 mm or more over the day), then a strong breeze at any
/// hour, then the mean cloud while the sun is up.
pub fn daily_condition(
    rain_mm: Option<f64>,
    any_pouring: bool,
    daytime_cloud_pct: Option<f64>,
    max_wind_ms: f64,
) -> &'static str {
    if any_pouring {
        return "pouring";
    }
    if rain_mm.unwrap_or(0.) >= 1. {
        return "rainy";
    }
    let cloud = daytime_cloud_pct.unwrap_or(0.);
    if max_wind_ms >= 10.8 {
        return if cloud > 75. {
            "windy-variant"
        } else {
            "windy"
        };
    }
    if cloud > 75. {
        "cloudy"
    } else if cloud > 25. {
        "partlycloudy"
    } else {
        "sunny"
    }
}

pub fn round(value: f64, digits: i32) -> f64 {
    let scale = 10f64.powi(digits);
    (value * scale).round() / scale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_conditions() {
        assert_eq!(daily_condition(Some(0.4), false, Some(90.), 3.), "cloudy");
        assert_eq!(daily_condition(Some(2.), false, Some(10.), 3.), "rainy");
        assert_eq!(daily_condition(Some(2.), true, Some(10.), 3.), "pouring");
        assert_eq!(daily_condition(None, false, Some(40.), 12.), "windy");
        assert_eq!(daily_condition(None, false, None, 3.), "sunny");
    }

    #[test]
    fn humidity() {
        assert!((relative_humidity(20., 20.) - 100.).abs() < 1e-9);
        // 20 C over a 10 C dewpoint: 52.5 % by Magnus.
        assert!((relative_humidity(20., 10.) - 52.5).abs() < 0.1);
    }

    #[test]
    fn uv() {
        assert_eq!(uv_index(-0.1, None, None, None), 0.);
        let clear = uv_index(0.63, None, None, Some(0.));
        assert!((clear - 4.1).abs() < 0.1, "{clear}");
        // The 2026-11-14 13:00 case: shortwave 62 of a clear-sky 571.
        let cloudy = uv_index(0.58, Some(62.), Some(571.), Some(57.));
        assert!((cloudy - 0.85).abs() < 0.15, "{cloudy}");
    }

    #[test]
    fn conditions() {
        assert_eq!(condition(Some(5.), Some(100.), 1., true), "pouring");
        assert_eq!(condition(Some(0.3), Some(99.), 1., true), "rainy");
        assert_eq!(condition(Some(0.), Some(10.), 12., true), "windy");
        assert_eq!(condition(Some(0.), Some(90.), 12., true), "windy-variant");
        assert_eq!(condition(Some(0.), Some(90.), 1., true), "cloudy");
        assert_eq!(condition(Some(0.), Some(50.), 1., false), "partlycloudy");
        assert_eq!(condition(None, Some(5.), 1., true), "sunny");
        assert_eq!(condition(None, None, 1., false), "clear-night");
    }
}
