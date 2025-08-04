use anyhow::{anyhow, Context, Result};
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use spin_world::async_trait;
use spin_world::spin::postgres4_0_0::postgres::{
    self as v4, Column, DbDataType, DbValue, ParameterValue, RowSet,
};
use tokio_postgres::types::Type;
use tokio_postgres::{config::SslMode, types::ToSql, NoTls, Row};

/// Max connections in a given address' connection pool
const CONNECTION_POOL_SIZE: usize = 64;
/// Max addresses for which to keep pools in cache.
const CONNECTION_POOL_CACHE_CAPACITY: u64 = 16;

/// A factory object for Postgres clients. This abstracts
/// details of client creation such as pooling.
#[async_trait]
pub trait ClientFactory: Default + Send + Sync + 'static {
    /// The type of client produced by `get_client`.
    type Client: Client;
    /// Gets a client from the factory.
    async fn get_client(&self, address: &str) -> Result<Self::Client>;
}

/// A `ClientFactory` that uses a connection pool per address.
pub struct PooledTokioClientFactory {
    pools: moka::sync::Cache<String, deadpool_postgres::Pool>,
}

impl Default for PooledTokioClientFactory {
    fn default() -> Self {
        Self {
            pools: moka::sync::Cache::new(CONNECTION_POOL_CACHE_CAPACITY),
        }
    }
}

#[async_trait]
impl ClientFactory for PooledTokioClientFactory {
    type Client = deadpool_postgres::Object;

    async fn get_client(&self, address: &str) -> Result<Self::Client> {
        let pool = self
            .pools
            .try_get_with_by_ref(address, || create_connection_pool(address))
            .map_err(ArcError)
            .context("establishing PostgreSQL connection pool")?;

        Ok(pool.get().await?)
    }
}

/// Creates a Postgres connection pool for the given address.
fn create_connection_pool(address: &str) -> Result<deadpool_postgres::Pool> {
    let config = address
        .parse::<tokio_postgres::Config>()
        .context("parsing Postgres connection string")?;

    tracing::debug!("Build new connection: {}", address);

    let mgr_config = deadpool_postgres::ManagerConfig {
        recycling_method: deadpool_postgres::RecyclingMethod::Clean,
    };

    let mgr = if config.get_ssl_mode() == SslMode::Disable {
        deadpool_postgres::Manager::from_config(config, NoTls, mgr_config)
    } else {
        let builder = TlsConnector::builder();
        let connector = MakeTlsConnector::new(builder.build()?);
        deadpool_postgres::Manager::from_config(config, connector, mgr_config)
    };

    // TODO: what is our max size heuristic?  Should this be passed in so that different
    // hosts can manage it according to their needs?  Will a plain number suffice for
    // sophisticated hosts anyway?
    let pool = deadpool_postgres::Pool::builder(mgr)
        .max_size(CONNECTION_POOL_SIZE)
        .build()
        .context("building Postgres connection pool")?;

    Ok(pool)
}

#[async_trait]
pub trait Client: Send + Sync + 'static {
    async fn execute(
        &self,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<u64, v4::Error>;

    async fn query(
        &self,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<RowSet, v4::Error>;
}

#[async_trait]
impl Client for deadpool_postgres::Object {
    async fn execute(
        &self,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<u64, v4::Error> {
        let params = params
            .iter()
            .map(to_sql_parameter)
            .collect::<Result<Vec<_>>>()
            .map_err(|e| v4::Error::ValueConversionFailed(format!("{e:?}")))?;

        let params_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();

        self.as_ref()
            .execute(&statement, params_refs.as_slice())
            .await
            .map_err(|e| v4::Error::QueryFailed(format!("{e:?}")))
    }

    async fn query(
        &self,
        statement: String,
        params: Vec<ParameterValue>,
    ) -> Result<RowSet, v4::Error> {
        let params = params
            .iter()
            .map(to_sql_parameter)
            .collect::<Result<Vec<_>>>()
            .map_err(|e| v4::Error::BadParameter(format!("{e:?}")))?;

        let params_refs: Vec<&(dyn ToSql + Sync)> = params
            .iter()
            .map(|b| b.as_ref() as &(dyn ToSql + Sync))
            .collect();

        let results = self
            .as_ref()
            .query(&statement, params_refs.as_slice())
            .await
            .map_err(|e| v4::Error::QueryFailed(format!("{e:?}")))?;

        if results.is_empty() {
            return Ok(RowSet {
                columns: vec![],
                rows: vec![],
            });
        }

        let columns = infer_columns(&results[0]);
        let rows = results
            .iter()
            .map(convert_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| v4::Error::QueryFailed(format!("{e:?}")))?;

        Ok(RowSet { columns, rows })
    }
}

fn to_sql_parameter(value: &ParameterValue) -> Result<Box<dyn ToSql + Send + Sync>> {
    match value {
        ParameterValue::Boolean(v) => Ok(Box::new(*v)),
        ParameterValue::Int32(v) => Ok(Box::new(*v)),
        ParameterValue::Int64(v) => Ok(Box::new(*v)),
        ParameterValue::Int8(v) => Ok(Box::new(*v)),
        ParameterValue::Int16(v) => Ok(Box::new(*v)),
        ParameterValue::Floating32(v) => Ok(Box::new(*v)),
        ParameterValue::Floating64(v) => Ok(Box::new(*v)),
        ParameterValue::Str(v) => Ok(Box::new(v.clone())),
        ParameterValue::Binary(v) => Ok(Box::new(v.clone())),
        ParameterValue::Date((y, mon, d)) => {
            let naive_date = chrono::NaiveDate::from_ymd_opt(*y, (*mon).into(), (*d).into())
                .ok_or_else(|| anyhow!("invalid date y={y}, m={mon}, d={d}"))?;
            Ok(Box::new(naive_date))
        }
        ParameterValue::Time((h, min, s, ns)) => {
            let naive_time =
                chrono::NaiveTime::from_hms_nano_opt((*h).into(), (*min).into(), (*s).into(), *ns)
                    .ok_or_else(|| anyhow!("invalid time {h}:{min}:{s}:{ns}"))?;
            Ok(Box::new(naive_time))
        }
        ParameterValue::Datetime((y, mon, d, h, min, s, ns)) => {
            let naive_date = chrono::NaiveDate::from_ymd_opt(*y, (*mon).into(), (*d).into())
                .ok_or_else(|| anyhow!("invalid date y={y}, m={mon}, d={d}"))?;
            let naive_time =
                chrono::NaiveTime::from_hms_nano_opt((*h).into(), (*min).into(), (*s).into(), *ns)
                    .ok_or_else(|| anyhow!("invalid time {h}:{min}:{s}:{ns}"))?;
            let dt = chrono::NaiveDateTime::new(naive_date, naive_time);
            Ok(Box::new(dt))
        }
        ParameterValue::Timestamp(v) => {
            let ts = chrono::DateTime::<chrono::Utc>::from_timestamp(*v, 0)
                .ok_or_else(|| anyhow!("invalid epoch timestamp {v}"))?;
            Ok(Box::new(ts))
        }
        ParameterValue::Uuid(v) => {
            let u = uuid::Uuid::parse_str(v).with_context(|| format!("invalid UUID {v}"))?;
            Ok(Box::new(u))
        }
        ParameterValue::Jsonb(v) => {
            let j: serde_json::Value = serde_json::from_slice(v)
                .with_context(|| format!("invalid JSON {}", String::from_utf8_lossy(v)))?;
            Ok(Box::new(j))
        }
        ParameterValue::Decimal(v) => {
            let dec = rust_decimal::Decimal::from_str_exact(v)
                .with_context(|| format!("invalid decimal {v}"))?;
            Ok(Box::new(dec))
        }
        ParameterValue::Range32((lower, upper)) => {
            let lbound = lower.map(|(value, kind)| {
                postgres_range::RangeBound::new(value, range_bound_kind(kind))
            });
            let ubound = upper.map(|(value, kind)| {
                postgres_range::RangeBound::new(value, range_bound_kind(kind))
            });
            let r = postgres_range::Range::new(lbound, ubound);
            Ok(Box::new(r))
        }
        ParameterValue::Range64((lower, upper)) => {
            let lbound = lower.map(|(value, kind)| {
                postgres_range::RangeBound::new(value, range_bound_kind(kind))
            });
            let ubound = upper.map(|(value, kind)| {
                postgres_range::RangeBound::new(value, range_bound_kind(kind))
            });
            let r = postgres_range::Range::new(lbound, ubound);
            Ok(Box::new(r))
        }
        ParameterValue::DbNull => Ok(Box::new(PgNull)),
    }
}

fn range_bound_kind(wit_kind: v4::RangeBoundKind) -> postgres_range::BoundType {
    match wit_kind {
        v4::RangeBoundKind::Inclusive => postgres_range::BoundType::Inclusive,
        v4::RangeBoundKind::Exclusive => postgres_range::BoundType::Exclusive,
    }
}

fn infer_columns(row: &Row) -> Vec<Column> {
    let mut result = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        result.push(infer_column(row, index));
    }
    result
}

fn infer_column(row: &Row, index: usize) -> Column {
    let column = &row.columns()[index];
    let name = column.name().to_owned();
    let data_type = convert_data_type(column.type_());
    Column { name, data_type }
}

fn convert_data_type(pg_type: &Type) -> DbDataType {
    match *pg_type {
        Type::BOOL => DbDataType::Boolean,
        Type::BYTEA => DbDataType::Binary,
        Type::FLOAT4 => DbDataType::Floating32,
        Type::FLOAT8 => DbDataType::Floating64,
        Type::INT2 => DbDataType::Int16,
        Type::INT4 => DbDataType::Int32,
        Type::INT8 => DbDataType::Int64,
        Type::TEXT | Type::VARCHAR | Type::BPCHAR => DbDataType::Str,
        Type::TIMESTAMP | Type::TIMESTAMPTZ => DbDataType::Timestamp,
        Type::DATE => DbDataType::Date,
        Type::TIME => DbDataType::Time,
        Type::UUID => DbDataType::Uuid,
        Type::JSONB => DbDataType::Jsonb,
        Type::NUMERIC => DbDataType::Decimal,
        Type::INT4_RANGE => DbDataType::Range32,
        Type::INT8_RANGE => DbDataType::Range64,
        _ => {
            tracing::debug!("Couldn't convert Postgres type {} to WIT", pg_type.name(),);
            DbDataType::Other
        }
    }
}

fn convert_row(row: &Row) -> anyhow::Result<Vec<DbValue>> {
    let mut result = Vec::with_capacity(row.len());
    for index in 0..row.len() {
        result.push(convert_entry(row, index)?);
    }
    Ok(result)
}

fn convert_entry(row: &Row, index: usize) -> anyhow::Result<DbValue> {
    let column = &row.columns()[index];
    let value = match column.type_() {
        &Type::BOOL => {
            let value: Option<bool> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Boolean(v),
                None => DbValue::DbNull,
            }
        }
        &Type::BYTEA => {
            let value: Option<Vec<u8>> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Binary(v),
                None => DbValue::DbNull,
            }
        }
        &Type::FLOAT4 => {
            let value: Option<f32> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Floating32(v),
                None => DbValue::DbNull,
            }
        }
        &Type::FLOAT8 => {
            let value: Option<f64> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Floating64(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT2 => {
            let value: Option<i16> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int16(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT4 => {
            let value: Option<i32> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int32(v),
                None => DbValue::DbNull,
            }
        }
        &Type::INT8 => {
            let value: Option<i64> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Int64(v),
                None => DbValue::DbNull,
            }
        }
        &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR => {
            let value: Option<String> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Str(v),
                None => DbValue::DbNull,
            }
        }
        &Type::TIMESTAMP | &Type::TIMESTAMPTZ => {
            let value: Option<chrono::NaiveDateTime> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Datetime(tuplify_date_time(v)?),
                None => DbValue::DbNull,
            }
        }
        &Type::DATE => {
            let value: Option<chrono::NaiveDate> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Date(tuplify_date(v)?),
                None => DbValue::DbNull,
            }
        }
        &Type::TIME => {
            let value: Option<chrono::NaiveTime> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Time(tuplify_time(v)?),
                None => DbValue::DbNull,
            }
        }
        &Type::UUID => {
            let value: Option<uuid::Uuid> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Uuid(v.to_string()),
                None => DbValue::DbNull,
            }
        }
        &Type::JSONB => {
            let value: Option<serde_json::Value> = row.try_get(index)?;
            match value {
                Some(v) => {
                    DbValue::Jsonb(serde_json::to_vec(&v).context("invalid JSON from database")?)
                }
                None => DbValue::DbNull,
            }
        }
        &Type::NUMERIC => {
            let value: Option<rust_decimal::Decimal> = row.try_get(index)?;
            match value {
                Some(v) => DbValue::Decimal(v.to_string()),
                None => DbValue::DbNull,
            }
        }
        &Type::INT4_RANGE => {
            let value: Option<postgres_range::Range<i32>> = row.try_get(index)?;
            match value {
                Some(v) => {
                    let lower = v.lower().map(tuplify_range_bound);
                    let upper = v.lower().map(tuplify_range_bound);
                    DbValue::Range32((lower, upper))
                }
                None => DbValue::DbNull,
            }
        }
        &Type::INT8_RANGE => {
            let value: Option<postgres_range::Range<i64>> = row.try_get(index)?;
            match value {
                Some(v) => {
                    let lower = v.lower().map(tuplify_range_bound);
                    let upper = v.lower().map(tuplify_range_bound);
                    DbValue::Range64((lower, upper))
                }
                None => DbValue::DbNull,
            }
        }
        t => {
            tracing::debug!(
                "Couldn't convert Postgres type {} in column {}",
                t.name(),
                column.name()
            );
            DbValue::Unsupported
        }
    };
    Ok(value)
}

fn tuplify_range_bound<S: postgres_range::BoundSided, T: Copy>(
    value: &postgres_range::RangeBound<S, T>,
) -> (T, v4::RangeBoundKind) {
    (value.value, wit_bound_kind(value.type_))
}

fn wit_bound_kind(bound_type: postgres_range::BoundType) -> v4::RangeBoundKind {
    match bound_type {
        postgres_range::BoundType::Inclusive => v4::RangeBoundKind::Inclusive,
        postgres_range::BoundType::Exclusive => v4::RangeBoundKind::Exclusive,
    }
}

// Functions to convert from the chrono types to the WIT interface tuples
fn tuplify_date_time(
    value: chrono::NaiveDateTime,
) -> anyhow::Result<(i32, u8, u8, u8, u8, u8, u32)> {
    use chrono::{Datelike, Timelike};
    Ok((
        value.year(),
        value.month().try_into()?,
        value.day().try_into()?,
        value.hour().try_into()?,
        value.minute().try_into()?,
        value.second().try_into()?,
        value.nanosecond(),
    ))
}

fn tuplify_date(value: chrono::NaiveDate) -> anyhow::Result<(i32, u8, u8)> {
    use chrono::Datelike;
    Ok((
        value.year(),
        value.month().try_into()?,
        value.day().try_into()?,
    ))
}

fn tuplify_time(value: chrono::NaiveTime) -> anyhow::Result<(u8, u8, u8, u32)> {
    use chrono::Timelike;
    Ok((
        value.hour().try_into()?,
        value.minute().try_into()?,
        value.second().try_into()?,
        value.nanosecond(),
    ))
}

/// Although the Postgres crate converts Rust Option::None to Postgres NULL,
/// it enforces the type of the Option as it does so. (For example, trying to
/// pass an Option::<i32>::None to a VARCHAR column fails conversion.) As we
/// do not know expected column types, we instead use a "neutral" custom type
/// which allows conversion to any type but always tells the Postgres crate to
/// treat it as a SQL NULL.
struct PgNull;

impl ToSql for PgNull {
    fn to_sql(
        &self,
        _ty: &Type,
        _out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>>
    where
        Self: Sized,
    {
        Ok(tokio_postgres::types::IsNull::Yes)
    }

    fn accepts(_ty: &Type) -> bool
    where
        Self: Sized,
    {
        true
    }

    fn to_sql_checked(
        &self,
        _ty: &Type,
        _out: &mut tokio_postgres::types::private::BytesMut,
    ) -> Result<tokio_postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        Ok(tokio_postgres::types::IsNull::Yes)
    }
}

impl std::fmt::Debug for PgNull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NULL").finish()
    }
}

/// Workaround for moka returning Arc<Error> which, although
/// necessary for concurrency, does not play well with others.
struct ArcError(std::sync::Arc<anyhow::Error>);

impl std::error::Error for ArcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl std::fmt::Debug for ArcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for ArcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}
