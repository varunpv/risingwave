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

use std::ops::{Deref, DerefMut};

use risingwave_common::array::Op;
use risingwave_common::buffer::Bitmap;
use risingwave_common::hash::HashKey;
use risingwave_common::row::RowExt;
use risingwave_common::util::epoch::EpochPair;
use risingwave_common::util::iter_util::ZipEqDebug;
use risingwave_common::util::sort_util::ColumnOrder;

use super::top_n_cache::TopNCacheTrait;
use super::utils::*;
use super::{ManagedTopNState, TopNCache};
use crate::cache::{new_unbounded, ManagedLruCache};
use crate::common::metrics::MetricsInfo;
use crate::executor::prelude::*;

pub type GroupTopNExecutor<K, S, const WITH_TIES: bool> =
    TopNExecutorWrapper<InnerGroupTopNExecutor<K, S, WITH_TIES>>;

impl<K: HashKey, S: StateStore, const WITH_TIES: bool> GroupTopNExecutor<K, S, WITH_TIES> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        input: Executor,
        ctx: ActorContextRef,
        schema: Schema,
        storage_key: Vec<ColumnOrder>,
        offset_and_limit: (usize, usize),
        order_by: Vec<ColumnOrder>,
        group_by: Vec<usize>,
        state_table: StateTable<S>,
        watermark_epoch: AtomicU64Ref,
    ) -> StreamResult<Self> {
        Ok(TopNExecutorWrapper {
            input,
            ctx: ctx.clone(),
            inner: InnerGroupTopNExecutor::new(
                schema,
                storage_key,
                offset_and_limit,
                order_by,
                group_by,
                state_table,
                watermark_epoch,
                ctx,
            )?,
        })
    }
}

pub struct InnerGroupTopNExecutor<K: HashKey, S: StateStore, const WITH_TIES: bool> {
    schema: Schema,

    /// `LIMIT XXX`. None means no limit.
    limit: usize,

    /// `OFFSET XXX`. `0` means no offset.
    offset: usize,

    /// The storage key indices of the `GroupTopNExecutor`
    storage_key_indices: PkIndices,

    managed_state: ManagedTopNState<S>,

    /// which column we used to group the data.
    group_by: Vec<usize>,

    /// group key -> cache for this group
    caches: GroupTopNCache<K, WITH_TIES>,

    /// Used for serializing pk into `CacheKey`.
    cache_key_serde: CacheKeySerde,

    ctx: ActorContextRef,
}

impl<K: HashKey, S: StateStore, const WITH_TIES: bool> InnerGroupTopNExecutor<K, S, WITH_TIES> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        schema: Schema,
        storage_key: Vec<ColumnOrder>,
        offset_and_limit: (usize, usize),
        order_by: Vec<ColumnOrder>,
        group_by: Vec<usize>,
        state_table: StateTable<S>,
        watermark_epoch: AtomicU64Ref,
        ctx: ActorContextRef,
    ) -> StreamResult<Self> {
        let metrics_info = MetricsInfo::new(
            ctx.streaming_metrics.clone(),
            state_table.table_id(),
            ctx.id,
            "GroupTopN",
        );

        let cache_key_serde = create_cache_key_serde(&storage_key, &schema, &order_by, &group_by);
        let managed_state = ManagedTopNState::<S>::new(state_table, cache_key_serde.clone());

        Ok(Self {
            schema,
            offset: offset_and_limit.0,
            limit: offset_and_limit.1,
            managed_state,
            storage_key_indices: storage_key.into_iter().map(|op| op.column_index).collect(),
            group_by,
            caches: GroupTopNCache::new(watermark_epoch, metrics_info),
            cache_key_serde,
            ctx,
        })
    }
}

pub struct GroupTopNCache<K: HashKey, const WITH_TIES: bool> {
    data: ManagedLruCache<K, TopNCache<WITH_TIES>>,
}

impl<K: HashKey, const WITH_TIES: bool> GroupTopNCache<K, WITH_TIES> {
    pub fn new(watermark_epoch: AtomicU64Ref, metrics_info: MetricsInfo) -> Self {
        let cache = new_unbounded(watermark_epoch, metrics_info);
        Self { data: cache }
    }
}

impl<K: HashKey, const WITH_TIES: bool> Deref for GroupTopNCache<K, WITH_TIES> {
    type Target = ManagedLruCache<K, TopNCache<WITH_TIES>>;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<K: HashKey, const WITH_TIES: bool> DerefMut for GroupTopNCache<K, WITH_TIES> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<K: HashKey, S: StateStore, const WITH_TIES: bool> TopNExecutorBase
    for InnerGroupTopNExecutor<K, S, WITH_TIES>
where
    TopNCache<WITH_TIES>: TopNCacheTrait,
{
    async fn apply_chunk(&mut self, chunk: StreamChunk) -> StreamExecutorResult<StreamChunk> {
        let mut res_ops = Vec::with_capacity(self.limit);
        let mut res_rows = Vec::with_capacity(self.limit);
        let keys = K::build_many(&self.group_by, chunk.data_chunk());
        let table_id_str = self.managed_state.table().table_id().to_string();
        let actor_id_str = self.ctx.id.to_string();
        let fragment_id_str = self.ctx.fragment_id.to_string();
        for (r, group_cache_key) in chunk.rows_with_holes().zip_eq_debug(keys.iter()) {
            let Some((op, row_ref)) = r else {
                continue;
            };
            // The pk without group by
            let pk_row = row_ref.project(&self.storage_key_indices[self.group_by.len()..]);
            let cache_key = serialize_pk_to_cache_key(pk_row, &self.cache_key_serde);

            let group_key = row_ref.project(&self.group_by);
            self.ctx
                .streaming_metrics
                .group_top_n_total_query_cache_count
                .with_label_values(&[&table_id_str, &actor_id_str, &fragment_id_str])
                .inc();
            // If 'self.caches' does not already have a cache for the current group, create a new
            // cache for it and insert it into `self.caches`
            if !self.caches.contains(group_cache_key) {
                self.ctx
                    .streaming_metrics
                    .group_top_n_cache_miss_count
                    .with_label_values(&[&table_id_str, &actor_id_str, &fragment_id_str])
                    .inc();
                let mut topn_cache =
                    TopNCache::new(self.offset, self.limit, self.schema.data_types());
                self.managed_state
                    .init_topn_cache(Some(group_key), &mut topn_cache)
                    .await?;
                self.caches.push(group_cache_key.clone(), topn_cache);
            }

            let mut cache = self.caches.get_mut(group_cache_key).unwrap();

            // apply the chunk to state table
            match op {
                Op::Insert | Op::UpdateInsert => {
                    self.managed_state.insert(row_ref);
                    cache.insert(cache_key, row_ref, &mut res_ops, &mut res_rows);
                }

                Op::Delete | Op::UpdateDelete => {
                    self.managed_state.delete(row_ref);
                    cache
                        .delete(
                            Some(group_key),
                            &mut self.managed_state,
                            cache_key,
                            row_ref,
                            &mut res_ops,
                            &mut res_rows,
                        )
                        .await?;
                }
            }
        }
        self.ctx
            .streaming_metrics
            .group_top_n_cached_entry_count
            .with_label_values(&[&table_id_str, &actor_id_str, &fragment_id_str])
            .set(self.caches.len() as i64);
        generate_output(res_rows, res_ops, &self.schema)
    }

    async fn flush_data(&mut self, epoch: EpochPair) -> StreamExecutorResult<()> {
        self.managed_state.flush(epoch).await
    }

    async fn try_flush_data(&mut self) -> StreamExecutorResult<()> {
        self.managed_state.try_flush().await
    }

    fn update_epoch(&mut self, epoch: u64) {
        self.caches.update_epoch(epoch);
    }

    fn update_vnode_bitmap(&mut self, vnode_bitmap: Arc<Bitmap>) {
        let cache_may_stale = self.managed_state.update_vnode_bitmap(vnode_bitmap);
        if cache_may_stale {
            self.caches.clear();
        }
    }

    fn evict(&mut self) {
        self.caches.evict()
    }

    async fn init(&mut self, epoch: EpochPair) -> StreamExecutorResult<()> {
        self.managed_state.init_epoch(epoch);
        Ok(())
    }

    async fn handle_watermark(&mut self, watermark: Watermark) -> Option<Watermark> {
        if watermark.col_idx == self.group_by[0] {
            self.managed_state
                .update_watermark(watermark.val.clone(), false);
            Some(watermark)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use assert_matches::assert_matches;
    use risingwave_common::array::stream_chunk::StreamChunkTestExt;
    use risingwave_common::catalog::Field;
    use risingwave_common::hash::SerializedKey;
    use risingwave_common::util::epoch::test_epoch;
    use risingwave_common::util::sort_util::OrderType;
    use risingwave_storage::memory::MemoryStateStore;

    use super::*;
    use crate::executor::test_utils::top_n_executor::create_in_memory_state_table;
    use crate::executor::test_utils::MockSource;

    fn create_schema() -> Schema {
        Schema {
            fields: vec![
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
            ],
        }
    }

    fn storage_key() -> Vec<ColumnOrder> {
        vec![
            ColumnOrder::new(1, OrderType::ascending()),
            ColumnOrder::new(2, OrderType::ascending()),
            ColumnOrder::new(0, OrderType::ascending()),
        ]
    }

    /// group by 1, order by 2
    fn order_by_1() -> Vec<ColumnOrder> {
        vec![ColumnOrder::new(2, OrderType::ascending())]
    }

    /// group by 1,2, order by 0
    fn order_by_2() -> Vec<ColumnOrder> {
        vec![ColumnOrder::new(0, OrderType::ascending())]
    }

    fn pk_indices() -> PkIndices {
        vec![1, 2, 0]
    }

    fn create_stream_chunks() -> Vec<StreamChunk> {
        let chunk0 = StreamChunk::from_pretty(
            "  I I I
            + 10 9 1
            +  8 8 2
            +  7 8 2
            +  9 1 1
            + 10 1 1
            +  8 1 3",
        );
        let chunk1 = StreamChunk::from_pretty(
            "  I I I
            - 10 9 1
            -  8 8 2
            - 10 1 1",
        );
        let chunk2 = StreamChunk::from_pretty(
            " I I I
            - 7 8 2
            - 8 1 3
            - 9 1 1",
        );
        let chunk3 = StreamChunk::from_pretty(
            "  I I I
            +  5 1 1
            +  2 1 1
            +  3 1 2
            +  4 1 3",
        );
        vec![chunk0, chunk1, chunk2, chunk3]
    }

    fn create_source() -> Executor {
        let mut chunks = create_stream_chunks();
        let schema = create_schema();
        MockSource::with_messages(vec![
            Message::Barrier(Barrier::new_test_barrier(test_epoch(1))),
            Message::Chunk(std::mem::take(&mut chunks[0])),
            Message::Barrier(Barrier::new_test_barrier(test_epoch(2))),
            Message::Chunk(std::mem::take(&mut chunks[1])),
            Message::Barrier(Barrier::new_test_barrier(test_epoch(3))),
            Message::Chunk(std::mem::take(&mut chunks[2])),
            Message::Barrier(Barrier::new_test_barrier(test_epoch(4))),
            Message::Chunk(std::mem::take(&mut chunks[3])),
            Message::Barrier(Barrier::new_test_barrier(test_epoch(5))),
        ])
        .into_executor(schema, pk_indices())
    }

    #[tokio::test]
    async fn test_without_offset_and_with_limits() {
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::ascending(),
                OrderType::ascending(),
                OrderType::ascending(),
            ],
            &pk_indices(),
        )
        .await;
        let schema = source.schema().clone();
        let top_n_executor = GroupTopNExecutor::<SerializedKey, MemoryStateStore, false>::new(
            source,
            ActorContext::for_test(0),
            schema,
            storage_key(),
            (0, 2),
            order_by_1(),
            vec![1],
            state_table,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let mut top_n_executor = top_n_executor.boxed().execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                + 10 9 1
                +  8 8 2
                +  7 8 2
                +  9 1 1
                + 10 1 1
                ",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 9 1
                -  8 8 2
                - 10 1 1
                +  8 1 3
                ",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                - 7 8 2
                - 8 1 3
                - 9 1 1
                ",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                + 5 1 1
                + 2 1 1
                ",
            ),
        );
    }

    #[tokio::test]
    async fn test_with_offset_and_with_limits() {
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::ascending(),
                OrderType::ascending(),
                OrderType::ascending(),
            ],
            &pk_indices(),
        )
        .await;
        let schema = source.schema().clone();
        let top_n_executor = GroupTopNExecutor::<SerializedKey, MemoryStateStore, false>::new(
            source,
            ActorContext::for_test(0),
            schema,
            storage_key(),
            (1, 2),
            order_by_1(),
            vec![1],
            state_table,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let mut top_n_executor = top_n_executor.boxed().execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                +  8 8 2
                + 10 1 1
                +  8 1 3
                ",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                -  8 8 2
                - 10 1 1
                ",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                - 8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                " I I I
                + 5 1 1
                + 3 1 2
                ",
            ),
        );
    }

    #[tokio::test]
    async fn test_multi_group_key() {
        let source = create_source();
        let state_table = create_in_memory_state_table(
            &[DataType::Int64, DataType::Int64, DataType::Int64],
            &[
                OrderType::ascending(),
                OrderType::ascending(),
                OrderType::ascending(),
            ],
            &pk_indices(),
        )
        .await;
        let schema = source.schema().clone();
        let top_n_executor = GroupTopNExecutor::<SerializedKey, MemoryStateStore, false>::new(
            source,
            ActorContext::for_test(0),
            schema,
            storage_key(),
            (0, 2),
            order_by_2(),
            vec![1, 2],
            state_table,
            Arc::new(AtomicU64::new(0)),
        )
        .unwrap();
        let mut top_n_executor = top_n_executor.boxed().execute();

        // consume the init barrier
        top_n_executor.next().await.unwrap().unwrap();
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                + 10 9 1
                +  8 8 2
                +  7 8 2
                +  9 1 1
                + 10 1 1
                +  8 1 3",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 10 9 1
                -  8 8 2
                - 10 1 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                - 7 8 2
                - 8 1 3
                - 9 1 1",
            ),
        );

        // barrier
        assert_matches!(
            top_n_executor.next().await.unwrap().unwrap(),
            Message::Barrier(_)
        );
        let res = top_n_executor.next().await.unwrap().unwrap();
        assert_eq!(
            res.as_chunk().unwrap(),
            &StreamChunk::from_pretty(
                "  I I I
                +  5 1 1
                +  2 1 1
                +  3 1 2
                +  4 1 3",
            ),
        );
    }
}
