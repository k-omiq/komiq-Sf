use anyhow::{bail, ensure, Result};
use chrono::{DateTime, Datelike, Timelike, Utc};
use std::collections::BTreeMap;

use super::plan::Column;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cell {
    Null,
    Text(String),
    Bytes(Vec<u8>),
    Integer(i64),
    Real(f64),
    Time(i64),
    Flag(bool),
    Uuid(String),
    Json(JsonCell),
}

#[derive(Clone, Debug)]
pub(crate) struct JsonCell {
    pub input: String,
    pub canonical: String,
}
impl PartialEq for JsonCell {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}

pub(super) fn text(bytes: &[u8]) -> Result<&str> {
    let s =
        std::str::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid UTF-8 in SQLite TEXT"))?;
    ensure!(
        !s.contains('\0'),
        "NUL cannot be represented in PostgreSQL text"
    );
    Ok(s)
}

/// Strict census grammar; cut the decimal fraction before parsing the instant.
pub(super) fn date(s: &str) -> Result<(i64, bool)> {
    ensure!(s.is_ascii(), "date is not ASCII");
    let year_end = if let Some(expanded) = s.strip_prefix('+') {
        expanded.find('-').map(|p| p + 1)
    } else {
        Some(4)
    }
    .ok_or_else(|| anyhow::anyhow!("invalid expanded year"))?;
    ensure!(
        year_end >= 4 && (!s.starts_with('+') || year_end >= 6),
        "invalid year width"
    );
    let year_digits = if s.starts_with('+') {
        &s[1..year_end]
    } else {
        s.get(..year_end).unwrap_or("")
    };
    ensure!(
        year_digits.bytes().all(|b| b.is_ascii_digit()) && !year_digits.is_empty(),
        "invalid year"
    );
    ensure!(
        !s.starts_with('+') || year_digits.parse::<u32>().is_ok_and(|y| y > 9999),
        "expanded years must be beyond 9999"
    );
    let rest = s.get(year_end..).unwrap_or("");
    ensure!(
        rest.len() >= 6
            && rest.as_bytes()[0] == b'-'
            && rest.as_bytes()[3] == b'-'
            && rest[1..3].bytes().all(|b| b.is_ascii_digit())
            && rest[4..6].bytes().all(|b| b.is_ascii_digit()),
        "invalid calendar layout"
    );
    let base = year_end + 6;
    ensure!(
        s.len() >= base + 9 && matches!(s.as_bytes()[base], b'T' | b' '),
        "invalid date separator"
    );
    let clock = &s[base + 1..base + 9];
    ensure!(
        clock.as_bytes()[2] == b':'
            && clock.as_bytes()[5] == b':'
            && clock
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 2 || i == 5 || b.is_ascii_digit()),
        "invalid clock layout"
    );
    ensure!(
        &clock[6..8] != "60",
        "leap seconds are not supported by PostgreSQL timestamps"
    );
    let mut end = base + 9;
    let mut truncated = false;
    let mut fraction = "";
    if s.as_bytes().get(end) == Some(&b'.') {
        let start = end + 1;
        end = start;
        while s.as_bytes().get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        ensure!(
            (1..=9).contains(&(end - start)),
            "fraction must have 1-9 digits"
        );
        truncated = end - start > 6;
        fraction = &s[start..end.min(start + 6)];
    }
    let zone = &s[end..];
    ensure!(
        !zone.is_empty() || fraction.is_empty(),
        "zone-less census dates have whole seconds"
    );
    ensure!(
        zone.is_empty()
            || zone == "Z"
            || (zone.len() == 6
                && matches!(zone.as_bytes()[0], b'+' | b'-')
                && zone.as_bytes()[3] == b':'
                && zone[1..3].bytes().all(|b| b.is_ascii_digit())
                && zone[4..6].bytes().all(|b| b.is_ascii_digit())
                && zone[1..3].parse::<u8>()? < 24
                && zone[4..6].parse::<u8>()? < 60),
        "invalid time zone"
    );
    ensure!(
        s.as_bytes()[base] == b'T' || zone.is_empty(),
        "space-separated census dates have no zone"
    );
    let value = format!(
        "{}T{}{}{}",
        &s[..base],
        clock,
        if fraction.is_empty() {
            String::new()
        } else {
            format!(".{fraction}")
        },
        if zone.is_empty() || zone == "Z" {
            "+00:00"
        } else {
            zone
        }
    );
    let dt = DateTime::parse_from_str(&value, "%Y-%m-%dT%H:%M:%S%.f%:z")
        .map_err(|_| anyhow::anyhow!("invalid date, clock or offset"))?
        .with_timezone(&Utc);
    Ok((checked_time(dt)?, truncated))
}

fn checked_time(dt: DateTime<Utc>) -> Result<i64> {
    // Chrono's positive bound is narrower than PG's; reject BCE and year zero to
    // avoid PostgreSQL's no-year-zero calendar and COPY ambiguity.
    ensure!(
        dt.year() >= 1 && dt.year() <= 262142 && dt.nanosecond() < 1_000_000_000,
        "timestamp outside supported PostgreSQL/chrono range"
    );
    Ok(dt.timestamp_micros())
}

pub(super) fn milliseconds(ms: i64) -> Result<i64> {
    let dt = DateTime::from_timestamp(
        ms.div_euclid(1000),
        (ms.rem_euclid(1000) * 1_000_000) as u32,
    )
    .ok_or_else(|| anyhow::anyhow!("millisecond epoch out of range"))?;
    checked_time(dt)
}

pub(super) fn time_text(us: i64) -> Result<String> {
    let dt = DateTime::from_timestamp_micros(us)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range"))?;
    // PostgreSQL input accepts an unsigned expanded AD year, not chrono's '+'.
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}+00:00",
        dt.year(),
        dt.month(),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.timestamp_subsec_micros()
    ))
}

pub(super) fn uuid(s: &str) -> Result<String> {
    ensure!(
        s.len() == 32 || s.len() == 36,
        "UUID must be 32 hex digits or hyphenated UUID text"
    );
    uuid::Uuid::parse_str(s)
        .map(|u| u.hyphenated().to_string())
        .map_err(|_| anyhow::anyhow!("invalid UUID text"))
}

pub(super) fn boolean(i: i64) -> Result<bool> {
    match i {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("flag must be 0 or 1"),
    }
}

pub(super) fn integer(s: &str, target: &str) -> Result<i64> {
    ensure!(
        !s.is_empty()
            && (s.strip_prefix('-').unwrap_or(s))
                .bytes()
                .all(|b| b.is_ascii_digit()),
        "invalid integer spelling"
    );
    let i: i64 = s
        .parse()
        .map_err(|_| anyhow::anyhow!("signed 64-bit integer overflow"))?;
    ensure!(s == i.to_string(), "non-canonical integer spelling");
    if target == "integer" {
        i32::try_from(i).map_err(|_| anyhow::anyhow!("signed 32-bit integer overflow"))?;
    }
    Ok(i)
}

pub(super) fn real(s: &str) -> Result<f64> {
    let n: f64 = s.parse().map_err(|_| anyhow::anyhow!("invalid real"))?;
    ensure!(n.is_finite(), "non-finite real");
    Ok(if n == 0.0 { 0.0 } else { n })
}

/// Lossless decimal normalization. Never route JSON numbers through f64.
fn number(s: &str) -> Result<String> {
    let (mantissa, exponent) = s.split_once(['e', 'E']).unwrap_or((s, "0"));
    let exponent: i64 = exponent
        .parse()
        .map_err(|_| anyhow::anyhow!("JSON numeric exponent out of range"))?;
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches('-');
    let scale = mantissa.split_once('.').map_or(0, |(_, r)| r.len()) as i64;
    ensure!(
        (-16383..=131071).contains(&exponent)
            && scale.checked_sub(exponent).is_some_and(|s| s <= 16383),
        "JSON number spelling exceeds PostgreSQL numeric range"
    );
    let mut digits = mantissa.replace('.', "").trim_start_matches('0').to_owned();
    if digits.is_empty() {
        return Ok("0".into());
    }
    let mut power = exponent
        .checked_sub(scale)
        .ok_or_else(|| anyhow::anyhow!("JSON number out of range"))?;
    while digits.ends_with('0') {
        digits.pop();
        power += 1;
    }
    ensure!(
        power >= -16383 && power + digits.len() as i64 <= 131072,
        "JSON number exceeds PostgreSQL numeric range"
    );
    Ok(format!(
        "{}{digits}e{power}",
        if negative { "-" } else { "" }
    ))
}

pub(super) fn json(s: &str) -> Result<String> {
    // Isolated lexical parser: serde_json's default numbers lose large decimals,
    // and enabling arbitrary_precision globally changes the server's JSON path.
    // Use serde_json only for string escaping/surrogate-pair validation.
    let mut parser = JsonParser {
        bytes: s.as_bytes(),
        pos: 0,
    };
    let canonical = parser.value(0)?;
    parser.whitespace();
    ensure!(parser.pos == parser.bytes.len(), "trailing JSON input");
    Ok(canonical)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl JsonParser<'_> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.pos += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.bytes.get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn string(&mut self) -> Result<String> {
        self.whitespace();
        let start = self.pos;
        ensure!(
            self.bytes.get(self.pos) == Some(&b'"'),
            "expected JSON string"
        );
        self.pos += 1;
        loop {
            let byte = self
                .bytes
                .get(self.pos)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("unterminated JSON string"))?;
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    ensure!(self.pos < self.bytes.len(), "unterminated JSON escape");
                    self.pos += 1;
                }
                _ => {}
            }
        }
        let value: String = serde_json::from_slice(&self.bytes[start..self.pos])
            .map_err(|_| anyhow::anyhow!("invalid JSON string or escape"))?;
        ensure!(!value.contains('\0'), "decoded NUL in JSON string/key");
        Ok(value)
    }
    fn digits(&mut self) -> Result<()> {
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        ensure!(self.pos > start, "JSON number requires digits");
        Ok(())
    }
    fn numeric(&mut self) -> Result<String> {
        let start = self.pos;
        if self.bytes.get(self.pos) == Some(&b'-') {
            self.pos += 1;
        }
        match self.bytes.get(self.pos) {
            Some(b'0') => {
                self.pos += 1;
            }
            Some(b'1'..=b'9') => self.digits()?,
            _ => bail!("invalid JSON number"),
        }
        if self.bytes.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            self.digits()?;
        }
        if self
            .bytes
            .get(self.pos)
            .is_some_and(|b| matches!(b, b'e' | b'E'))
        {
            self.pos += 1;
            if self
                .bytes
                .get(self.pos)
                .is_some_and(|b| matches!(b, b'+' | b'-'))
            {
                self.pos += 1;
            }
            self.digits()?;
        }
        number(std::str::from_utf8(&self.bytes[start..self.pos])?)
    }
    fn value(&mut self, depth: usize) -> Result<String> {
        ensure!(depth < 128, "JSON nesting exceeds 128 levels");
        self.whitespace();
        let byte = self
            .bytes
            .get(self.pos)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("missing JSON value"))?;
        Ok(match byte {
            b'{' => {
                self.pos += 1;
                let mut fields = BTreeMap::new();
                if !self.take(b'}') {
                    loop {
                        let key = self.string()?;
                        ensure!(self.take(b':'), "expected JSON colon");
                        let value = self.value(depth + 1)?;
                        ensure!(
                            fields.insert(key, value).is_none(),
                            "duplicate JSON object key"
                        );
                        if self.take(b'}') {
                            break;
                        }
                        ensure!(self.take(b','), "expected JSON object comma");
                    }
                }
                let fields = fields
                    .into_iter()
                    .map(|(k, v)| Ok(format!("{}:{v}", serde_json::to_string(&k)?)))
                    .collect::<Result<Vec<_>>>()?;
                format!("{{{}}}", fields.join(","))
            }
            b'[' => {
                self.pos += 1;
                let mut values = Vec::new();
                if !self.take(b']') {
                    loop {
                        values.push(self.value(depth + 1)?);
                        if self.take(b']') {
                            break;
                        }
                        ensure!(self.take(b','), "expected JSON array comma");
                    }
                }
                format!("[{}]", values.join(","))
            }
            b'"' => serde_json::to_string(&self.string()?)?,
            b't' | b'f' | b'n' => {
                let word = match byte {
                    b't' => "true",
                    b'f' => "false",
                    _ => "null",
                };
                ensure!(
                    self.bytes[self.pos..].starts_with(word.as_bytes()),
                    "invalid JSON literal"
                );
                self.pos += word.len();
                word.into()
            }
            b'-' | b'0'..=b'9' => self.numeric()?,
            _ => bail!("invalid JSON value"),
        })
    }
}

pub(crate) fn convert(c: &Column, storage: &str, bytes: Option<&[u8]>) -> Result<(Cell, bool)> {
    let Some(bytes) = bytes else {
        ensure!(!c.required, "NULL in required/primary-key column");
        return Ok((Cell::Null, false));
    };
    if c.target == "bytea" {
        ensure!(storage == "blob", "bytea source is not BLOB");
        return Ok((Cell::Bytes(bytes.to_vec()), false));
    }
    let s = text(bytes)?;
    let cell = match c.target.as_str() {
        "text" => {
            ensure!(storage == "text", "text source has non-TEXT storage class");
            Cell::Text(s.into())
        }
        "uuid" => {
            ensure!(storage == "text", "UUID source is not TEXT");
            Cell::Uuid(uuid(s)?)
        }
        "jsonb" => {
            ensure!(storage == "text", "JSON source is not TEXT");
            Cell::Json(JsonCell {
                input: s.to_owned(),
                canonical: json(s)?,
            })
        }
        "boolean" => {
            ensure!(storage == "integer", "flag source is not INTEGER");
            Cell::Flag(boolean(integer(s, "bigint")?)?)
        }
        "bigint" | "integer" => {
            ensure!(
                storage == "integer" || (c.source == "TEXT" && storage == "text"),
                "integer source has unexpected storage class"
            );
            let value = integer(s, &c.target)?;
            ensure!(
                !c.identity || value != i64::MAX,
                "identity sequence exhausted by bigint maximum"
            );
            Cell::Integer(value)
        }
        "double precision" => {
            ensure!(
                matches!(storage, "integer" | "real"),
                "real source is not numeric"
            );
            Cell::Real(real(s)?)
        }
        "timestamptz" if c.source == "INTEGER" => {
            ensure!(storage == "integer", "epoch source is not INTEGER");
            let ms = integer(s, "bigint")?;
            // Same 1e11 cutoff as date_repair; valid negatives explicitly retained.
            ensure!(
                !c.repair || !(0..100_000_000_000).contains(&ms),
                "seconds-like millisecond value; run repair-upload-dates on a SQLite copy first"
            );
            Cell::Time(milliseconds(ms)?)
        }
        "timestamptz" => {
            ensure!(storage == "text", "date source is not TEXT");
            let (us, cut) = date(s)?;
            return Ok((Cell::Time(us), cut));
        }
        _ => bail!("unsupported target type"),
    };
    Ok((cell, false))
}

pub(crate) fn pg_cell(c: &Column, s: Option<&str>) -> Result<Cell> {
    let Some(s) = s else {
        return Ok(Cell::Null);
    };
    Ok(match c.target.as_str() {
        "text" => Cell::Text(s.into()),
        "bytea" => Cell::Bytes(hex::decode(s)?),
        "uuid" => Cell::Uuid(uuid(s)?),
        "boolean" => Cell::Flag(match s {
            "true" => true,
            "false" => false,
            _ => bail!("invalid PG boolean"),
        }),
        "bigint" | "integer" => Cell::Integer(integer(s, &c.target)?),
        "double precision" => Cell::Real(real(s)?),
        "jsonb" => Cell::Json(JsonCell {
            input: s.to_owned(),
            canonical: json(s)?,
        }),
        "timestamptz" => {
            // PG ISO text uses a space and +00, plus unsigned expanded AD years.
            let (day, clock) = s
                .split_once(' ')
                .ok_or_else(|| anyhow::anyhow!("invalid PG date output"))?;
            let day = if day.find('-').unwrap_or(0) > 4 {
                format!("+{day}")
            } else {
                day.to_owned()
            };
            let clock = clock
                .strip_suffix("+00")
                .ok_or_else(|| anyhow::anyhow!("PG connection must use UTC"))?;
            Cell::Time(date(&format!("{day}T{clock}+00:00"))?.0)
        }
        _ => bail!("unsupported PG target type"),
    })
}

// Length framing makes NULL, empty text, empty bytes and concatenations distinct.
pub(crate) fn frame(cell: &Cell, out: &mut Vec<u8>) {
    let (tag, bytes): (u8, Vec<u8>) = match cell {
        Cell::Null => (0, vec![]),
        Cell::Text(s) => (1, s.as_bytes().to_vec()),
        Cell::Bytes(b) => (2, b.clone()),
        Cell::Integer(n) => (3, n.to_be_bytes().to_vec()),
        Cell::Real(n) => (4, n.to_bits().to_be_bytes().to_vec()),
        Cell::Time(n) => (5, n.to_be_bytes().to_vec()),
        Cell::Flag(b) => (6, vec![u8::from(*b)]),
        Cell::Uuid(s) => (7, s.as_bytes().to_vec()),
        Cell::Json(s) => (8, s.canonical.as_bytes().to_vec()),
    };
    out.push(tag);
    out.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    out.extend_from_slice(&bytes);
}
