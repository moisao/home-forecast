//! Named locations from the LOCATIONS environment variable:
//!
//!     LOCATIONS="home=32.988,-117.122; mt-laguna=32.87,-116.42"
//!
//! Entries are separated by `;` or newlines; a name is lowercase letters,
//! digits, `-` and `_`. Each is served at `/forecast/<name>` and kept current
//! in the background, all of them from one download per model run.

use crate::Result;
use crate::forecast::Target;

pub fn parse(spec: &str) -> Result<Vec<Target>> {
    let mut targets: Vec<Target> = Vec::new();
    // Errors say which entry by position, never what it holds: a typo'd
    // entry is still somebody's coordinates, and errors end up in logs.
    for (n, entry) in spec
        .split([';', '\n'])
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .enumerate()
    {
        let bad = || format!("LOCATIONS: entry {} isn't name=lat,lon", n + 1);
        let (name, coords) = entry.split_once('=').ok_or_else(bad)?;
        let (lat, lon) = coords.split_once(',').ok_or_else(bad)?;
        let name = name.trim();
        let (lat, lon): (f64, f64) = (
            lat.trim().parse().map_err(|_| bad())?,
            lon.trim().parse().map_err(|_| bad())?,
        );
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(
                format!("LOCATIONS: entry {}'s name isn't a-z, 0-9, - and _", n + 1).into(),
            );
        }
        if !(-90. ..=90.).contains(&lat) || !(-180. ..=180.).contains(&lon) {
            return Err(format!("LOCATIONS: {name}'s coordinates are out of range").into());
        }
        if targets.iter().any(|t| t.name.as_deref() == Some(name)) {
            return Err(format!("LOCATIONS: {name} is given twice").into());
        }
        targets.push(Target {
            name: Some(name.to_string()),
            lat,
            lon,
        });
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_list() {
        let targets = parse(" little-black=32.988,-117.122;\n blossom = 33.03, -116.89 ;").unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0],
            Target {
                name: Some("little-black".into()),
                lat: 32.988,
                lon: -117.122
            }
        );
        assert_eq!((targets[1].lat, targets[1].lon), (33.03, -116.89));
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn rejects_bad_entries() {
        for spec in [
            "home",
            "home=1",
            "home=a,b",
            "Home=1,2",
            "=1,2",
            "h=91,0",
            "h=0,181",
            "h=1,2;h=3,4",
        ] {
            assert!(parse(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn errors_never_echo_coordinates() {
        for spec in [
            "home=95.123,-117.456",
            "home=32.123;-117.456",
            "Home=32.123,-117.456",
            "home 32.123,-117.456",
        ] {
            let e = parse(spec).unwrap_err().to_string();
            assert!(
                !e.contains("32.123") && !e.contains("95.123") && !e.contains("117.456"),
                "{e}"
            );
        }
    }
}
