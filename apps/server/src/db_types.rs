//! Native PostgreSQL storage values and the stable API representation.
//! Decode native values here; SQL never casts storage columns to text for the API.
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Timelike, Utc};
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
pub fn date(value: &str) -> Result<DateTime<Utc>> {
    crate::pg_import::parse_runtime_date(value)
}
pub fn optional_date(value: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    value.map(date).transpose()
}
pub fn epoch_millis(value: i64) -> Result<DateTime<Utc>> {
    crate::pg_import::parse_runtime_epoch_millis(value)
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
    #[allow(dead_code)]
    pub fn fetch<'e, 'c: 'e, E>(
        self,
        executor: E,
    ) -> futures::stream::BoxStream<'e, std::result::Result<O, sqlx::Error>>
    where
        'q: 'e,
        E: 'e + sqlx::Executor<'c, Database = Postgres>,
    {
        self.inner.try_map(self.decode).fetch(executor)
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
