# home-forecast

The next 24 hours, hour by hour, at the places you name, from NOAA's
[RRFS](https://registry.opendata.aws/noaa-rrfs/) 3 km model (falling back to
[HRRR](https://registry.opendata.aws/noaa-hrrr-pds/)), served as JSON. It's
made to feed a [Home Assistant](https://www.home-assistant.io/) weather entity.

For each hour:

| Field | From |
|---|---|
| `temperature_c`, `dew_point_c` | 2 m temperature and dewpoint |
| `humidity` | from those two (Magnus) |
| `wind_speed_ms`, `wind_bearing`, `wind_gust_ms` | 10 m wind, surface gust |
| `precipitation_mm` | the hour's total |
| `cloud_coverage` | total cloud, % |
| `uv_index` | **estimated**: the models publish none (see below) |
| `condition` | a Home Assistant condition (`sunny`, `partlycloudy`, `rainy`, ...) |
| `daylight` | whether the sun is up |

plus today's and tomorrow's sunrise and sunset. Coverage is the contiguous
US (CONUS), where RRFS and HRRR run.

## How it works

The models publish one GRIB2 file per run and forecast hour, each the whole
CONUS grid (~350 MB), with an index of where each field sits in it. For each
hour the service fetches just the fields it needs by HTTP range request,
about 10 MB, from NOAA's public S3 buckets. It decodes each field once,
picks out every configured location's value, and keeps only those numbers.
A new model run (RRFS posts every 3 hours) costs about 250 MB however many
locations there are. No API keys and no third-party weather service are
involved.

Each hour comes from the freshest run that reaches it. The whole 24 hours
come from one model, since a switch of model partway through would read as
a change in the weather.

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
| `PORT` | default `8080` |
| `TZ` | the zone times are given in, default `America/Los_Angeles` |
| `CACHE_DIR` | keep decoded values here, so a restart doesn't download again |
| `MODEL` | `rrfs` or `hrrr` only, instead of RRFS then HRRR |
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
      "cloud_coverage": 99.0,
      "uv_index": 0.9,
      "daylight": true,
      "condition": "rainy"
    }
  ]
}
```

The first hour is the current one. `downloaded_mb` is what the update that
produced this response fetched, shared by every location in it.

## Home Assistant

A REST sensor holding the hours, and a template weather entity built from it.
Home Assistant converts the units for display.

```yaml
rest:
  - resource: http://home-forecast:8080/forecast/home
    scan_interval: 300
    timeout: 30
    sensor:
      - name: Home forecast updated
        unique_id: home_forecast_updated
        device_class: timestamp
        value_template: "{{ value_json.generated }}"
        json_attributes: [model, runs, sunrise, sunset, next_sunrise, next_sunset, hours]

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
              'cloud_coverage': h.cloud_coverage, 'uv_index': h.uv_index}] %}
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
