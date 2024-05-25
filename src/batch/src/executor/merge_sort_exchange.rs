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

use futures_async_stream::try_stream;
use risingwave_common::array::DataChunk;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::memory::{MemMonitoredHeap, MemoryContext, MonitoredGlobalAlloc};
use risingwave_common::types::ToOwnedDatum;
use risingwave_common::util::sort_util::{ColumnOrder, HeapElem};
use risingwave_common_estimate_size::EstimateSize;
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::PbExchangeSource;

use crate::error::{BatchError, Result};
use crate::exchange_source::ExchangeSourceImpl;
use crate::executor::{
    BoxedDataChunkStream, BoxedExecutor, BoxedExecutorBuilder, CreateSource, DefaultCreateSource,
    Executor, ExecutorBuilder,
};
use crate::task::{BatchTaskContext, TaskId};

pub type MergeSortExchangeExecutor<C> = MergeSortExchangeExecutorImpl<DefaultCreateSource, C>;

/// `MergeSortExchangeExecutor2` takes inputs from multiple sources and
/// The outputs of all the sources have been sorted in the same way.
pub struct MergeSortExchangeExecutorImpl<CS, C> {
    context: C,
    /// keeps one data chunk of each source if any
    source_inputs: Vec<Option<DataChunk>, MonitoredGlobalAlloc>,
    column_orders: Arc<Vec<ColumnOrder>>,
    min_heap: MemMonitoredHeap<HeapElem>,
    proto_sources: Vec<PbExchangeSource>,
    sources: Vec<ExchangeSourceImpl>, // impl
    /// Mock-able `CreateSource`.
    source_creators: Vec<CS>,
    schema: Schema,
    task_id: TaskId,
    identity: String,
    /// The maximum size of the chunk produced by executor at a time.
    chunk_size: usize,
    mem_ctx: MemoryContext,
    alloc: MonitoredGlobalAlloc,
}

impl<CS: 'static + Send + CreateSource, C: BatchTaskContext> MergeSortExchangeExecutorImpl<CS, C> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: C,
        column_orders: Arc<Vec<ColumnOrder>>,
        proto_sources: Vec<PbExchangeSource>,
        source_creators: Vec<CS>,
        schema: Schema,
        task_id: TaskId,
        identity: String,
        chunk_size: usize,
    ) -> Self {
        let mem_ctx = context.create_executor_mem_context(&identity);
        let alloc = MonitoredGlobalAlloc::with_memory_context(mem_ctx.clone());

        let source_inputs = {
            let mut v = Vec::with_capacity_in(proto_sources.len(), alloc.clone());
            (0..proto_sources.len()).for_each(|_| v.push(None));
            v
        };

        let num_sources = proto_sources.len();

        Self {
            context,
            source_inputs,
            column_orders,
            min_heap: MemMonitoredHeap::with_capacity(num_sources, mem_ctx.clone()),
            proto_sources,
            sources: Vec::with_capacity(num_sources),
            source_creators,
            schema,
            task_id,
            identity,
            chunk_size,
            mem_ctx,
            alloc,
        }
    }

    /// We assume that the source would always send `Some(chunk)` with cardinality > 0
    /// or `None`, but never `Some(chunk)` with cardinality == 0.
    async fn get_source_chunk(&mut self, source_idx: usize) -> Result<()> {
        assert!(source_idx < self.source_inputs.len());
        let res = self.sources[source_idx].take_data().await?;
        let old = match res {
            Some(chunk) => {
                assert_ne!(chunk.cardinality(), 0);
                let new_chunk_size = chunk.estimated_heap_size() as i64;
                let old = std::mem::replace(&mut self.source_inputs[source_idx], Some(chunk));
                self.mem_ctx.add(new_chunk_size);
                old
            }
            None => std::mem::take(&mut self.source_inputs[source_idx]),
        };

        if let Some(chunk) = old {
            // Reduce the heap size of retired chunk
            self.mem_ctx.add(-(chunk.estimated_heap_size() as i64));
        }

        Ok(())
    }

    // Check whether there is indeed a chunk and there is a visible row sitting at `row_idx`
    // in the chunk before calling this function.
    fn push_row_into_heap(&mut self, source_idx: usize, row_idx: usize) -> Result<()> {
        assert!(source_idx < self.source_inputs.len());
        let chunk_ref = self.source_inputs[source_idx].as_ref().unwrap();
        self.min_heap.push(HeapElem::new(
            self.column_orders.clone(),
            chunk_ref.clone(),
            source_idx,
            row_idx,
            None,
        ));

        if self.min_heap.mem_context().check_memory_usage() {
            Ok(())
        } else {
            Err(BatchError::OutOfMemory(
                self.min_heap.mem_context().mem_limit(),
            ))
        }
    }
}

impl<CS: 'static + Send + CreateSource, C: BatchTaskContext> Executor
    for MergeSortExchangeExecutorImpl<CS, C>
{
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute()
    }
}
/// Everytime `execute` is called, it tries to produce a chunk of size
/// `self.chunk_size`. It is possible that the chunk's size is smaller than the
/// `self.chunk_size` as the executor runs out of input from `sources`.
impl<CS: 'static + Send + CreateSource, C: BatchTaskContext> MergeSortExchangeExecutorImpl<CS, C> {
    #[try_stream(boxed, ok = DataChunk, error = BatchError)]
    async fn do_execute(mut self: Box<Self>) {
        for source_idx in 0..self.proto_sources.len() {
            let new_source = self.source_creators[source_idx]
                .create_source(self.context.clone(), &self.proto_sources[source_idx])
                .await?;
            self.sources.push(new_source);
            self.get_source_chunk(source_idx).await?;
            if let Some(chunk) = &self.source_inputs[source_idx] {
                // We assume that we would always get a non-empty chunk from the upstream of
                // exchange, therefore we are sure that there is at least
                // one visible row.
                let next_row_idx = chunk.next_visible_row_idx(0);
                self.push_row_into_heap(source_idx, next_row_idx.unwrap())?;
            }
        }

        // If there is no rows in the heap,
        // we run out of input data chunks and emit `Done`.
        while !self.min_heap.is_empty() {
            // It is possible that we cannot produce this much as
            // we may run out of input data chunks from sources.
            let mut want_to_produce = self.chunk_size;

            let mut builders: Vec<_> = self
                .schema()
                .fields
                .iter()
                .map(|field| field.data_type.create_array_builder(self.chunk_size))
                .collect();
            let mut array_len = 0;
            while want_to_produce > 0 && !self.min_heap.is_empty() {
                let top_elem = self.min_heap.pop().unwrap();
                let child_idx = top_elem.chunk_idx();
                let cur_chunk = top_elem.chunk();
                let row_idx = top_elem.elem_idx();
                for (idx, builder) in builders.iter_mut().enumerate() {
                    let chunk_arr = cur_chunk.column_at(idx);
                    let chunk_arr = chunk_arr.as_ref();
                    let datum = chunk_arr.value_at(row_idx).to_owned_datum();
                    builder.append(&datum);
                }
                want_to_produce -= 1;
                array_len += 1;
                // check whether we have another row from the same chunk being popped
                let possible_next_row_idx = cur_chunk.next_visible_row_idx(row_idx + 1);
                match possible_next_row_idx {
                    Some(next_row_idx) => {
                        self.push_row_into_heap(child_idx, next_row_idx)?;
                    }
                    None => {
                        self.get_source_chunk(child_idx).await?;
                        if let Some(chunk) = &self.source_inputs[child_idx] {
                            let next_row_idx = chunk.next_visible_row_idx(0);
                            self.push_row_into_heap(child_idx, next_row_idx.unwrap())?;
                        }
                    }
                }
            }

            let columns = builders
                .into_iter()
                .map(|builder| builder.finish().into())
                .collect::<Vec<_>>();
            let chunk = DataChunk::new(columns, array_len);
            yield chunk
        }
    }
}

pub struct MergeSortExchangeExecutorBuilder {}

#[async_trait::async_trait]
impl BoxedExecutorBuilder for MergeSortExchangeExecutorBuilder {
    async fn new_boxed_executor<C: BatchTaskContext>(
        source: &ExecutorBuilder<'_, C>,
        inputs: Vec<BoxedExecutor>,
    ) -> Result<BoxedExecutor> {
        ensure!(
            inputs.is_empty(),
            "MergeSortExchangeExecutor should not have child!"
        );
        let sort_merge_node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::MergeSortExchange
        )?;

        let column_orders = sort_merge_node
            .column_orders
            .iter()
            .map(ColumnOrder::from_protobuf)
            .collect();
        let column_orders = Arc::new(column_orders);

        let exchange_node = sort_merge_node.get_exchange()?;
        let proto_sources: Vec<PbExchangeSource> = exchange_node.get_sources().to_vec();
        let source_creators =
            vec![DefaultCreateSource::new(source.context().client_pool()); proto_sources.len()];
        ensure!(!exchange_node.get_sources().is_empty());
        let fields = exchange_node
            .get_input_schema()
            .iter()
            .map(Field::from)
            .collect::<Vec<Field>>();

        Ok(Box::new(MergeSortExchangeExecutor::<C>::new(
            source.context().clone(),
            column_orders,
            proto_sources,
            source_creators,
            Schema { fields },
            source.task_id.clone(),
            source.plan_node().get_identity().clone(),
            source.context.get_config().developer.chunk_size,
        )))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use risingwave_common::array::Array;
    use risingwave_common::test_prelude::DataChunkTestExt;
    use risingwave_common::types::DataType;
    use risingwave_common::util::sort_util::OrderType;

    use super::*;
    use crate::executor::test_utils::{FakeCreateSource, FakeExchangeSource};
    use crate::task::ComputeNodeContext;

    const CHUNK_SIZE: usize = 1024;

    #[tokio::test]
    async fn test_exchange_multiple_sources() {
        let chunk = DataChunk::from_pretty(
            "i
                     1
                     2
                     3",
        );
        let fake_exchange_source = FakeExchangeSource::new(vec![Some(chunk)]);
        let fake_create_source = FakeCreateSource::new(fake_exchange_source);

        let mut proto_sources: Vec<PbExchangeSource> = vec![];
        let mut source_creators = vec![];
        let num_sources = 2;
        for _ in 0..num_sources {
            proto_sources.push(PbExchangeSource::default());
            source_creators.push(fake_create_source.clone());
        }
        let column_orders = Arc::new(vec![ColumnOrder {
            column_index: 0,
            order_type: OrderType::ascending(),
        }]);

        let executor = Box::new(MergeSortExchangeExecutorImpl::<
            FakeCreateSource,
            ComputeNodeContext,
        >::new(
            ComputeNodeContext::for_test(),
            column_orders,
            proto_sources,
            source_creators,
            Schema {
                fields: vec![Field::unnamed(DataType::Int32)],
            },
            TaskId::default(),
            "MergeSortExchangeExecutor2".to_string(),
            CHUNK_SIZE,
        ));

        let mut stream = executor.execute();
        let res = stream.next().await;
        assert!(res.is_some());
        if let Some(res) = res {
            let res = res.unwrap();
            assert_eq!(res.capacity(), 3 * num_sources);
            let col0 = res.column_at(0);
            assert_eq!(col0.as_int32().value_at(0), Some(1));
            assert_eq!(col0.as_int32().value_at(1), Some(1));
            assert_eq!(col0.as_int32().value_at(2), Some(2));
            assert_eq!(col0.as_int32().value_at(3), Some(2));
            assert_eq!(col0.as_int32().value_at(4), Some(3));
            assert_eq!(col0.as_int32().value_at(5), Some(3));
        }
        let res = stream.next().await;
        assert!(res.is_none());
    }
}
