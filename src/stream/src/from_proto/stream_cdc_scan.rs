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

use std::collections::HashMap;
use std::sync::Arc;

use risingwave_common::catalog::{Schema, TableId};
use risingwave_common::util::sort_util::OrderType;
use risingwave_connector::source::cdc::external::{CdcTableType, SchemaTableName};
use risingwave_pb::plan_common::ExternalTableDesc;
use risingwave_pb::stream_plan::StreamCdcScanNode;

use super::*;
use crate::common::table::state_table::StateTable;
use crate::executor::{CdcBackfillExecutor, CdcScanOptions, ExternalStorageTable};

pub struct StreamCdcScanExecutorBuilder;

impl ExecutorBuilder for StreamCdcScanExecutorBuilder {
    type Node = StreamCdcScanNode;

    async fn new_boxed_executor(
        params: ExecutorParams,
        node: &Self::Node,
        state_store: impl StateStore,
    ) -> StreamResult<Executor> {
        let [upstream]: [_; 1] = params.input.try_into().unwrap();

        let output_indices = node
            .output_indices
            .iter()
            .map(|&i| i as usize)
            .collect_vec();

        let table_desc: &ExternalTableDesc = node.get_cdc_table_desc()?;

        let table_schema: Schema = table_desc.columns.iter().map(Into::into).collect();
        assert_eq!(output_indices, (0..table_schema.len()).collect_vec());
        assert_eq!(table_schema.data_types(), params.info.schema.data_types());

        let properties: HashMap<String, String> = table_desc
            .connect_properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let table_pk_order_types = table_desc
            .pk
            .iter()
            .map(|desc| OrderType::from_protobuf(desc.get_order_type().unwrap()))
            .collect_vec();
        let table_pk_indices = table_desc
            .pk
            .iter()
            .map(|k| k.column_index as usize)
            .collect_vec();

        let scan_options = node
            .options
            .as_ref()
            .map(CdcScanOptions::from_proto)
            .unwrap_or(CdcScanOptions {
                disable_backfill: node.disable_backfill,
                ..Default::default()
            });
        let table_type = CdcTableType::from_properties(&properties);
        let table_reader = table_type
            .create_table_reader(
                properties.clone(),
                table_schema.clone(),
                table_pk_indices.clone(),
                scan_options.snapshot_batch_size,
            )
            .await?;

        let schema_table_name = SchemaTableName::from_properties(&properties);
        let external_table = ExternalStorageTable::new(
            TableId::new(table_desc.table_id),
            schema_table_name,
            table_reader,
            table_schema,
            table_pk_order_types,
            table_pk_indices,
            output_indices.clone(),
        );

        let vnodes = params.vnode_bitmap.map(Arc::new);
        // cdc backfill should be singleton, so vnodes must be None.
        assert_eq!(None, vnodes);
        let state_table =
            StateTable::from_table_catalog(node.get_state_table()?, state_store, vnodes).await;

        let exec = CdcBackfillExecutor::new(
            params.actor_context.clone(),
            external_table,
            upstream,
            output_indices,
            None,
            params.executor_stats,
            state_table,
            node.rate_limit,
            scan_options,
        );
        Ok((params.info, exec).into())
    }
}
