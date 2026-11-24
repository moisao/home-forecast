# home-forecast

The next three days, hour by hour and by day, at the places you name, from NOAA's
[RRFS](https://registry.opendata.aws/noaa-rrfs/) 3 km model, with clouds and
rain blended with [HRRR](https://registry.opendata.aws/noaa-hrrr-pds/)'s (and
HRRR as a fallback), served as JSON. It's
made to feed a [Home Assistant](https://www.home-assistant.io/) weather entity.

For each hour:

| Field | From |
|---|---|
| `temperature_c`, `dew_point_c` | 2 m temperature and dewpoint |
| `humidity` | from those two (Magnus) |
| `wind_speed_ms`, `wind_bearing`, `wind_gust_ms` | 10 m wind, surface gust |
| `precipitation_mm` | the total for the hour starting at `time`, averaged over 10 km and both models |
| `precipitation_probability` | the share of that area, in both models, getting at least 0.01 in (0.25 mm) that hour, % |
| `cloud_coverage` | total cloud, %, averaged over 10 km and both models |
| `uv_index` | **estimated**: the models publish none (see below) |
| `condition` | a Home Assistant condition (`sunny`, `partlycloudy`, `rainy`, ...) |
| `daylight` | whether the sun is up |

For each local day (today's from the hours left in it, later days only
when the forecast covers all 24 of their hours), under `days`: the high
and low, the day's rain total and its highest hourly chance, the
strongest hour's wind and its direction, the strongest gust, mean
humidity, mean cloud while the sun is up, the highest UV, and a condition.

Plus today's and tomorrow's sunrise and sunset. Coverage is the contiguous
US (CONUS), where RRFS and HRRR run.

## How it works

The models publish one GRIB2 file per run and forecast hour, each the whole
CONUS grid (~350 MB), with an index of where each field sits in it. For each
hour the service fetches just the fields it needs by HTTP range request,
about 10 MB, from NOAA's public S3 buckets. It decodes each field once,
picks out every configured location's value, and keeps only those numbers.
The cost depends on how far ahead you ask (`FORECAST_HOURS`) and not on
how many locations there are: about 11 MB per hour of forecast. RRFS's
00/06/12/18Z runs reach 84 hours, so each one refetches the whole forecast
(about 800 MB for 72 hours). The runs between them reach 18 hours. No API
keys and no third-party weather service are involved.

Each hour comes from the freshest run that reaches it. Temperature,
humidity and wind for every hour come from one model, since a switch of
model partway through would read as a change in the weather.

Clouds and rain are where a 3 km model is least reliable at a single grid
point: it often gets a shower right within a few kilometres and an hour
but not at the point itself, and one model often keeps a shower that the
other drops between runs. So they are read differently:

- **Over an area**: the mean over every grid point within 10 km (37
  points), not the one nearest.
- **From both models**: the other model's clouds and rain (HRRR's, with
  RRFS as the main model) are averaged in where it has the hour. HRRR runs
  hourly, so this also picks up newer data between RRFS runs. HRRR reaches
  48 hours, so hours after that are RRFS's alone. A new HRRR run costs about
  1 MB per hour it covers.
- **Chance of rain**: the share of those grid points, across both models,
  that get measurable rain in the hour.

Set `MODEL` to use one model alone, without the blend.

Each hour's rain is the hour *starting* at its time, as Home Assistant
reads a forecast hour: the 6 pm entry is the rain from 6 to 7 pm.

## Running it

```bash
docker run --rm -p 8080:8080 \
  -e LOCATIONS="little-black=32.988,-117.122; mt-laguna=32.87,-116.42" \
  ghcr.io/moisao/home-forecast:latest
```

```bash
curl -s localhost:8080/forecast/little-black
```

### Configuration (environment)

| Variable | |
|---|---|
| `LOCATIONS` | `name=lat,lon` entries separated by `;` or newlines. Names are `a-z`, `0-9`, `-`, `_`. |
| `ALLOW_ADHOC` | `true` to also answer `/forecast?lat=..&lon=..` for any point. Off by default: on a shared instance, anyone could make it download and decode new points. |
| `FORECAST_HOURS` | how far ahead, `1` to `84`, default `72` |
| `PORT` | default `8080` |
| `TZ` | the zone times are given in, default `America/Los_Angeles` |
| `CACHE_DIR` | keep decoded values here, so a restart doesn't download again |
| `MODEL` | `rrfs` or `hrrr` alone, instead of RRFS then HRRR with the other's clouds and rain blended in |
| `RUST_LOG` | default `info` |

Your locations are probably where you live. Keep them in a secret store
(a Kubernetes Secret via `envFrom`, a Docker env file kept out of version
control), not in a committed compose file. The service never returns a named
location's coordinates, `/locations` lists names only, and logs never print
coordinates. A malformed `LOCATIONS` entry is reported by its position, not
its contents.

### Endpoints

| | |
|---|---|
| `GET /forecast/<name>` | a configured location |
| `GET /forecast?lat=..&lon=..` | any point, with `ALLOW_ADHOC` (the response then includes the coordinates and grid point) |
| `GET /locations` | `{"locations": [names], "adhoc": bool}` |
| `GET /healthz` | liveness |

The configured locations are forecast at startup, then whenever a newer run
posts (checked every 5 minutes). A request that arrives before a location's
first forecast is ready waits up to 20 s, then gets `503`
`{"status": "warming"}` with `Retry-After`. A location outside the model's
grid gets `503` with the reason. It is retried with each new run, not on
every request.

### Response

```json
{
  "location": "little-black",
  "model": "RRFS 3 km",
  "generated": "2026-11-14T14:23:46-08:00",
  "runs": ["2026-11-14T18:00:00+00:00", "2026-11-14T21:00:00+00:00"],
  "blend_model": "HRRR 3 km",
  "blend_runs": ["2026-11-14T21:00:00+00:00", "2026-11-14T22:00:00+00:00"],
  "downloaded_mb": 0.0,
  "sunrise": "2026-11-14T06:18:01-08:00",
  "sunset": "2026-11-14T16:48:25-08:00",
  "sunrise_tomorrow": "2026-11-15T06:18:58-08:00",
  "sunset_tomorrow": "2026-11-15T16:47:50-08:00",
  "next_sunrise": "2026-11-15T06:18:58-08:00",
  "next_sunset": "2026-11-14T16:48:25-08:00",
  "hours": [
    {
      "time": "2026-11-14T14:00:00-08:00",
      "run": "2026-11-14T21:00:00+00:00",
      "temperature_c": 17.2,
      "dew_point_c": 14.4,
      "humidity": 84.0,
      "wind_speed_ms": 2.3,
      "wind_gust_ms": 4.0,
      "wind_bearing": 245.0,
      "precipitation_mm": 0.31,
      "precipitation_probability": 40.0,
      "cloud_coverage": 99.0,
      "uv_index": 0.9,
      "daylight": true,
      "condition": "rainy"
    }
  ],
  "days": [
    {
      "date": "2026-11-14T00:00:00-08:00",
      "temperature_high_c": 18.1,
      "temperature_low_c": 13.9,
      "precipitation_mm": 2.4,
      "precipitation_probability": 70.0,
      "wind_speed_ms": 4.6,
      "wind_bearing": 250.0,
      "wind_gust_ms": 8.2,
      "humidity": 86.0,
      "cloud_coverage": 97.0,
      "uv_index": 1.2,
      "condition": "rainy"
    }
  ]
}
```

The first hour is the current one. `downloaded_mb` is what the update that
produced this response fetched, shared by every location in it.

## Home Assistant

A REST sensor holding the hours and days, and a template weather entity
built from it. Home Assistant converts the units for display. Three days of
hours is ~25 KB, past the recorder's 16 KB limit for attributes, so the
sensor is left out of the recorder.

```yaml
recorder:
  exclude:
    entities: [sensor.home_forecast_updated]

rest:
  - resource: http://home-forecast:8080/forecast/home
    scan_interval: 300
    timeout: 30
    sensor:
      - name: Home forecast updated
        unique_id: home_forecast_updated
        device_class: timestamp
        value_template: "{{ value_json.generated }}"
        json_attributes: [model, runs, blend_model, blend_runs, sunrise, sunset, next_sunrise, next_sunset, hours, days]

template:
  - weather:
      - name: Home
        unique_id: home_forecast_weather
        condition: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{'condition': 'unknown'}])[0].condition }}"
        temperature: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].temperature_c | default(none) }}"
        humidity: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].humidity | default(none) }}"
        dew_point: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].dew_point_c | default(none) }}"
        wind_speed: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].wind_speed_ms | default(none) }}"
        wind_gust_speed: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].wind_gust_ms | default(none) }}"
        wind_bearing: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].wind_bearing | default(none) }}"
        cloud_coverage: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].cloud_coverage | default(none) }}"
        uv_index: "{{ (state_attr('sensor.home_forecast_updated', 'hours') or [{}])[0].uv_index | default(none) }}"
        temperature_unit: "°C"
        wind_speed_unit: "m/s"
        precipitation_unit: "mm"
        forecast_hourly: >-
          {% set ns = namespace(out=[]) %}
          {% for h in state_attr('sensor.home_forecast_updated', 'hours') or [] %}
            {% set ns.out = ns.out + [{'datetime': h.time, 'condition': h.condition,
              'is_daytime': h.daylight, 'temperature': h.temperature_c, 'dew_point': h.dew_point_c,
              'humidity': h.humidity, 'wind_speed': h.wind_speed_ms, 'wind_gust_speed': h.wind_gust_ms,
              'wind_bearing': h.wind_bearing, 'precipitation': h.precipitation_mm,
              'precipitation_probability': h.precipitation_probability,
              'cloud_coverage': h.cloud_coverage, 'uv_index': h.uv_index}] %}
          {% endfor %}
          {{ ns.out }}
        forecast_daily: >-
          {% set ns = namespace(out=[]) %}
          {% for d in state_attr('sensor.home_forecast_updated', 'days') or [] %}
            {% set ns.out = ns.out + [{'datetime': d.date, 'condition': d.condition,
              'temperature': d.temperature_high_c, 'templow': d.temperature_low_c,
              'humidity': d.humidity, 'wind_speed': d.wind_speed_ms, 'wind_gust_speed': d.wind_gust_ms,
              'wind_bearing': d.wind_bearing, 'precipitation': d.precipitation_mm,
              'precipitation_probability': d.precipitation_probability,
              'cloud_coverage': d.cloud_coverage, 'uv_index': d.uv_index}] %}
          {% endfor %}
          {{ ns.out }}
```

## The UV estimate

Clear sky: `12.5 * mu^2.42` (Madronich 2007, for typical mid-latitude ozone),
where `mu` is the cosine of the solar zenith angle. Under cloud it is scaled
by the fraction of clear-sky shortwave the model lets through
(`DSWRF / CSDSF`) raised to the 0.6, since cloud dims UV less than it dims
total sunlight. That exponent was checked against Open-Meteo's UV on only
one cloudy afternoon, so treat cloudy-hour UV as rough. Clear-sky values
are the ones that matter, and those don't depend on it.

## Development

Needs only Docker:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.99-slim-trixie cargo test
```

`HF_TEST_GRIB=<file>` also runs a decode test against a real RRFS message:
2026-11-14 18Z f06 2 m temperature, where eccodes reads 290.89 K at
32.988, -117.122.

Releases: tag `vX.Y.Z` (keep `version` in `Cargo.toml` in step), and CI
publishes `:X.Y.Z`, `:X.Y` and `:latest`.

## License

MIT, see [LICENSE](LICENSE). Forecast data is from NOAA (RRFS, HRRR), which
is public domain.
