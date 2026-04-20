use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use fx_core::types::{EventTier, StreamId};
use fx_events::event::GenericEvent;
use fx_events::header::EventHeader;
use fx_events::proto;
use prost::Message as _;
use serde::Deserialize;

/// A single row of tick data loaded from CSV.
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

/// Load ticks from a CSV file and validate them.
pub fn load_csv<P: AsRef<Path>>(path: P) -> Result<Vec<ValidatedTick>, DataLoadError> {
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(DataLoadError::Csv)?;

    let mut ticks = Vec::new();
    let mut prev_ts: Option<u64> = None;

    for (idx, result) in reader.records().enumerate() {
        let record = result.map_err(|e| DataLoadError::CsvParse {
            row: idx + 2, // +2 for header + 0-indexed
            message: e.to_string(),
        })?;

        let tick: DataTick = record
            .deserialize(None)
            .map_err(|e| DataLoadError::CsvParse {
                row: idx + 2,
                message: e.to_string(),
            })?;

        let ts_ns = parse_timestamp(&tick.timestamp).map_err(|e| DataLoadError::Validation {
            row: idx + 2,
            message: format!("invalid timestamp '{}': {}", tick.timestamp, e),
        })?;

        // Validate bid < ask
        if tick.bid >= tick.ask {
            return Err(DataLoadError::Validation {
                row: idx + 2,
                message: format!("bid ({}) must be < ask ({})", tick.bid, tick.ask),
            });
        }

        // Validate monotonic timestamps
        if let Some(prev) = prev_ts {
            if ts_ns <= prev {
                return Err(DataLoadError::Validation {
                    row: idx + 2,
                    message: format!(
                        "timestamp {} (ns) is not monotonically increasing (prev: {})",
                        ts_ns, prev
                    ),
                });
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

/// Load ticks from a CSV reader (for in-memory/testing).
pub fn load_csv_reader<R: Read>(reader: R) -> Result<Vec<ValidatedTick>, DataLoadError> {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(reader);

    let mut ticks = Vec::new();
    let mut prev_ts: Option<u64> = None;

    for (idx, result) in rdr.records().enumerate() {
        let record = result.map_err(|e| DataLoadError::CsvParse {
            row: idx + 2,
            message: e.to_string(),
        })?;

        let tick: DataTick = record
            .deserialize(None)
            .map_err(|e| DataLoadError::CsvParse {
                row: idx + 2,
                message: e.to_string(),
            })?;

        let ts_ns = parse_timestamp(&tick.timestamp).map_err(|e| DataLoadError::Validation {
            row: idx + 2,
            message: format!("invalid timestamp '{}': {}", tick.timestamp, e),
        })?;

        if tick.bid >= tick.ask {
            return Err(DataLoadError::Validation {
                row: idx + 2,
                message: format!("bid ({}) must be < ask ({})", tick.bid, tick.ask),
            });
        }

        if let Some(prev) = prev_ts {
            if ts_ns <= prev {
                return Err(DataLoadError::Validation {
                    row: idx + 2,
                    message: format!(
                        "timestamp {} (ns) is not monotonically increasing (prev: {})",
                        ts_ns, prev
                    ),
                });
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
}
