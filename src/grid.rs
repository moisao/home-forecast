//! One value out of a whole-grid GRIB2 message: the grid point nearest a
//! latitude/longitude on a Lambert conformal grid (RRFS, HRRR), found by
//! projecting the point onto the grid rather than searching it.

use grib::GridDefinitionTemplateValues;
use grib::def::grib2::template::Template3_30;
use grib::projection::{Ellipsoid, Lcc, LccParams, Project};

use crate::Result;

/// Where a point falls on a model's grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridPoint {
    /// Position of the value in the message, in scanning order.
    pub index: usize,
    pub lat: f64,
    pub lon: f64,
    /// Degrees to turn grid-relative winds to true north (0 if the
    /// message's winds are already earth-relative).
    pub wind_rotation: f64,
}

const MICRO: f64 = 1e-6;

fn signed_lon(lon: f64) -> f64 {
    (lon + 180.).rem_euclid(360.) - 180.
}

/// The grid point nearest (lat, lon), or an error if it is off the grid.
pub fn locate(t: &Template3_30, lat: f64, lon: f64) -> Result<GridPoint> {
    let (a, b) = t
        .earth_shape
        .radii()
        .ok_or_else(|| format!("unknown shape of the earth {}", t.earth_shape.shape))?;
    let latin1 = t.latin1 as f64 * MICRO;
    let latin2 = t.latin2 as f64 * MICRO;
    let lov = t.lov as f64 * MICRO;
    let projection = Lcc::new(&LccParams {
        ellipsoid: Ellipsoid::from_a_and_b(a, b),
        lat_0: t.lad as f64 * MICRO,
        lon_0: lov,
        lat_1: latin1,
        lat_2: latin2,
    })?;

    let scanning = &t.scanning_mode;
    if !scanning.is_consecutive_for_i() || scanning.scans_alternating_rows() {
        return Err(format!("unsupported scanning mode {:#010b}", scanning.0).into());
    }
    // Grid spacing in metres, negative along an axis scanned backwards
    // (as grib's own Lambert lat/lon computation does).
    let dx = t.dx as f64
        * 1e-3
        * if scanning.scans_positively_for_i() {
            1.
        } else {
            -1.
        };
    let dy = t.dy as f64
        * 1e-3
        * if scanning.scans_positively_for_j() {
            1.
        } else {
            -1.
        };

    let first = (
        (t.first_point_lon as f64 * MICRO).to_radians(),
        (t.first_point_lat as f64 * MICRO).to_radians(),
    );
    let (x0, y0) = projection.project(&first, false)?;
    let (x, y) = projection.project(&(lon.to_radians(), lat.to_radians()), false)?;
    let i = ((x - x0) / dx).round();
    let j = ((y - y0) / dy).round();
    let (ni, nj) = (t.ni as usize, t.nj as usize);
    if i < 0. || j < 0. || i >= ni as f64 || j >= nj as f64 {
        return Err(format!("{lat:.3}, {lon:.3} is outside the model's grid").into());
    }
    let (i, j) = (i as usize, j as usize);
    let (grid_lon, grid_lat) =
        projection.project(&(x0 + dx * i as f64, y0 + dy * j as f64), true)?;
    let (grid_lat, grid_lon) = (grid_lat.to_degrees(), signed_lon(grid_lon.to_degrees()));

    // Flag table 3.3, bit 5: u/v relative to the grid's x/y, not east/north.
    let grid_relative = t.resolution_and_component_flags.0 & 0b0000_1000 != 0;
    let wind_rotation = if grid_relative {
        cone_constant(latin1, latin2) * signed_lon(grid_lon - lov)
    } else {
        0.
    };
    Ok(GridPoint {
        index: j * ni + i,
        lat: grid_lat,
        lon: grid_lon,
        wind_rotation,
    })
}

/// The Lambert cone constant for a spherical earth.
fn cone_constant(latin1: f64, latin2: f64) -> f64 {
    let (p1, p2) = (latin1.to_radians(), latin2.to_radians());
    if (p1 - p2).abs() < 1e-9 {
        return p1.sin();
    }
    let quarter = std::f64::consts::FRAC_PI_4;
    (p1.cos() / p2.cos()).ln() / ((quarter + p2 / 2.).tan() / (quarter + p1 / 2.).tan()).ln()
}

/// The values at several points in a single-message GRIB2 blob, in their
/// order, and where on the grid each came from: one decode however many
/// points (the whole-grid unpack is the work; picking values out of it is
/// nothing). None for a point off the grid; NaN where the bitmap masks it.
pub fn values_at(message: Vec<u8>, points: &[(f64, f64)]) -> Result<Vec<Option<(f64, GridPoint)>>> {
    let grib2 = grib::from_bytes(message)?;
    let (_, submessage) = grib2.iter().next().ok_or("empty GRIB2 message")?;
    let located: Vec<Option<GridPoint>> =
        match GridDefinitionTemplateValues::try_from(submessage.grid_def())? {
            GridDefinitionTemplateValues::Template30(t) => points
                .iter()
                .map(|&(lat, lon)| locate(&t, lat, lon).ok())
                .collect(),
            _ => return Err("not a Lambert conformal grid".into()),
        };
    let mut wanted: Vec<usize> = located.iter().flatten().map(|p| p.index).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut found = std::collections::HashMap::with_capacity(wanted.len());
    if let Some(&last) = wanted.last() {
        let decoder = grib::Grib2SubmessageDecoder::from(submessage)?;
        let mut next = wanted.iter().peekable();
        for (i, value) in decoder.dispatch()?.enumerate().take(last + 1) {
            if next.peek() == Some(&&i) {
                found.insert(i, value as f64);
                next.next();
            }
        }
        if found.len() != wanted.len() {
            return Err("message has fewer values than its grid".into());
        }
    }
    Ok(located
        .into_iter()
        .map(|p| p.map(|p| (found[&p.index], p)))
        .collect())
}

/// (speed m/s, bearing the wind blows from, degrees) from u/v, turned off
/// the grid by `rotation` degrees where the grid isn't north-up.
pub fn earth_wind(u: f64, v: f64, rotation: f64) -> (f64, f64) {
    let angle = rotation.to_radians();
    let (sin, cos) = angle.sin_cos();
    let (u, v) = (v * sin + u * cos, v * cos - u * sin);
    (u.hypot(v), (-u).atan2(-v).to_degrees().rem_euclid(360.))
}

#[cfg(test)]
mod tests {
    use super::*;
    use grib::def::grib2::template::param_set::{
        EarthShape, ProjectionCentreFlag, ResolutionAndComponentFlags, ScaledValue, ScanningMode,
    };

    /// RRFS's CONUS grid as its 2dfld files describe it (2026-11-14 18Z).
    fn rrfs_grid() -> Template3_30 {
        Template3_30 {
            earth_shape: EarthShape {
                shape: 6,
                spherical_earth_radius: ScaledValue {
                    scale_factor: 0,
                    scaled_value: 0,
                },
                major_axis: ScaledValue {
                    scale_factor: 0,
                    scaled_value: 0,
                },
                minor_axis: ScaledValue {
                    scale_factor: 0,
                    scaled_value: 0,
                },
            },
            ni: 1799,
            nj: 1059,
            first_point_lat: 21_138_123,
            first_point_lon: 237_280_472,
            resolution_and_component_flags: ResolutionAndComponentFlags(0b0011_1000),
            lad: 38_500_000,
            lov: 262_500_000,
            dx: 3_000_000,
            dy: 3_000_000,
            projection_centre: ProjectionCentreFlag(0),
            scanning_mode: ScanningMode(0b0100_0000),
            latin1: 38_500_000,
            latin2: 38_500_000,
            south_pole_lat: -90_000_000,
            south_pole_lon: 0,
        }
    }

    #[test]
    fn first_point_is_index_zero() {
        let point = locate(&rrfs_grid(), 21.138123, -122.719528).unwrap();
        assert_eq!(point.index, 0);
        assert!((point.lat - 21.138123).abs() < 1e-4 && (point.lon + 122.719528).abs() < 1e-4);
    }

    #[test]
    fn nearest_point_matches_eccodes() {
        // eccodes' nearest grid point to Little Black (32.988, -117.122) on
        // this grid: index 701901 at 32.99966, -117.12433.
        let point = locate(&rrfs_grid(), 32.988, -117.122).unwrap();
        assert_eq!(point.index, 701_901, "{point:?}");
        assert!(
            (point.lat - 32.99966).abs() < 1e-4 && (point.lon + 117.12433).abs() < 1e-4,
            "{point:?}"
        );
        // West of LoV (262.5 = -97.5), so grid north leans east of true north.
        assert!(point.wind_rotation < 0.);
    }

    /// Decodes a real message when HF_TEST_GRIB names one: RRFS 2026-11-14
    /// 18Z f06 TMP at 2 m, where eccodes reads 290.89 K at Little Black.
    #[test]
    fn decodes_a_real_message() {
        let Ok(path) = std::env::var("HF_TEST_GRIB") else {
            return;
        };
        // Little Black, the same point again, and one off the grid.
        let points = [(32.988, -117.122), (32.988, -117.122), (51.5, -0.1)];
        let values = values_at(std::fs::read(path).unwrap(), &points).unwrap();
        let (value, point) = values[0].unwrap();
        assert_eq!(point.index, 701_901);
        assert!((value - 290.89).abs() < 0.01, "{value}");
        assert_eq!(values[1], values[0]);
        assert!(values[2].is_none());
    }

    #[test]
    fn off_grid_is_an_error() {
        assert!(locate(&rrfs_grid(), 51.5, -0.1).is_err());
    }

    #[test]
    fn wind_bearing() {
        // A wind from the south (blowing north) with no rotation.
        let (speed, bearing) = earth_wind(0., 5., 0.);
        assert!((speed - 5.).abs() < 1e-9 && (bearing - 180.).abs() < 1e-9);
        // From the west.
        let (_, bearing) = earth_wind(5., 0., 0.);
        assert!((bearing - 270.).abs() < 1e-9);
    }
}
