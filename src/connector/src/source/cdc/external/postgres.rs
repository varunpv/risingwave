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

use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::Context;
use futures::stream::BoxStream;
use futures::{pin_mut, StreamExt};
use futures_async_stream::try_stream;
use itertools::Itertools;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use postgres_openssl::MakeTlsConnector;
use risingwave_common::catalog::Schema;
use risingwave_common::row::{OwnedRow, Row};
use risingwave_common::util::iter_util::ZipEqFast;
use serde_derive::{Deserialize, Serialize};
use thiserror_ext::AsReport;
use tokio_postgres::types::PgLsn;
use tokio_postgres::{NoTls, Statement};

use crate::error::{ConnectorError, ConnectorResult};
use crate::parser::postgres_row_to_owned_row;
use crate::parser::scalar_adapter::ScalarAdapter;
#[cfg(not(madsim))]
use crate::source::cdc::external::maybe_tls_connector::MaybeMakeTlsConnector;
use crate::source::cdc::external::{
    CdcOffset, CdcOffsetParseFunc, DebeziumOffset, ExternalTableConfig, ExternalTableReader,
    SchemaTableName, SslMode,
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PostgresOffset {
    pub txid: i64,
    // In postgres, an LSN is a 64-bit integer, representing a byte position in the write-ahead log stream.
    // It is printed as two hexadecimal numbers of up to 8 digits each, separated by a slash; for example, 16/B374D848
    pub lsn: u64,
}

// only compare the lsn field
impl PartialOrd for PostgresOffset {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.lsn.partial_cmp(&other.lsn)
    }
}

impl PostgresOffset {
    pub fn parse_debezium_offset(offset: &str) -> ConnectorResult<Self> {
        let dbz_offset: DebeziumOffset = serde_json::from_str(offset)
            .with_context(|| format!("invalid upstream offset: {}", offset))?;

        Ok(Self {
            txid: dbz_offset
                .source_offset
                .txid
                .context("invalid postgres txid")?,
            lsn: dbz_offset
                .source_offset
                .lsn
                .context("invalid postgres lsn")?,
        })
    }
}

pub struct PostgresExternalTableReader {
    config: ExternalTableConfig,
    rw_schema: Schema,
    field_names: String,
    prepared_scan_stmt: Statement,
    client: tokio::sync::Mutex<tokio_postgres::Client>,
}

impl ExternalTableReader for PostgresExternalTableReader {
    async fn current_cdc_offset(&self) -> ConnectorResult<CdcOffset> {
        let mut client = self.client.lock().await;
        // start a transaction to read current lsn and txid
        let trxn = client.transaction().await?;
        let row = trxn.query_one("SELECT pg_current_wal_lsn()", &[]).await?;
        let mut pg_offset = PostgresOffset::default();
        let pg_lsn = row.get::<_, PgLsn>(0);
        tracing::debug!("current lsn: {}", pg_lsn);
        pg_offset.lsn = pg_lsn.into();

        let txid_row = trxn.query_one("SELECT txid_current()", &[]).await?;
        let txid: i64 = txid_row.get::<_, i64>(0);
        pg_offset.txid = txid;

        // commit the transaction
        trxn.commit().await?;

        Ok(CdcOffset::Postgres(pg_offset))
    }

    fn snapshot_read(
        &self,
        table_name: SchemaTableName,
        start_pk: Option<OwnedRow>,
        primary_keys: Vec<String>,
        limit: u32,
    ) -> BoxStream<'_, ConnectorResult<OwnedRow>> {
        self.snapshot_read_inner(table_name, start_pk, primary_keys, limit)
    }
}

impl PostgresExternalTableReader {
    pub async fn new(
        properties: HashMap<String, String>,
        rw_schema: Schema,
        pk_indices: Vec<usize>,
        scan_limit: u32,
    ) -> ConnectorResult<Self> {
        tracing::info!(
            ?rw_schema,
            ?pk_indices,
            "create postgres external table reader"
        );

        let config = serde_json::from_value::<ExternalTableConfig>(
            serde_json::to_value(properties.clone()).unwrap(),
        )
        .context("failed to extract postgres connector properties")?;

        let mut pg_config = tokio_postgres::Config::new();
        pg_config
            .user(&config.username)
            .password(&config.password)
            .host(&config.host)
            .port(config.port.parse::<u16>().unwrap())
            .dbname(&config.database);

        #[cfg(not(madsim))]
        let connector = match config.sslmode {
            SslMode::Disabled => {
                pg_config.ssl_mode(tokio_postgres::config::SslMode::Disable);
                MaybeMakeTlsConnector::NoTls(NoTls)
            }
            SslMode::Preferred => {
                pg_config.ssl_mode(tokio_postgres::config::SslMode::Prefer);
                match SslConnector::builder(SslMethod::tls()) {
                    Ok(mut builder) => {
                        // disable certificate verification for `prefer`
                        builder.set_verify(SslVerifyMode::NONE);
                        MaybeMakeTlsConnector::Tls(MakeTlsConnector::new(builder.build()))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e.as_report(), "SSL connector error");
                        MaybeMakeTlsConnector::NoTls(NoTls)
                    }
                }
            }
            SslMode::Required => {
                pg_config.ssl_mode(tokio_postgres::config::SslMode::Require);
                let mut builder = SslConnector::builder(SslMethod::tls())?;
                // disable certificate verification for `require`
                builder.set_verify(SslVerifyMode::NONE);
                MaybeMakeTlsConnector::Tls(MakeTlsConnector::new(builder.build()))
            }
        };
        #[cfg(madsim)]
        let connector = NoTls;

        let (client, connection) = pg_config.connect(connector).await?;

        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::error!(error = %e.as_report(), "postgres connection error");
            }
        });

        let field_names = rw_schema
            .fields
            .iter()
            .map(|f| Self::quote_column(&f.name))
            .join(",");

        // prepare once
        let prepared_scan_stmt = {
            let primary_keys = pk_indices
                .iter()
                .map(|i| rw_schema.fields[*i].name.clone())
                .collect_vec();

            let table_name = SchemaTableName::from_properties(&properties);
            let order_key = primary_keys.iter().join(",");
            let scan_sql = format!(
                "SELECT {} FROM {} WHERE {} ORDER BY {} LIMIT {scan_limit}",
                field_names,
                Self::get_normalized_table_name(&table_name),
                Self::filter_expression(&primary_keys),
                order_key,
            );
            client.prepare(&scan_sql).await?
        };

        Ok(Self {
            config,
            rw_schema,
            field_names,
            prepared_scan_stmt,
            client: tokio::sync::Mutex::new(client),
        })
    }

    pub fn get_normalized_table_name(table_name: &SchemaTableName) -> String {
        format!(
            "\"{}\".\"{}\"",
            table_name.schema_name, table_name.table_name
        )
    }

    pub fn get_cdc_offset_parser() -> CdcOffsetParseFunc {
        Box::new(move |offset| {
            Ok(CdcOffset::Postgres(PostgresOffset::parse_debezium_offset(
                offset,
            )?))
        })
    }

    #[try_stream(boxed, ok = OwnedRow, error = ConnectorError)]
    async fn snapshot_read_inner(
        &self,
        table_name: SchemaTableName,
        start_pk_row: Option<OwnedRow>,
        primary_keys: Vec<String>,
        limit: u32,
    ) {
        let order_key = primary_keys.iter().join(",");
        let client = self.client.lock().await;
        client.execute("set time zone '+00:00'", &[]).await?;

        let stream = match start_pk_row {
            Some(ref pk_row) => {
                let params: Vec<Option<ScalarAdapter<'_>>> = pk_row
                    .iter()
                    .zip_eq_fast(self.prepared_scan_stmt.params())
                    .map(|(datum, ty)| {
                        datum
                            .map(|scalar| ScalarAdapter::from_scalar(scalar, ty))
                            .transpose()
                    })
                    .try_collect()?;

                client.query_raw(&self.prepared_scan_stmt, &params).await?
            }
            None => {
                let sql = format!(
                    "SELECT {} FROM {} ORDER BY {} LIMIT {limit}",
                    self.field_names,
                    Self::get_normalized_table_name(&table_name),
                    order_key,
                );
                let params: Vec<Option<ScalarAdapter<'_>>> = vec![];
                client.query_raw(&sql, &params).await?
            }
        };

        let row_stream = stream.map(|row| {
            let row = row?;
            Ok::<_, crate::error::ConnectorError>(postgres_row_to_owned_row(row, &self.rw_schema))
        });

        pin_mut!(row_stream);
        #[for_await]
        for row in row_stream {
            let row = row?;
            yield row;
        }
    }

    // row filter expression: (v1, v2, v3) > ($1, $2, $3)
    fn filter_expression(columns: &[String]) -> String {
        let mut col_expr = String::new();
        let mut arg_expr = String::new();
        for (i, column) in columns.iter().enumerate() {
            if i > 0 {
                col_expr.push_str(", ");
                arg_expr.push_str(", ");
            }
            col_expr.push_str(&Self::quote_column(column));
            arg_expr.push_str(format!("${}", i + 1).as_str());
        }
        format!("({}) > ({})", col_expr, arg_expr)
    }

    fn quote_column(column: &str) -> String {
        format!("\"{}\"", column)
    }
}

#[cfg(test)]
mod tests {
    use futures::pin_mut;
    use futures_async_stream::for_await;
    use maplit::{convert_args, hashmap};
    use risingwave_common::catalog::{ColumnDesc, ColumnId, Field, Schema};
    use risingwave_common::row::OwnedRow;
    use risingwave_common::types::{DataType, ScalarImpl};

    use crate::source::cdc::external::postgres::{PostgresExternalTableReader, PostgresOffset};
    use crate::source::cdc::external::{ExternalTableReader, SchemaTableName};

    #[test]
    fn test_postgres_offset() {
        let off1 = PostgresOffset { txid: 4, lsn: 2 };
        let off2 = PostgresOffset { txid: 1, lsn: 3 };
        let off3 = PostgresOffset { txid: 5, lsn: 1 };

        assert!(off1 < off2);
        assert!(off3 < off1);
        assert!(off2 > off3);
    }

    #[test]
    fn test_filter_expression() {
        let cols = vec!["v1".to_string()];
        let expr = PostgresExternalTableReader::filter_expression(&cols);
        assert_eq!(expr, "(\"v1\") > ($1)");

        let cols = vec!["v1".to_string(), "v2".to_string()];
        let expr = PostgresExternalTableReader::filter_expression(&cols);
        assert_eq!(expr, "(\"v1\", \"v2\") > ($1, $2)");

        let cols = vec!["v1".to_string(), "v2".to_string(), "v3".to_string()];
        let expr = PostgresExternalTableReader::filter_expression(&cols);
        assert_eq!(expr, "(\"v1\", \"v2\", \"v3\") > ($1, $2, $3)");
    }

    // manual test
    #[ignore]
    #[tokio::test]
    async fn test_pg_table_reader() {
        let columns = vec![
            ColumnDesc::named("v1", ColumnId::new(1), DataType::Int32),
            ColumnDesc::named("v2", ColumnId::new(2), DataType::Varchar),
            ColumnDesc::named("v3", ColumnId::new(3), DataType::Decimal),
            ColumnDesc::named("v4", ColumnId::new(4), DataType::Date),
        ];
        let rw_schema = Schema {
            fields: columns.iter().map(Field::from).collect(),
        };

        let props = convert_args!(hashmap!(
                "hostname" => "localhost",
                "port" => "8432",
                "username" => "myuser",
                "password" => "123456",
                "database.name" => "mydb",
                "schema.name" => "public",
                "table.name" => "t1"));
        let reader = PostgresExternalTableReader::new(props, rw_schema, vec![0, 1], 1000)
            .await
            .unwrap();

        let offset = reader.current_cdc_offset().await.unwrap();
        println!("CdcOffset: {:?}", offset);

        let start_pk = OwnedRow::new(vec![Some(ScalarImpl::from(3)), Some(ScalarImpl::from("c"))]);
        let stream = reader.snapshot_read(
            SchemaTableName {
                schema_name: "public".to_string(),
                table_name: "t1".to_string(),
            },
            Some(start_pk),
            vec!["v1".to_string(), "v2".to_string()],
            1000,
        );

        pin_mut!(stream);
        #[for_await]
        for row in stream {
            println!("OwnedRow: {:?}", row);
        }
    }
}
