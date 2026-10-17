//! Native PostgreSQL storage values and the stable API representation.
//! Decode native values here; SQL never casts storage columns to text for the API.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Datelike, SecondsFormat, Timelike, Utc};
use sqlx::postgres::{PgRow, PgTypeInfo, PgValueRef};
use sqlx::{ColumnIndex, Decode, Postgres, Type, ValueRef};
use uuid::Uuid;

/// C10: UTC, six fractional digits, explicit offset, truncation (never rounding).
pub fn format_date(value: DateTime<Utc>) -> String {
    truncate_date(value).to_rfc3339_opts(SecondsFormat::Micros, false)
}
pub fn truncate_date(value: DateTime<Utc>) -> DateTime<Utc> {
    value
        .with_nanosecond(value.nanosecond() / 1_000 * 1_000)
        .unwrap()
}
/// The audited timestamp grammar (the one the SQLite move accepted), cut to
/// microseconds.
pub fn date(value: &str) -> Result<DateTime<Utc>> {
    let (micros, _) = census_date(value)?;
    DateTime::from_timestamp_micros(micros)
        .ok_or_else(|| anyhow::anyhow!("timestamp outside supported range"))
}
pub fn optional_date(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value.map(date).transpose()
}
pub fn epoch_millis(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_micros(epoch_micros(value)?)
        .ok_or_else(|| anyhow::anyhow!("epoch milliseconds outside supported range"))
}

/// Strict census grammar; cut the decimal fraction before parsing the instant.
fn census_date(s: &str) -> Result<(i64, bool)> {
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

fn epoch_micros(ms: i64) -> Result<i64> {
    let dt = DateTime::from_timestamp(
        ms.div_euclid(1000),
        (ms.rem_euclid(1000) * 1_000_000) as u32,
    )
    .ok_or_else(|| anyhow::anyhow!("millisecond epoch out of range"))?;
    checked_time(dt)
}

#[cfg(test)]
pub fn optional_epoch_millis(value: Option<i64>) -> Result<Option<DateTime<Utc>>> {
    value.map(epoch_millis).transpose()
}
pub fn uuid(value: &str) -> Result<Uuid> {
    Uuid::parse_str(value).context("invalid UUID")
}
pub fn optional_uuid(value: Option<&str>) -> Result<Option<Uuid>> {
    value.map(uuid).transpose()
}

#[derive(Debug)]
pub struct DbString(pub String);
#[derive(Debug)]
pub struct DbOptionalString(pub Option<String>);
#[derive(Debug)]
pub struct DbI64(pub i64);
#[derive(Debug)]
pub struct DbOptionalI64(pub Option<i64>);

fn string_type(ty: &PgTypeInfo) -> bool {
    <String as Type<Postgres>>::compatible(ty)
        || <Uuid as Type<Postgres>>::compatible(ty)
        || <DateTime<Utc> as Type<Postgres>>::compatible(ty)
        // suwayomi_series.latest_chapter_at stays an epoch-millisecond BIGINT
        // while its established application value is a decimal String.
        || <i64 as Type<Postgres>>::compatible(ty)
        || <serde_json::Value as Type<Postgres>>::compatible(ty)
}
fn integer_type(ty: &PgTypeInfo) -> bool {
    <i64 as Type<Postgres>>::compatible(ty)
        || <i32 as Type<Postgres>>::compatible(ty)
        || <i16 as Type<Postgres>>::compatible(ty)
        || <bool as Type<Postgres>>::compatible(ty)
        || <DateTime<Utc> as Type<Postgres>>::compatible(ty)
}
macro_rules! native_type {
    ($t:ty, $canonical:ty, $compatible:ident) => {
        impl Type<Postgres> for $t {
            fn type_info() -> PgTypeInfo {
                <$canonical as Type<Postgres>>::type_info()
            }
            fn compatible(ty: &PgTypeInfo) -> bool {
                $compatible(ty)
            }
        }
    };
}
native_type!(DbString, String, string_type);
native_type!(DbOptionalString, String, string_type);
native_type!(DbI64, i64, integer_type);
native_type!(DbOptionalI64, i64, integer_type);
impl<'r> Decode<'r, Postgres> for DbString {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        let ty = value.type_info();
        let value = if <Uuid as Type<Postgres>>::compatible(&ty) {
            <Uuid as Decode<Postgres>>::decode(value)?.to_string()
        } else if <DateTime<Utc> as Type<Postgres>>::compatible(&ty) {
            format_date(<DateTime<Utc> as Decode<Postgres>>::decode(value)?)
        } else if <i64 as Type<Postgres>>::compatible(&ty) {
            <i64 as Decode<Postgres>>::decode(value)?.to_string()
        } else if <serde_json::Value as Type<Postgres>>::compatible(&ty) {
            serde_json::to_string(&<serde_json::Value as Decode<Postgres>>::decode(value)?)?
        } else {
            <String as Decode<Postgres>>::decode(value)?
        };
        Ok(Self(value))
    }
}
impl<'r> Decode<'r, Postgres> for DbI64 {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        let ty = value.type_info();
        let value = if <bool as Type<Postgres>>::compatible(&ty) {
            i64::from(<bool as Decode<Postgres>>::decode(value)?)
        } else if <DateTime<Utc> as Type<Postgres>>::compatible(&ty) {
            <DateTime<Utc> as Decode<Postgres>>::decode(value)?.timestamp_millis()
        } else if <i32 as Type<Postgres>>::compatible(&ty) {
            i64::from(<i32 as Decode<Postgres>>::decode(value)?)
        } else if <i16 as Type<Postgres>>::compatible(&ty) {
            i64::from(<i16 as Decode<Postgres>>::decode(value)?)
        } else {
            <i64 as Decode<Postgres>>::decode(value)?
        };
        Ok(Self(value))
    }
}
impl<'r> Decode<'r, Postgres> for DbOptionalString {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        Ok(Self(if value.is_null() {
            None
        } else {
            Some(DbString::decode(value)?.0)
        }))
    }
}
impl<'r> Decode<'r, Postgres> for DbOptionalI64 {
    fn decode(value: PgValueRef<'r>) -> std::result::Result<Self, sqlx::error::BoxDynError> {
        Ok(Self(if value.is_null() {
            None
        } else {
            Some(DbI64::decode(value)?.0)
        }))
    }
}
impl From<DbString> for String {
    fn from(value: DbString) -> Self {
        value.0
    }
}
impl From<DbOptionalString> for Option<String> {
    fn from(value: DbOptionalString) -> Self {
        value.0
    }
}
impl From<DbI64> for i64 {
    fn from(value: DbI64) -> Self {
        value.0
    }
}
impl From<DbOptionalI64> for Option<i64> {
    fn from(value: DbOptionalI64) -> Self {
        value.0
    }
}

pub trait NativeDecode: Sized {
    fn decode<I: ColumnIndex<PgRow>>(
        row: &PgRow,
        index: I,
    ) -> std::result::Result<Self, sqlx::Error>;
}
macro_rules! decode_via {
    ($t:ty, $decoder:ty) => {
        impl NativeDecode for $t {
            fn decode<I: ColumnIndex<PgRow>>(
                row: &PgRow,
                index: I,
            ) -> std::result::Result<Self, sqlx::Error> {
                sqlx::Row::try_get::<$decoder, _>(row, index).map(Into::into)
            }
        }
    };
}
decode_via!(String, DbString);
decode_via!(Option<String>, DbOptionalString);
decode_via!(i64, DbI64);
decode_via!(Option<i64>, DbOptionalI64);
macro_rules! decode_direct {
    ($($t:ty),* $(,)?) => { $(
        impl NativeDecode for $t {
            fn decode<I: ColumnIndex<PgRow>>(row: &PgRow, index: I) -> std::result::Result<Self, sqlx::Error> {
                sqlx::Row::try_get(row, index)
            }
        }
    )* };
}
decode_direct!(
    bool,
    Option<bool>,
    i16,
    Option<i16>,
    i32,
    Option<i32>,
    f32,
    Option<f32>,
    f64,
    Option<f64>,
    Vec<u8>,
    Option<Vec<u8>>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Uuid,
    Option<Uuid>,
    serde_json::Value,
    Option<serde_json::Value>
);
/// Native decoding with conversion only into the existing application/API types.
pub trait Row {
    fn try_get<T: NativeDecode, I: ColumnIndex<PgRow>>(
        &self,
        index: I,
    ) -> std::result::Result<T, sqlx::Error>;
    #[cfg(test)]
    fn get<T: NativeDecode, I: ColumnIndex<PgRow>>(&self, index: I) -> T {
        self.try_get(index).unwrap()
    }
}
impl Row for PgRow {
    fn try_get<T: NativeDecode, I: ColumnIndex<PgRow>>(
        &self,
        index: I,
    ) -> std::result::Result<T, sqlx::Error> {
        T::decode(self, index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn date_census_layouts_and_truncation() {
        let expected = census_date("2020-01-02T01:02:03Z").unwrap().0;
        for s in [
            "2020-01-02T01:02:03+00:00",
            "2020-01-02T03:02:03+02:00",
            "2020-01-02T01:02:03",
            "2020-01-02 01:02:03",
        ] {
            assert_eq!(census_date(s).unwrap(), (expected, false), "{s}");
        }
        for digits in 0..=9 {
            let suffix = if digits == 0 {
                String::new()
            } else {
                format!(".{}", &"123456789"[..digits])
            };
            let (us, cut) = census_date(&format!("2020-01-02T01:02:03{suffix}Z")).unwrap();
            let fraction = if digits == 0 {
                0
            } else {
                format!("{:<06}", &"123456789"[..digits.min(6)])
                    .replace(' ', "0")
                    .parse::<i64>()
                    .unwrap()
            };
            assert_eq!(us, expected + fraction);
            assert_eq!(cut, digits > 6);
        }
        assert_eq!(
            census_date("2020-01-02T01:02:03.999999999Z").unwrap().0,
            expected + 999999
        );
        assert!(census_date("2020-01-02T01:02:03.123456000Z").unwrap().1);
        assert!(census_date("+20250-01-03T19:44:17.225+00:00").is_ok());
        let sentinel = census_date("2037-12-31T15:00:00+00:00").unwrap().0;
        assert_eq!(sentinel, epoch_micros(2_145_884_400_000).unwrap());
    }

    #[test]
    fn dates_have_hand_computed_utc_microseconds() {
        // 2020-01-01: (50*365 + 12 leap days)*86400 = 1_577_836_800 s.
        // UTC = local seconds - signed offset; multiply by 1_000_000 for micros.
        let jan1 = 1_577_836_800i64 * 1_000_000;
        for (value, expected, cut) in [
            (
                "2020-01-01T00:00:00-05:30",
                jan1 + (5 * 3600 + 30 * 60) * 1_000_000,
                false,
            ),
            (
                "2020-01-01T00:00:00+05:45",
                jan1 - (5 * 3600 + 45 * 60) * 1_000_000,
                false,
            ),
            (
                "2020-01-01T00:00:00-23:59",
                jan1 + (23 * 3600 + 59 * 60) * 1_000_000,
                false,
            ),
            (
                "2020-01-01T00:00:00+14:00",
                jan1 - 14 * 3600 * 1_000_000,
                false,
            ),
            // UTC crosses day, month and year into 2019-12-31 10:30.
            (
                "2020-01-01T00:30:00+14:00",
                jan1 - (13 * 3600 + 30 * 60) * 1_000_000,
                false,
            ),
            ("2020-01-01T00:00:00.999999999Z", jan1 + 999_999, true),
            // Jan has 31 days; Feb 29 is 28 days after Feb 1: 59*86400 s.
            (
                "2020-02-29T12:34:56Z",
                jan1 + (59 * 86400 + 12 * 3600 + 34 * 60 + 56) * 1_000_000,
                false,
            ),
            // 1970..20249: 18280*365 + (4570-183+46) leap days.
            // Jan 3 adds 2 days: 6_676_635*86400 + 19*3600 + 44*60 + 17 s,
            // plus 225_000 micros = 576_861_335_057_225_000.
            (
                "+20250-01-03T19:44:17.225+00:00",
                576_861_335_057_225_000,
                false,
            ),
        ] {
            assert_eq!(census_date(value).unwrap(), (expected, cut), "{value}");
        }
    }

    #[test]
    fn milliseconds_have_hand_computed_utc_microseconds() {
        // 2026-01-01: (56*365 + 14)*86400 s, plus .225 s; ms*1000.
        assert_eq!(
            epoch_micros(1_767_225_600_225).unwrap(),
            1_767_225_600_225_000
        );
        // 9999-12-31: 8029*365 + (2007-80+20) leap days + 364 = 2_932_896 days.
        // (days*86400 + 86399)*1_000_000 + 999_000 micros.
        assert_eq!(
            epoch_micros(253_402_300_799_999).unwrap(),
            253_402_300_799_999_000
        );
        // -1001 ms = (-2 s + 999 ms), Euclidean remainder, hence -1_001_000 us.
        assert_eq!(epoch_micros(-1001).unwrap(), -1_001_000);
    }

    #[test]
    fn date_refuses_unrecorded_or_invalid_values_without_panics() {
        for s in [
            "",
            "Z",
            "+",
            "+2-01-01",
            "2020-01-01",
            "2020-02-30T00:00:00Z",
            "2020-01-01t00:00:00z",
            "2020-01-01T24:00:00Z",
            "2020-01-01T00:00:60Z",
            "2020-01-01T00:00:00.Z",
            "2020-01-01T00:00:00.1234567890Z",
            "2020-01-01T00:00:00+24:00",
            "2020-01-01T00:00:00+01:60",
            "2020-01-01T00:00:00+0000",
            "2020-01-01 00:00:00Z",
            "0000-01-01T00:00:00Z",
            "2020-01-01T00:00:00Z secret",
            "💥",
        ] {
            assert!(census_date(s).is_err(), "{s}");
        }
    }

    #[test]
    fn signed_epochs_and_range() {
        assert_eq!(epoch_micros(-1).unwrap(), -1000);
        assert_eq!(
            epoch_micros(-26_616_935_569_000).unwrap(),
            -26_616_935_569_000_000
        );
        assert_eq!(
            epoch_micros(-37_028_731_301_000).unwrap(),
            -37_028_731_301_000_000
        );
        assert!(epoch_micros(i64::MAX).is_err());
        assert!(epoch_micros(i64::MIN).is_err());
    }

    #[test]
    fn c10_truncates_and_preserves_offsets_and_empty_null_semantics() {
        assert_eq!(
            format_date(date("2025-01-02T08:34:05.123456999+05:30").unwrap()),
            "2025-01-02T03:04:05.123456+00:00"
        );
        assert_eq!(
            format_date(date("2025-01-02T03:04:05Z").unwrap()),
            "2025-01-02T03:04:05.000000+00:00"
        );
        assert_eq!(
            format_date(date("2025-01-02T03:04:05").unwrap()),
            "2025-01-02T03:04:05.000000+00:00"
        );
        assert_eq!(optional_date(None).unwrap(), None);
        assert!(date("").is_err());
        for invalid in [
            "0000-01-01T00:00:00Z",
            "2016-12-31T23:59:60Z",
            "2025-01-02T03:04:05.123",
        ] {
            assert!(date(invalid).is_err());
        }
        assert!(uuid("not-a-uuid").is_err());
    }
    #[tokio::test]
    async fn native_timestamp_binds_preserve_historic_and_expanded_instants_and_truncate() {
        let pool = crate::database::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        for (input, expected) in [
            ("0001-01-01T00:00:00Z", "0001-01-01T00:00:00.000000+00:00"),
            (
                "+20250-01-03T19:44:17.225+00:00",
                "+20250-01-03T19:44:17.225000+00:00",
            ),
            (
                "2025-01-02T08:34:05.123456999+05:30",
                "2025-01-02T03:04:05.123456+00:00",
            ),
        ] {
            let stored: DateTime<Utc> = sqlx::query_scalar("SELECT $1::timestamptz")
                .bind(date(input).unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(format_date(stored), expected);
        }
        let stored: DateTime<Utc> = sqlx::query_scalar("SELECT $1::timestamptz")
            .bind(epoch_millis(-1).unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(format_date(stored), "1969-12-31T23:59:59.999000+00:00");
        pool.close().await;
    }
    #[tokio::test]
    async fn native_decoders_use_uuid_timestamp_boolean_and_integer_values() {
        let pool = crate::database::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let row = sqlx::query("SELECT '00112233-4455-6677-8899-aabbccddeeff'::uuid AS id, '2025-01-02T03:04:05.123456Z'::timestamptz AS moment, true AS flag, 7::integer AS count, NULL::timestamptz AS absent, -1::bigint AS epoch, NULL::bigint AS missing_epoch, '[\"a\",\"b\"]'::jsonb AS genre, NULL::jsonb AS missing_genre")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(
            row.get::<String, _>("id"),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
        assert_eq!(
            row.get::<String, _>("moment"),
            "2025-01-02T03:04:05.123456+00:00"
        );
        assert_eq!(row.get::<i64, _>("moment"), 1_735_787_045_123);
        assert_eq!(row.get::<i64, _>("flag"), 1);
        assert_eq!(row.get::<i64, _>("count"), 7);
        assert_eq!(row.get::<Option<String>, _>("absent"), None);
        assert_eq!(row.get::<String, _>("epoch"), "-1");
        assert_eq!(row.get::<Option<String>, _>("epoch"), Some("-1".into()));
        assert_eq!(row.get::<Option<String>, _>("missing_epoch"), None);
        assert_eq!(row.get::<String, _>("genre"), "[\"a\",\"b\"]");
        assert_eq!(
            row.get::<Option<String>, _>("genre"),
            Some("[\"a\",\"b\"]".into())
        );
        assert_eq!(row.get::<Option<String>, _>("missing_genre"), None);
        pool.close().await;
    }
}

/// Runtime scalar/tuple reads decode the audited native types before adapting to the
/// existing repository return types. Bindings remain PostgreSQL's native SQLx values.
pub struct NativeQuery<'q, O> {
    inner: sqlx::query::Query<'q, Postgres, sqlx::postgres::PgArguments>,
    decode: fn(PgRow) -> std::result::Result<O, sqlx::Error>,
}

impl<'q, O: Send + Unpin + 'static> NativeQuery<'q, O> {
    pub fn bind<T: 'q + sqlx::Encode<'q, Postgres> + Type<Postgres>>(mut self, value: T) -> Self {
        self.inner = self.inner.bind(value);
        self
    }
    pub async fn fetch_all<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> std::result::Result<Vec<O>, sqlx::Error>
    where
        'q: 'e,
        E: 'e + sqlx::Executor<'c, Database = Postgres>,
    {
        self.inner.try_map(self.decode).fetch_all(executor).await
    }
    pub async fn fetch_one<'e, 'c: 'e, E>(self, executor: E) -> std::result::Result<O, sqlx::Error>
    where
        'q: 'e,
        E: 'e + sqlx::Executor<'c, Database = Postgres>,
    {
        self.inner.try_map(self.decode).fetch_one(executor).await
    }
    pub async fn fetch_optional<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> std::result::Result<Option<O>, sqlx::Error>
    where
        'q: 'e,
        E: 'e + sqlx::Executor<'c, Database = Postgres>,
    {
        self.inner
            .try_map(self.decode)
            .fetch_optional(executor)
            .await
    }
}

pub fn query_scalar<T: NativeDecode + Send + Unpin + 'static>(sql: &str) -> NativeQuery<'_, T> {
    NativeQuery {
        inner: sqlx::query(sql),
        decode: |row| T::decode(&row, 0),
    }
}

pub trait NativeTuple: Sized {
    fn decode_tuple(row: &PgRow) -> std::result::Result<Self, sqlx::Error>;
}
macro_rules! native_tuple {
    ($($ty:ident:$index:tt),+) => {
        impl<$($ty: NativeDecode),+> NativeTuple for ($($ty,)+) {
            fn decode_tuple(row: &PgRow) -> std::result::Result<Self, sqlx::Error> {
                Ok(($($ty::decode(row, $index)?,)+))
            }
        }
    };
}
native_tuple!(A:0);
native_tuple!(A:0,B:1);
native_tuple!(A:0,B:1,C:2);
native_tuple!(A:0,B:1,C:2,D:3);
native_tuple!(A:0,B:1,C:2,D:3,E:4);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13,O:14);
native_tuple!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7,I:8,J:9,K:10,L:11,M:12,N:13,O:14,P:15);

pub fn query_tuple<T: NativeTuple + Send + Unpin + 'static>(sql: &str) -> NativeQuery<'_, T> {
    NativeQuery {
        inner: sqlx::query(sql),
        decode: |row| T::decode_tuple(&row),
    }
}

/// Explicit test-fixture identity conversion. Runtime UUID parsing remains strict;
/// valid fixture UUIDs (including every golden identity) keep their exact bytes.
#[cfg(test)]
pub fn test_uuid(value: &str) -> Uuid {
    use sha2::Digest;
    if let Ok(id) = Uuid::parse_str(value) {
        return id;
    }
    let digest = sha2::Sha256::digest(format!("komika-phase3-test:{value}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
