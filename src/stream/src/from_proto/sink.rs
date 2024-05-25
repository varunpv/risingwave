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

use std::sync::Arc;

use anyhow::anyhow;
use risingwave_common::catalog::{ColumnCatalog, Schema};
use risingwave_common::types::DataType;
use risingwave_connector::match_sink_name_str;
use risingwave_connector::sink::catalog::{SinkFormatDesc, SinkType};
use risingwave_connector::sink::{
    SinkError, SinkMetaClient, SinkParam, SinkWriterParam, CONNECTOR_TYPE_KEY, SINK_TYPE_OPTION,
};
use risingwave_pb::catalog::Table;
use risingwave_pb::plan_common::PbColumnCatalog;
use risingwave_pb::stream_plan::{SinkLogStoreType, SinkNode};

use super::*;
use crate::common::log_store_impl::in_mem::BoundedInMemLogStoreFactory;
use crate::common::log_store_impl::kv_log_store::{
    KvLogStoreFactory, KvLogStoreMetrics, KvLogStorePkInfo, KV_LOG_STORE_V2_INFO,
};
use crate::executor::SinkExecutor;

pub struct SinkExecutorBuilder;

fn resolve_pk_info(
    input_schema: &Schema,
    log_store_table: &Table,
) -> StreamResult<&'static KvLogStorePkInfo> {
    let predefined_column_len = log_store_table.columns.len() - input_schema.fields.len();

    #[expect(deprecated)]
    let info = match predefined_column_len {
        len if len
            == crate::common::log_store_impl::kv_log_store::KV_LOG_STORE_V1_INFO
                .predefined_column_len() =>
        {
            Ok(&crate::common::log_store_impl::kv_log_store::KV_LOG_STORE_V1_INFO)
        }
        len if len == KV_LOG_STORE_V2_INFO.predefined_column_len() => Ok(&KV_LOG_STORE_V2_INFO),
        other_len => Err(anyhow!(
            "invalid log store predefined len {}. log store table: {:?}, input_schema: {:?}",
            other_len,
            log_store_table,
            input_schema
        )),
    }?;
    validate_payload_schema(
        &log_store_table.columns[predefined_column_len..],
        input_schema,
    )?;
    Ok(info)
}

fn validate_payload_schema(
    log_store_payload_schema: &[PbColumnCatalog],
    input_schema: &Schema,
) -> StreamResult<()> {
    if log_store_payload_schema
        .iter()
        .zip_eq(input_schema.fields.iter())
        .map(|(log_store_col, input_field)| {
            let log_store_col_type = DataType::from(
                log_store_col
                    .column_desc
                    .as_ref()
                    .unwrap()
                    .column_type
                    .as_ref()
                    .unwrap(),
            );
            log_store_col_type.equals_datatype(&input_field.data_type)
        })
        .all(|equal| equal)
    {
        Ok(())
    } else {
        Err(anyhow!(
            "mismatch schema: log store: {:?}, input: {:?}",
            log_store_payload_schema,
            input_schema
        )
        .into())
    }
}

impl ExecutorBuilder for SinkExecutorBuilder {
    type Node = SinkNode;

    async fn new_boxed_executor(
        params: ExecutorParams,
        node: &Self::Node,
        state_store: impl StateStore,
    ) -> StreamResult<Executor> {
        let [input_executor]: [_; 1] = params.input.try_into().unwrap();
        let input_data_types = input_executor.info().schema.data_types();
        let chunk_size = params.env.config().developer.chunk_size;

        let sink_desc = node.sink_desc.as_ref().unwrap();
        let sink_type = SinkType::from_proto(sink_desc.get_sink_type().unwrap());
        let sink_id = sink_desc.get_id().into();
        let sink_name = sink_desc.get_name().to_owned();
        let db_name = sink_desc.get_db_name().into();
        let sink_from_name = sink_desc.get_sink_from_name().into();
        let properties = sink_desc.get_properties().clone();
        let downstream_pk = sink_desc
            .downstream_pk
            .iter()
            .map(|i| *i as usize)
            .collect_vec();
        let columns = sink_desc
            .column_catalogs
            .clone()
            .into_iter()
            .map(ColumnCatalog::from)
            .collect_vec();

        let connector = {
            let sink_type = properties.get(CONNECTOR_TYPE_KEY).ok_or_else(|| {
                SinkError::Config(anyhow!("missing config: {}", CONNECTOR_TYPE_KEY))
            })?;

            match_sink_name_str!(
                sink_type.to_lowercase().as_str(),
                SinkType,
                Ok(SinkType::SINK_NAME),
                |other| {
                    Err(SinkError::Config(anyhow!(
                        "unsupported sink connector {}",
                        other
                    )))
                }
            )
        }?;
        let format_desc = match &sink_desc.format_desc {
            // Case A: new syntax `format ... encode ...`
            Some(f) => Some(f.clone().try_into()?),
            None => match sink_desc.properties.get(SINK_TYPE_OPTION) {
                // Case B: old syntax `type = '...'`
                Some(t) => SinkFormatDesc::from_legacy_type(connector, t)?,
                // Case C: no format + encode required
                None => None,
            },
        };

        let sink_param = SinkParam {
            sink_id,
            sink_name,
            properties,
            columns: columns
                .iter()
                .filter(|col| !col.is_hidden)
                .map(|col| col.column_desc.clone())
                .collect(),
            downstream_pk,
            sink_type,
            format_desc,
            db_name,
            sink_from_name,
        };

        let sink_id_str = format!("{}", sink_id.sink_id);

        let sink_metrics = params.executor_stats.new_sink_metrics(
            &params.info.identity,
            sink_id_str.as_str(),
            connector,
        );

        let sink_write_param = SinkWriterParam {
            executor_id: params.executor_id,
            vnode_bitmap: params.vnode_bitmap.clone(),
            meta_client: params.env.meta_client().map(SinkMetaClient::MetaClient),
            sink_metrics,
            extra_partition_col_idx: sink_desc.extra_partition_col_idx.map(|v| v as usize),
        };

        let log_store_identity = format!(
            "sink[{}]-[{}]-executor[{}]",
            connector, sink_id.sink_id, params.executor_id
        );

        let exec = match node.log_store_type() {
            // Default value is the normal in memory log store to be backward compatible with the
            // previously unset value
            SinkLogStoreType::InMemoryLogStore | SinkLogStoreType::Unspecified => {
                let factory = BoundedInMemLogStoreFactory::new(1);
                SinkExecutor::new(
                    params.actor_context,
                    params.info.clone(),
                    input_executor,
                    sink_write_param,
                    sink_param,
                    columns,
                    factory,
                    chunk_size,
                    input_data_types,
                )
                .await?
                .boxed()
            }
            SinkLogStoreType::KvLogStore => {
                let metrics = KvLogStoreMetrics::new(
                    &params.executor_stats,
                    &params.info.identity,
                    &sink_param,
                    connector,
                );

                let table = node.table.as_ref().unwrap().clone();
                let input_schema = input_executor.schema();
                let pk_info = resolve_pk_info(input_schema, &table)?;

                // TODO: support setting max row count in config
                let factory = KvLogStoreFactory::new(
                    state_store,
                    table,
                    params.vnode_bitmap.clone().map(Arc::new),
                    65536,
                    metrics,
                    log_store_identity,
                    pk_info,
                );

                SinkExecutor::new(
                    params.actor_context,
                    params.info.clone(),
                    input_executor,
                    sink_write_param,
                    sink_param,
                    columns,
                    factory,
                    chunk_size,
                    input_data_types,
                )
                .await?
                .boxed()
            }
        };

        Ok((params.info, exec).into())
    }
}
