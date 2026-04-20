use std::io::Read;
use std::path::Path;
use tracing::warn;

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use fx_core::types::{EventTier, StreamId};
use fx_events::event::GenericEvent;
use fx_events::header::EventHeader;
use fx_events::proto;
use prost::Message as _;
use serde::Deserialize;

/// A single row of tick data loaded from CSV.
///
/// Expected column names (after header normalization):
/// `timestamp`, `bid`, `ask`, `bid_volume` (opt), `ask_volume` (opt), `symbol` (opt)
#[derive(Debug, Clone, Deserialize)]
pub struct DataTick {
    pub timestamp: String,
    pub bid: f64,
    pub ask: f64,
    #[serde(default)]
    pub bid_volume: Option<f64>,
    #[serde(default)]
    pub ask_volume: Option<f64>,
    #[serde(default = "default_symbol")]
    pub symbol: String,
}

fn default_symbol() -> String {
    "USD/JPY".to_string()
}

/// Parsed tick data with validated fields and a nanosecond timestamp.
#[derive(Debug, Clone)]
pub struct ValidatedTick {
    pub timestamp_ns: u64,
    pub bid: f64,
    pub ask: f64,
    pub bid_volume: f64,
    pub ask_volume: f64,
    pub symbol: String,
}

/// Errors from CSV data loading and validation.
#[derive(Debug, thiserror::Error)]
pub enum DataLoadError {
    #[error("CSV parse error at row {row}: {message}")]
    CsvParse { row: usize, message: String },
    #[error("Validation error at row {row}: {message}")]
    Validation { row: usize, message: String },
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("CSV error: {0}")]
    Csv(#[from] csv::Error),
}

/// Normalize CSV header names to canonical field names.
fn normalize_header(header: &str) -> String {
    match header.trim().to_lowercase().replace(' ', "_").as_str() {
        "local_time" | "time" | "timestamp" | "datetime" | "date" | "time_(eet)" => {
            "timestamp".to_string()
        }
        "bid" => "bid".to_string(),
        "ask" => "ask".to_string(),
        "bidvolume" | "bid_vol" | "bid_size" => "bid_volume".to_string(),
        "askvolume" | "ask_vol" | "ask_size" => "ask_volume".to_string(),
        "symbol" | "ticker" | "pair" => "symbol".to_string(),
        other => other.to_string(),
    }
}

/// Load ticks from a CSV file and validate them.
pub fn load_csv<P: AsRef<Path>>(path: P) -> Result<Vec<ValidatedTick>, DataLoadError> {
    let data = std::fs::read_to_string(path).map_err(DataLoadError::Io)?;
    load_csv_reader(data.as_bytes())
}

/// Load ticks from a CSV reader (for in-memory/testing).
pub fn load_csv_reader<R: Read>(reader: R) -> Result<Vec<ValidatedTick>, DataLoadError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    // Read and normalize headers
    let headers = rdr.headers().map_err(DataLoadError::Csv)?.clone();
    let normalized: csv::StringRecord = headers.iter().map(normalize_header).collect();

    let mut ticks = Vec::new();
    let mut prev_ts: Option<u64> = None;

    for (idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| DataLoadError::CsvParse {
            row: idx + 2,
            message: e.to_string(),
        })?;

        let tick: DataTick =
            record
                .deserialize(Some(&normalized))
                .map_err(|e| DataLoadError::CsvParse {
                    row: idx + 2,
                    message: e.to_string(),
                })?;

        let ts_ns = parse_timestamp(&tick.timestamp).map_err(|e| DataLoadError::Validation {
            row: idx + 2,
            message: format!("invalid timestamp '{}': {}", tick.timestamp, e),
        })?;

        if tick.bid >= tick.ask {
            warn!(
                row = idx + 2,
                bid = tick.bid,
                ask = tick.ask,
                "skipping row: bid >= ask (crossed market)"
            );
            continue;
        }

        if let Some(prev) = prev_ts {
            if ts_ns <= prev {
                warn!(
                    row = idx + 2,
                    timestamp_ns = ts_ns,
                    prev = prev,
                    "skipping row: timestamp not monotonically increasing"
                );
                continue;
            }
        }

        ticks.push(ValidatedTick {
            timestamp_ns: ts_ns,
            bid: tick.bid,
            ask: tick.ask,
            bid_volume: tick.bid_volume.unwrap_or(0.0),
            ask_volume: tick.ask_volume.unwrap_or(0.0),
            symbol: tick.symbol,
        });

        prev_ts = Some(ts_ns);
    }

    Ok(ticks)
}

/// Convert validated ticks into `GenericEvent`s suitable for `BacktestEngine::run_from_events`.
pub fn ticks_to_events(ticks: &[ValidatedTick]) -> Vec<GenericEvent> {
    ticks.iter().map(tick_to_event).collect()
}

/// Convert a single validated tick to a `GenericEvent`.
pub fn tick_to_event(tick: &ValidatedTick) -> GenericEvent {
    let payload = proto::MarketEventPayload {
        header: None,
        symbol: tick.symbol.clone(),
        bid: tick.bid,
        ask: tick.ask,
        bid_size: tick.bid_volume,
        ask_size: tick.ask_volume,
        timestamp_ns: tick.timestamp_ns,
        bid_levels: vec![],
        ask_levels: vec![],
        latency_ms: 0.0,
    }
    .encode_to_vec();

    let header = EventHeader {
        timestamp_ns: tick.timestamp_ns,
        stream_id: StreamId::Market,
        sequence_id: 0,
        tier: EventTier::Tier3Raw,
        ..EventHeader::new(StreamId::Market, 0, EventTier::Tier3Raw)
    };

    GenericEvent::new(header, payload)
}

/// Parse a timestamp string into nanoseconds.
///
/// Supported formats:
/// - Unix timestamp in nanoseconds (pure digits): `"1700000000000000000"`
/// - Unix timestamp in seconds (short number): `"1700000000"`
/// - ISO 8601: `"2023-11-14T22:13:20"`, `"2023-11-14T22:13:20.123"`
/// - Date + time: `"2023-11-14 22:13:20"`, `"2023-11-14 22:13:20.123"`
fn parse_timestamp(s: &str) -> Result<u64> {
    let s = s.trim();

    // Pure numeric: treat as nanoseconds if > 1e15, otherwise seconds
    if s.chars().all(|c| c.is_ascii_digit()) {
        let val: u64 = s
            .parse()
            .with_context(|| format!("cannot parse '{}' as integer", s))?;
        if val > 1_000_000_000_000_000 {
            return Ok(val);
        }
        return Ok(val * 1_000_000_000);
    }

    // DD.MM.YYYY HH:MM:SS.mmm GMT+HHMM (e.g. "13.04.2026 06:00:30.789 GMT+0900")
    if let Some(gmt_pos) = s.find("GMT") {
        let datetime_part = s[..gmt_pos].trim();
        let tz_str = &s[gmt_pos + 3..];
        let tz_offset_secs: i32 = if !tz_str.is_empty() {
            let sign = if tz_str.starts_with('-') { -1 } else { 1i32 };
            let digits = tz_str.trim_start_matches('+').trim_start_matches('-');
            sign * digits
                .get(..2)
                .and_then(|h| h.parse::<i32>().ok())
                .unwrap_or(0)
                * 3600
                + sign
                    * digits
                        .get(2..)
                        .and_then(|m| m.parse::<i32>().ok())
                        .unwrap_or(0)
                    * 60
        } else {
            0
        };

        for fmt in ["%d.%m.%Y %H:%M:%S%.f", "%d.%m.%Y %H:%M:%S"] {
            if let Ok(dt) = NaiveDateTime::parse_from_str(datetime_part, fmt) {
                let utc_ns = dt.and_utc().timestamp_nanos_opt().unwrap_or(0) as i64
                    - (tz_offset_secs as i64) * 1_000_000_000;
                return Ok(utc_ns as u64);
            }
        }
    }

    // YYYY.MM.DD HH:MM:SS.mmm (Dukascopy EET format, no timezone suffix)
    // EET = UTC+2 (winter) / EEST = UTC+3 (summer DST)
    // Use Europe/Helsinki timezone for automatic DST detection.
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y.%m.%d %H:%M:%S%.f") {
        let helsinki = chrono_tz::Europe::Helsinki;
        let local = dt.and_local_timezone(helsinki).single();
        let utc_ns = match local {
            Some(local_dt) => local_dt.timestamp_nanos_opt().unwrap_or(0) as u64,
            None => {
                // Ambiguous time (fall-back) or non-existent (spring-forward): prefer earlier
                let earliest = dt.and_local_timezone(helsinki).earliest();
                match earliest {
                    Some(e) => e.timestamp_nanos_opt().unwrap_or(0) as u64,
                    None => {
                        // Non-existent local time: use UTC+2 as fallback
                        let eet_offset_ns = 2i64 * 3600 * 1_000_000_000;
                        (dt.and_utc().timestamp_nanos_opt().unwrap_or(0) as i64 - eet_offset_ns)
                            as u64
                    }
                }
            }
        };
        return Ok(utc_ns);
    }

    // Try ISO 8601 with T separator
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt.and_utc().timestamp_nanos_opt().unwrap_or(0) as u64);
        }
    }

    anyhow::bail!("unrecognized timestamp format: '{}'", s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_timestamp_ns() {
        assert_eq!(
            parse_timestamp("1700000000000000000").unwrap(),
            1700000000000000000u64
        );
    }

    #[test]
    fn test_parse_timestamp_seconds() {
        assert_eq!(
            parse_timestamp("1700000000").unwrap(),
            1700000000000000000u64
        );
    }

    #[test]
    fn test_parse_timestamp_iso8601() {
        let ns = parse_timestamp("2023-11-14T22:13:20").unwrap();
        assert!(ns > 0);
        // Should be equivalent to 1700000000 seconds in ns
        assert_eq!(ns, 1700000000000000000u64);
    }

    #[test]
    fn test_parse_timestamp_datetime_with_millis() {
        let ns = parse_timestamp("2023-11-14T22:13:20.500").unwrap();
        assert_eq!(ns, 1700000000500000000u64);
    }

    #[test]
    fn test_parse_timestamp_space_separator() {
        let ns = parse_timestamp("2023-11-14 22:13:20").unwrap();
        assert_eq!(ns, 1700000000000000000u64);
    }

    #[test]
    fn test_load_csv_from_reader() {
        let csv_data = "timestamp,bid,ask,bid_volume,ask_volume,symbol
1700000000000000000,110.001,110.003,1000000,1000000,USD/JPY
1700000001000000000,110.005,110.007,1000000,1000000,USD/JPY
1700000002000000000,110.010,110.012,1000000,1000000,USD/JPY";
        let ticks = load_csv_reader(Cursor::new(csv_data)).unwrap();
        assert_eq!(ticks.len(), 3);
        assert!((ticks[0].bid - 110.001).abs() < 1e-10);
        assert!((ticks[0].ask - 110.003).abs() < 1e-10);
        assert_eq!(ticks[0].symbol, "USD/JPY");
    }

    #[test]
    fn test_load_csv_validation_bid_ge_ask() {
        let csv_data = "timestamp,bid,ask
1700000000000000000,110.005,110.003";
        let result = load_csv_reader(Cursor::new(csv_data));
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            DataLoadError::Validation { row, .. } => assert_eq!(row, 2),
            _ => panic!("expected Validation error, got {:?}", err),
        }
    }

    #[test]
    fn test_load_csv_validation_non_monotonic() {
        let csv_data = "timestamp,bid,ask
1700000002000000000,110.001,110.003
1700000001000000000,110.005,110.007";
        let result = load_csv_reader(Cursor::new(csv_data));
        assert!(result.is_err());
        let err = result.unwrap_err();
        match err {
            DataLoadError::Validation { row, .. } => assert_eq!(row, 3),
            _ => panic!("expected Validation error, got {:?}", err),
        }
    }

    #[test]
    fn test_ticks_to_events() {
        let ticks = vec![
            ValidatedTick {
                timestamp_ns: 1000,
                bid: 110.001,
                ask: 110.003,
                bid_volume: 1e6,
                ask_volume: 1e6,
                symbol: "USD/JPY".to_string(),
            },
            ValidatedTick {
                timestamp_ns: 2000,
                bid: 110.005,
                ask: 110.007,
                bid_volume: 1e6,
                ask_volume: 1e6,
                symbol: "USD/JPY".to_string(),
            },
        ];
        let events = ticks_to_events(&ticks);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].header.timestamp_ns, 1000);
        assert_eq!(events[1].header.timestamp_ns, 2000);
    }

    #[test]
    fn test_load_csv_with_seconds_timestamp() {
        let csv_data = "timestamp,bid,ask,bid_volume,ask_volume,symbol
1700000000,110.001,110.003,1000000,1000000,USD/JPY
1700000001,110.005,110.007,1000000,1000000,USD/JPY";
        let ticks = load_csv_reader(Cursor::new(csv_data)).unwrap();
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].timestamp_ns, 1700000000000000000u64);
        assert_eq!(ticks[1].timestamp_ns, 1700000001000000000u64);
    }

    #[test]
    fn test_load_csv_with_iso_timestamp() {
        let csv_data = "timestamp,bid,ask
2023-11-14T22:13:20,110.001,110.003
2023-11-14T22:13:21,110.005,110.007";
        let ticks = load_csv_reader(Cursor::new(csv_data)).unwrap();
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].timestamp_ns, 1700000000000000000u64);
    }

    #[test]
    fn test_load_csv_optional_volumes() {
        let csv_data = "timestamp,bid,ask
1700000000000000000,110.001,110.003
1700000001000000000,110.005,110.007";
        let ticks = load_csv_reader(Cursor::new(csv_data)).unwrap();
        assert_eq!(ticks[0].bid_volume, 0.0);
        assert_eq!(ticks[0].ask_volume, 0.0);
    }

    #[test]
    fn test_parse_timestamp_eet_format_winter() {
        // Dukascopy EET format: YYYY.MM.DD HH:MM:SS.mmm
        // Winter (EET = UTC+2): 2024-01-15 12:00:00 EET = 2024-01-15 10:00:00 UTC
        let ns = parse_timestamp("2024.01.15 12:00:00.000").unwrap();
        let utc_ns = parse_timestamp("2024-01-15T10:00:00").unwrap();
        assert_eq!(ns, utc_ns);
    }

    #[test]
    fn test_parse_timestamp_eet_format_summer_dst() {
        // Summer (EEST = UTC+3): 2024-04-22 00:00:08.859 EET = 2024-04-21 21:00:08.859 UTC
        let ns = parse_timestamp("2024.04.22 00:00:08.859").unwrap();
        let utc_ns = parse_timestamp("2024-04-21T21:00:08.859").unwrap();
        assert_eq!(ns, utc_ns);
    }

    #[test]
    fn test_parse_timestamp_eet_dst_transition_spring() {
        // DST starts last Sunday of March 2024 (March 31, 03:00 EET → 04:00 EEST)
        // Before transition: 2024-03-31 02:00:00 EET (UTC+2) = 2024-03-31 00:00:00 UTC
        let ns_before = parse_timestamp("2024.03.31 02:00:00.000").unwrap();
        let utc_before = parse_timestamp("2024-03-31T00:00:00").unwrap();
        assert_eq!(ns_before, utc_before);

        // After transition: 2024-03-31 04:00:00 EEST (UTC+3) = 2024-03-31 01:00:00 UTC
        let ns_after = parse_timestamp("2024.03.31 04:00:00.000").unwrap();
        let utc_after = parse_timestamp("2024-03-31T01:00:00").unwrap();
        assert_eq!(ns_after, utc_after);
    }

    #[test]
    fn test_parse_timestamp_eet_dst_transition_autumn() {
        // DST ends last Sunday of October 2024 (October 27, 04:00 EEST → 03:00 EET)
        // Before transition (clearly DST): 2024-10-27 02:00:00 EEST (UTC+3) = 2024-10-26 23:00:00 UTC
        let ns_before = parse_timestamp("2024.10.27 02:00:00.000").unwrap();
        let utc_before = parse_timestamp("2024-10-26T23:00:00").unwrap();
        assert_eq!(ns_before, utc_before);

        // After transition (clearly standard): 2024-10-27 05:00:00 EET (UTC+2) = 2024-10-27 03:00:00 UTC
        let ns_after = parse_timestamp("2024.10.27 05:00:00.000").unwrap();
        let utc_after = parse_timestamp("2024-10-27T03:00:00").unwrap();
        assert_eq!(ns_after, utc_after);
    }

    #[test]
    fn test_parse_timestamp_eet_dst_ambiguous_time_earliest() {
        // At 2024-10-27 03:30:00, clocks have gone back so this time exists in both EEST and EET.
        // earliest() picks EEST (UTC+3) → 2024-10-27 00:30:00 UTC
        let ns = parse_timestamp("2024.10.27 03:30:00.000").unwrap();
        let utc_ns = parse_timestamp("2024-10-27T00:30:00").unwrap();
        assert_eq!(ns, utc_ns);
    }

    #[test]
    fn test_parse_timestamp_eet_header_normalization() {
        assert_eq!(normalize_header("Time (EET)"), "timestamp");
    }
}
