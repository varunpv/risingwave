// Copyright 2024 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::str::FromStr;
use std::sync::LazyLock;

use apache_avro::schema::{DecimalSchema, RecordSchema};
use apache_avro::types::Value;
use apache_avro::{Decimal as AvroDecimal, Schema};
use chrono::Datelike;
use itertools::Itertools;
use num_bigint::{BigInt, Sign};
use risingwave_common::array::{ListValue, StructValue};
use risingwave_common::bail;
use risingwave_common::log::LogSuppresser;
use risingwave_common::types::{
    DataType, Date, Datum, Interval, JsonbVal, ScalarImpl, Time, Timestamp, Timestamptz,
};
use risingwave_common::util::iter_util::ZipEqFast;

use super::{bail_uncategorized, uncategorized, Access, AccessError, AccessResult};
use crate::error::ConnectorResult;
#[derive(Clone)]
/// Options for parsing an `AvroValue` into Datum, with an optional avro schema.
pub struct AvroParseOptions<'a> {
    pub schema: Option<&'a Schema>,
    /// Strict Mode
    /// If strict mode is disabled, an int64 can be parsed from an `AvroInt` (int32) value.
    pub relax_numeric: bool,
}

impl<'a> Default for AvroParseOptions<'a> {
    fn default() -> Self {
        Self {
            schema: None,
            relax_numeric: true,
        }
    }
}

impl<'a> AvroParseOptions<'a> {
    pub fn with_schema(mut self, schema: &'a Schema) -> Self {
        self.schema = Some(schema);
        self
    }

    fn extract_inner_schema(&self, key: Option<&'a str>) -> Option<&'a Schema> {
        self.schema
            .map(|schema| avro_extract_field_schema(schema, key))
            .transpose()
            .map_err(|_err| {
                static LOG_SUPPERSSER: LazyLock<LogSuppresser> =
                    LazyLock::new(LogSuppresser::default);
                if let Ok(suppressed_count) = LOG_SUPPERSSER.check() {
                    tracing::error!(suppressed_count, "extract sub-schema");
                }
            })
            .ok()
            .flatten()
    }

    /// Parse an avro value into expected type.
    /// 3 kinds of type info are used to parsing things.
    ///     - `type_expected`. The type that we expect the value is.
    ///     - value type. The type info together with the value argument.
    ///     - schema. The `AvroSchema` provided in option.
    /// If both `type_expected` and schema are provided, it will check both strictly.
    /// If only `type_expected` is provided, it will try to match the value type and the
    /// `type_expected`, converting the value if possible. If only value is provided (without
    /// schema and `type_expected`), the `DateType` will be inferred.
    pub fn parse<'b>(&self, value: &'b Value, type_expected: Option<&'b DataType>) -> AccessResult
    where
        'b: 'a,
    {
        let create_error = || AccessError::TypeError {
            expected: format!("{:?}", type_expected),
            got: format!("{:?}", value),
            value: String::new(),
        };

        let v: ScalarImpl = match (type_expected, value) {
            (_, Value::Null) => return Ok(None),
            (_, Value::Union(_, v)) => {
                let schema = self.extract_inner_schema(None);
                return Self {
                    schema,
                    relax_numeric: self.relax_numeric,
                }
                .parse(v, type_expected);
            }
            // ---- Boolean -----
            (Some(DataType::Boolean) | None, Value::Boolean(b)) => (*b).into(),
            // ---- Int16 -----
            (Some(DataType::Int16), Value::Int(i)) if self.relax_numeric => (*i as i16).into(),
            (Some(DataType::Int16), Value::Long(i)) if self.relax_numeric => (*i as i16).into(),

            // ---- Int32 -----
            (Some(DataType::Int32) | None, Value::Int(i)) => (*i).into(),
            (Some(DataType::Int32), Value::Long(i)) if self.relax_numeric => (*i as i32).into(),
            // ---- Int64 -----
            (Some(DataType::Int64) | None, Value::Long(i)) => (*i).into(),
            (Some(DataType::Int64), Value::Int(i)) if self.relax_numeric => (*i as i64).into(),
            // ---- Float32 -----
            (Some(DataType::Float32) | None, Value::Float(i)) => (*i).into(),
            (Some(DataType::Float32), Value::Double(i)) => (*i as f32).into(),
            // ---- Float64 -----
            (Some(DataType::Float64) | None, Value::Double(i)) => (*i).into(),
            (Some(DataType::Float64), Value::Float(i)) => (*i as f64).into(),
            // ---- Decimal -----
            (Some(DataType::Decimal) | None, Value::Decimal(avro_decimal)) => {
                let (precision, scale) = match self.schema {
                    Some(Schema::Decimal(DecimalSchema {
                        precision, scale, ..
                    })) => (*precision, *scale),
                    _ => Err(create_error())?,
                };
                let decimal = avro_decimal_to_rust_decimal(avro_decimal.clone(), precision, scale)
                    .map_err(|_| create_error())?;
                ScalarImpl::Decimal(risingwave_common::types::Decimal::Normalized(decimal))
            }
            (Some(DataType::Decimal), Value::Record(fields)) => {
                // VariableScaleDecimal has fixed fields, scale(int) and value(bytes)
                let find_in_records = |field_name: &str| {
                    fields
                        .iter()
                        .find(|field| field.0 == field_name)
                        .map(|field| &field.1)
                        .ok_or_else(|| {
                            uncategorized!("`{field_name}` field not found in VariableScaleDecimal")
                        })
                };
                let scale = match find_in_records("scale")? {
                    Value::Int(scale) => *scale,
                    avro_value => bail_uncategorized!(
                        "scale field in VariableScaleDecimal is not int, got {:?}",
                        avro_value
                    ),
                };

                let value: BigInt = match find_in_records("value")? {
                    Value::Bytes(bytes) => BigInt::from_signed_bytes_be(bytes),
                    avro_value => bail_uncategorized!(
                        "value field in VariableScaleDecimal is not bytes, got {:?}",
                        avro_value
                    ),
                };

                let negative = value.sign() == Sign::Minus;
                let (lo, mid, hi) = extract_decimal(value.to_bytes_be().1)?;
                let decimal =
                    rust_decimal::Decimal::from_parts(lo, mid, hi, negative, scale as u32);
                ScalarImpl::Decimal(risingwave_common::types::Decimal::Normalized(decimal))
            }
            // ---- Time -----
            (Some(DataType::Time), Value::TimeMillis(ms)) => Time::with_milli(*ms as u32)
                .map_err(|_| create_error())?
                .into(),
            (Some(DataType::Time), Value::TimeMicros(us)) => Time::with_micro(*us as u64)
                .map_err(|_| create_error())?
                .into(),
            // ---- Date -----
            (Some(DataType::Date) | None, Value::Date(days)) => {
                Date::with_days(days + unix_epoch_days())
                    .map_err(|_| create_error())?
                    .into()
            }
            // ---- Varchar -----
            (Some(DataType::Varchar) | None, Value::Enum(_, symbol)) => {
                symbol.clone().into_boxed_str().into()
            }
            (Some(DataType::Varchar) | None, Value::String(s)) => s.clone().into_boxed_str().into(),
            // ---- Timestamp -----
            (Some(DataType::Timestamp) | None, Value::LocalTimestampMillis(ms)) => {
                Timestamp::with_millis(*ms)
                    .map_err(|_| create_error())?
                    .into()
            }
            (Some(DataType::Timestamp) | None, Value::LocalTimestampMicros(us)) => {
                Timestamp::with_micros(*us)
                    .map_err(|_| create_error())?
                    .into()
            }

            // ---- TimestampTz -----
            (Some(DataType::Timestamptz) | None, Value::TimestampMillis(ms)) => {
                Timestamptz::from_millis(*ms)
                    .ok_or_else(|| {
                        uncategorized!("timestamptz with milliseconds {ms} * 1000 is out of range")
                    })?
                    .into()
            }
            (Some(DataType::Timestamptz) | None, Value::TimestampMicros(us)) => {
                Timestamptz::from_micros(*us).into()
            }

            // ---- Interval -----
            (Some(DataType::Interval) | None, Value::Duration(duration)) => {
                let months = u32::from(duration.months()) as i32;
                let days = u32::from(duration.days()) as i32;
                let usecs = (u32::from(duration.millis()) as i64) * 1000; // never overflows
                ScalarImpl::Interval(Interval::from_month_day_usec(months, days, usecs))
            }
            // ---- Struct -----
            (Some(DataType::Struct(struct_type_info)), Value::Record(descs)) => StructValue::new(
                struct_type_info
                    .names()
                    .zip_eq_fast(struct_type_info.types())
                    .map(|(field_name, field_type)| {
                        let maybe_value = descs.iter().find(|(k, _v)| k == field_name);
                        if let Some((_, value)) = maybe_value {
                            let schema = self.extract_inner_schema(Some(field_name));
                            Ok(Self {
                                schema,
                                relax_numeric: self.relax_numeric,
                            }
                            .parse(value, Some(field_type))?)
                        } else {
                            Ok(None)
                        }
                    })
                    .collect::<Result<_, AccessError>>()?,
            )
            .into(),
            (None, Value::Record(descs)) => {
                let rw_values = descs
                    .iter()
                    .map(|(field_name, field_value)| {
                        let schema = self.extract_inner_schema(Some(field_name));
                        Self {
                            schema,
                            relax_numeric: self.relax_numeric,
                        }
                        .parse(field_value, None)
                    })
                    .collect::<Result<Vec<Datum>, AccessError>>()?;
                ScalarImpl::Struct(StructValue::new(rw_values))
            }
            // ---- List -----
            (Some(DataType::List(item_type)), Value::Array(array)) => ListValue::new({
                let schema = self.extract_inner_schema(None);
                let mut builder = item_type.create_array_builder(array.len());
                for v in array {
                    let value = Self {
                        schema,
                        relax_numeric: self.relax_numeric,
                    }
                    .parse(v, Some(item_type))?;
                    builder.append(value);
                }
                builder.finish()
            })
            .into(),
            // ---- Bytea -----
            (Some(DataType::Bytea) | None, Value::Bytes(value)) => {
                value.clone().into_boxed_slice().into()
            }
            // ---- Jsonb -----
            (Some(DataType::Jsonb), Value::String(s)) => {
                JsonbVal::from_str(s).map_err(|_| create_error())?.into()
            }

            (_expected, _got) => Err(create_error())?,
        };
        Ok(Some(v))
    }
}

pub struct AvroAccess<'a, 'b> {
    value: &'a Value,
    options: AvroParseOptions<'b>,
}

impl<'a, 'b> AvroAccess<'a, 'b> {
    pub fn new(value: &'a Value, options: AvroParseOptions<'b>) -> Self {
        Self { value, options }
    }
}

impl<'a, 'b> Access for AvroAccess<'a, 'b>
where
    'a: 'b,
{
    fn access(&self, path: &[&str], type_expected: Option<&DataType>) -> AccessResult {
        let mut value = self.value;
        let mut options: AvroParseOptions<'_> = self.options.clone();

        let mut i = 0;
        while i < path.len() {
            let key = path[i];
            let create_error = || AccessError::Undefined {
                name: key.to_string(),
                path: path.iter().take(i).join("."),
            };
            match value {
                Value::Union(_, v) => {
                    value = v;
                    options.schema = options.extract_inner_schema(None);
                    continue;
                }
                Value::Map(fields) if fields.contains_key(key) => {
                    value = fields.get(key).unwrap();
                    options.schema = None;
                    i += 1;
                    continue;
                }
                Value::Record(fields) => {
                    if let Some((_, v)) = fields.iter().find(|(k, _)| k == key) {
                        value = v;
                        options.schema = options.extract_inner_schema(Some(key));
                        i += 1;
                        continue;
                    }
                }
                _ => (),
            }
            Err(create_error())?;
        }

        options.parse(value, type_expected)
    }
}

pub(crate) fn avro_decimal_to_rust_decimal(
    avro_decimal: AvroDecimal,
    _precision: usize,
    scale: usize,
) -> AccessResult<rust_decimal::Decimal> {
    let negative = !avro_decimal.is_positive();
    let bytes = avro_decimal.to_vec_unsigned();

    let (lo, mid, hi) = extract_decimal(bytes)?;
    Ok(rust_decimal::Decimal::from_parts(
        lo,
        mid,
        hi,
        negative,
        scale as u32,
    ))
}

pub(crate) fn extract_decimal(bytes: Vec<u8>) -> AccessResult<(u32, u32, u32)> {
    match bytes.len() {
        len @ 0..=4 => {
            let mut pad = vec![0; 4 - len];
            pad.extend_from_slice(&bytes);
            let lo = u32::from_be_bytes(pad.try_into().unwrap());
            Ok((lo, 0, 0))
        }
        len @ 5..=8 => {
            let zero_len = 8 - len;
            let mid_end = 4 - zero_len;

            let mut pad = vec![0; zero_len];
            pad.extend_from_slice(&bytes[..mid_end]);
            let mid = u32::from_be_bytes(pad.try_into().unwrap());

            let lo = u32::from_be_bytes(bytes[mid_end..].to_owned().try_into().unwrap());
            Ok((lo, mid, 0))
        }
        len @ 9..=12 => {
            let zero_len = 12 - len;
            let hi_end = 4 - zero_len;
            let mid_end = hi_end + 4;

            let mut pad = vec![0; zero_len];
            pad.extend_from_slice(&bytes[..hi_end]);
            let hi = u32::from_be_bytes(pad.try_into().unwrap());

            let mid = u32::from_be_bytes(bytes[hi_end..mid_end].to_owned().try_into().unwrap());

            let lo = u32::from_be_bytes(bytes[mid_end..].to_owned().try_into().unwrap());
            Ok((lo, mid, hi))
        }
        _ => bail_uncategorized!("invalid decimal bytes length {}", bytes.len()),
    }
}

pub fn avro_schema_skip_union(schema: &Schema) -> ConnectorResult<&Schema> {
    match schema {
        Schema::Union(union_schema) => {
            let inner_schema = union_schema
                .variants()
                .iter()
                .find(|s| !matches!(s, &&Schema::Null))
                .ok_or_else(|| {
                    anyhow::format_err!("illegal avro record schema {:?}", union_schema)
                })?;
            Ok(inner_schema)
        }
        other => Ok(other),
    }
}
// extract inner filed/item schema of record/array/union
pub fn avro_extract_field_schema<'a>(
    schema: &'a Schema,
    name: Option<&'a str>,
) -> ConnectorResult<&'a Schema> {
    match schema {
        Schema::Record(RecordSchema { fields, lookup, .. }) => {
            let name =
                name.ok_or_else(|| anyhow::format_err!("no name provided for a field in record"))?;
            let index = lookup.get(name).ok_or_else(|| {
                anyhow::format_err!("no field named '{}' in record: {:?}", name, schema)
            })?;
            let field = fields
                .get(*index)
                .ok_or_else(|| anyhow::format_err!("illegal avro record schema {:?}", schema))?;
            Ok(&field.schema)
        }
        Schema::Array(schema) => Ok(schema),
        Schema::Union(_) => avro_schema_skip_union(schema),
        _ => bail!("avro schema is not a record or array"),
    }
}

pub(crate) fn unix_epoch_days() -> i32 {
    Date::from_ymd_uncheck(1970, 1, 1).0.num_days_from_ce()
}

#[cfg(test)]
mod tests {
    use apache_avro::Decimal as AvroDecimal;
    use risingwave_common::types::Decimal;

    use super::*;

    #[test]
    fn test_convert_decimal() {
        // 280
        let v = vec![1, 24];
        let avro_decimal = AvroDecimal::from(v);
        let rust_decimal = avro_decimal_to_rust_decimal(avro_decimal, 28, 0).unwrap();
        assert_eq!(rust_decimal, rust_decimal::Decimal::from(280));

        // 28.1
        let v = vec![1, 25];
        let avro_decimal = AvroDecimal::from(v);
        let rust_decimal = avro_decimal_to_rust_decimal(avro_decimal, 28, 1).unwrap();
        assert_eq!(rust_decimal, rust_decimal::Decimal::try_from(28.1).unwrap());

        // 1.1234567891
        let value = BigInt::from(11234567891_i64);
        let negative = value.sign() == Sign::Minus;
        let (lo, mid, hi) = extract_decimal(value.to_bytes_be().1).unwrap();
        let decimal = rust_decimal::Decimal::from_parts(lo, mid, hi, negative, 10);
        assert_eq!(
            decimal,
            rust_decimal::Decimal::try_from(1.1234567891).unwrap()
        );

        // 1.123456789123456789123456789
        let v = vec![3, 161, 77, 58, 146, 180, 49, 220, 100, 4, 95, 21];
        let avro_decimal = AvroDecimal::from(v);
        let rust_decimal = avro_decimal_to_rust_decimal(avro_decimal, 28, 27).unwrap();
        assert_eq!(
            rust_decimal,
            rust_decimal::Decimal::from_str("1.123456789123456789123456789").unwrap()
        );
    }

    /// Convert Avro value to datum.For now, support the following [Avro type](https://avro.apache.org/docs/current/spec.html).
    ///  - boolean
    ///  - int : i32
    ///  - long: i64
    ///  - float: f32
    ///  - double: f64
    ///  - string: String
    ///  - Date (the number of days from the unix epoch, 1970-1-1 UTC)
    ///  - Timestamp (the number of milliseconds from the unix epoch,  1970-1-1 00:00:00.000 UTC)
    fn from_avro_value(
        value: Value,
        value_schema: &Schema,
        shape: &DataType,
    ) -> crate::error::ConnectorResult<Datum> {
        AvroParseOptions {
            schema: Some(value_schema),
            relax_numeric: true,
        }
        .parse(&value, Some(shape))
        .map_err(Into::into)
    }

    #[test]
    fn test_avro_timestamptz_micros() {
        let v1 = Value::TimestampMicros(1620000000000000);
        let v2 = Value::TimestampMillis(1620000000000);
        let value_schema1 = Schema::TimestampMicros;
        let value_schema2 = Schema::TimestampMillis;
        let datum1 = from_avro_value(v1, &value_schema1, &DataType::Timestamptz).unwrap();
        let datum2 = from_avro_value(v2, &value_schema2, &DataType::Timestamptz).unwrap();
        assert_eq!(
            datum1,
            Some(ScalarImpl::Timestamptz(
                Timestamptz::from_str("2021-05-03T00:00:00Z").unwrap()
            ))
        );
        assert_eq!(
            datum2,
            Some(ScalarImpl::Timestamptz(
                Timestamptz::from_str("2021-05-03T00:00:00Z").unwrap()
            ))
        );
    }

    #[test]
    fn test_decimal_truncate() {
        let schema = Schema::parse_str(
            r#"
            {
                "type": "bytes",
                "logicalType": "decimal",
                "precision": 38,
                "scale": 18
            }
            "#,
        )
        .unwrap();
        let bytes = vec![0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f];
        let value = Value::Decimal(AvroDecimal::from(bytes));
        let options = AvroParseOptions::default().with_schema(&schema);
        let resp = options.parse(&value, Some(&DataType::Decimal)).unwrap();
        assert_eq!(
            resp,
            Some(ScalarImpl::Decimal(Decimal::Normalized(
                rust_decimal::Decimal::from_str("0.017802464409370431").unwrap()
            )))
        );
    }

    #[test]
    fn test_variable_scale_decimal() {
        let schema = Schema::parse_str(
            r#"
            {
                "type": "record",
                "name": "VariableScaleDecimal",
                "namespace": "io.debezium.data",
                "fields": [
                    {
                        "name": "scale",
                        "type": "int"
                    },
                    {
                        "name": "value",
                        "type": "bytes"
                    }
                ]
            }
            "#,
        )
        .unwrap();
        let value = Value::Record(vec![
            ("scale".to_string(), Value::Int(0)),
            ("value".to_string(), Value::Bytes(vec![0x01, 0x02, 0x03])),
        ]);

        let options = AvroParseOptions::default().with_schema(&schema);
        let resp = options.parse(&value, Some(&DataType::Decimal)).unwrap();
        assert_eq!(resp, Some(ScalarImpl::Decimal(Decimal::from(66051))));
    }
}
