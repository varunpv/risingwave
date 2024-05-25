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

use std::sync::OnceLock;

use prometheus::core::{AtomicF64, AtomicI64, AtomicU64, GenericCounterVec, GenericGaugeVec};
use prometheus::{
    exponential_buckets, histogram_opts, register_gauge_vec_with_registry,
    register_histogram_with_registry, register_int_counter_vec_with_registry,
    register_int_counter_with_registry, register_int_gauge_vec_with_registry,
    register_int_gauge_with_registry, Histogram, IntCounter, IntCounterVec, IntGauge, Registry,
};
use risingwave_common::config::MetricLevel;
use risingwave_common::metrics::{
    LabelGuardedHistogramVec, LabelGuardedIntCounterVec, LabelGuardedIntGaugeVec,
    RelabeledGuardedHistogramVec,
};
use risingwave_common::monitor::GLOBAL_METRICS_REGISTRY;
use risingwave_common::{
    register_guarded_histogram_vec_with_registry, register_guarded_int_counter_vec_with_registry,
    register_guarded_int_gauge_vec_with_registry,
};
use risingwave_connector::sink::SinkMetrics;

use crate::common::log_store_impl::kv_log_store::{
    REWIND_BACKOFF_FACTOR, REWIND_BASE_DELAY, REWIND_MAX_DELAY,
};

#[derive(Clone)]
pub struct StreamingMetrics {
    pub level: MetricLevel,

    // Executor metrics (disabled by default)
    pub executor_row_count: GenericCounterVec<AtomicU64>,

    // Streaming actor metrics from tokio (disabled by default)
    pub actor_execution_time: GenericGaugeVec<AtomicF64>,
    pub actor_scheduled_duration: GenericGaugeVec<AtomicF64>,
    pub actor_scheduled_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_fast_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_fast_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_slow_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_slow_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_poll_duration: GenericGaugeVec<AtomicF64>,
    pub actor_poll_cnt: GenericGaugeVec<AtomicI64>,
    pub actor_idle_duration: GenericGaugeVec<AtomicF64>,
    pub actor_idle_cnt: GenericGaugeVec<AtomicI64>,

    // Streaming actor
    pub actor_memory_usage: GenericGaugeVec<AtomicI64>,
    pub actor_in_record_cnt: LabelGuardedIntCounterVec<3>,
    pub actor_out_record_cnt: LabelGuardedIntCounterVec<2>,

    // Source
    pub source_output_row_count: GenericCounterVec<AtomicU64>,
    pub source_split_change_count: GenericCounterVec<AtomicU64>,
    pub source_backfill_row_count: LabelGuardedIntCounterVec<4>,

    // Sink & materialized view
    pub sink_input_row_count: LabelGuardedIntCounterVec<3>,
    pub mview_input_row_count: IntCounterVec,
    pub sink_chunk_buffer_size: LabelGuardedIntGaugeVec<3>,

    // Exchange (see also `compute::ExchangeServiceMetrics`)
    pub exchange_frag_recv_size: GenericCounterVec<AtomicU64>,

    // Streaming Merge (We break out this metric from `barrier_align_duration` because
    // the alignment happens on different levels)
    pub merge_barrier_align_duration: RelabeledGuardedHistogramVec<2>,

    // Backpressure
    pub actor_output_buffer_blocking_duration_ns: LabelGuardedIntCounterVec<3>,
    pub actor_input_buffer_blocking_duration_ns: LabelGuardedIntCounterVec<3>,

    // Streaming Join
    pub join_lookup_miss_count: LabelGuardedIntCounterVec<5>,
    pub join_lookup_total_count: LabelGuardedIntCounterVec<5>,
    pub join_insert_cache_miss_count: LabelGuardedIntCounterVec<5>,
    pub join_actor_input_waiting_duration_ns: LabelGuardedIntCounterVec<2>,
    pub join_match_duration_ns: LabelGuardedIntCounterVec<3>,
    pub join_cached_entry_count: LabelGuardedIntGaugeVec<3>,
    pub join_matched_join_keys: RelabeledGuardedHistogramVec<3>,

    // Streaming Join, Streaming Dynamic Filter and Streaming Union
    pub barrier_align_duration: RelabeledGuardedHistogramVec<4>,

    // Streaming Aggregation
    pub agg_lookup_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_total_lookup_count: GenericCounterVec<AtomicU64>,
    pub agg_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub agg_chunk_lookup_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_chunk_total_lookup_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_total_cache_count: GenericCounterVec<AtomicU64>,
    pub agg_distinct_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub agg_dirty_groups_count: GenericGaugeVec<AtomicI64>,
    pub agg_dirty_groups_heap_size: GenericGaugeVec<AtomicI64>,

    // Streaming TopN
    pub group_top_n_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub group_top_n_appendonly_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_appendonly_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub group_top_n_appendonly_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // Lookup executor
    pub lookup_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub lookup_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub lookup_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // temporal join
    pub temporal_join_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub temporal_join_total_query_cache_count: GenericCounterVec<AtomicU64>,
    pub temporal_join_cached_entry_count: GenericGaugeVec<AtomicI64>,

    // Backfill
    pub backfill_snapshot_read_row_count: LabelGuardedIntCounterVec<2>,
    pub backfill_upstream_output_row_count: LabelGuardedIntCounterVec<2>,

    // CDC Backfill
    pub cdc_backfill_snapshot_read_row_count: GenericCounterVec<AtomicU64>,
    pub cdc_backfill_upstream_output_row_count: GenericCounterVec<AtomicU64>,

    // Over Window
    pub over_window_cached_entry_count: GenericGaugeVec<AtomicI64>,
    pub over_window_cache_lookup_count: GenericCounterVec<AtomicU64>,
    pub over_window_cache_miss_count: GenericCounterVec<AtomicU64>,
    pub over_window_range_cache_entry_count: GenericGaugeVec<AtomicI64>,
    pub over_window_range_cache_lookup_count: GenericCounterVec<AtomicU64>,
    pub over_window_range_cache_left_miss_count: GenericCounterVec<AtomicU64>,
    pub over_window_range_cache_right_miss_count: GenericCounterVec<AtomicU64>,

    /// The duration from receipt of barrier to all actors collection.
    /// And the max of all node `barrier_inflight_latency` is the latency for a barrier
    /// to flow through the graph.
    pub barrier_inflight_latency: Histogram,
    /// The duration of sync to storage.
    pub barrier_sync_latency: Histogram,
    /// The progress made by the earliest in-flight barriers in the local barrier manager.
    pub barrier_manager_progress: IntCounter,

    // Sink related metrics
    pub sink_commit_duration: LabelGuardedHistogramVec<3>,
    pub connector_sink_rows_received: LabelGuardedIntCounterVec<2>,
    pub log_store_first_write_epoch: LabelGuardedIntGaugeVec<3>,
    pub log_store_latest_write_epoch: LabelGuardedIntGaugeVec<3>,
    pub log_store_write_rows: LabelGuardedIntCounterVec<3>,
    pub log_store_latest_read_epoch: LabelGuardedIntGaugeVec<3>,
    pub log_store_read_rows: LabelGuardedIntCounterVec<3>,
    pub log_store_reader_wait_new_future_duration_ns: LabelGuardedIntCounterVec<3>,
    pub kv_log_store_storage_write_count: LabelGuardedIntCounterVec<3>,
    pub kv_log_store_storage_write_size: LabelGuardedIntCounterVec<3>,
    pub kv_log_store_rewind_count: LabelGuardedIntCounterVec<3>,
    pub kv_log_store_rewind_delay: LabelGuardedHistogramVec<3>,
    pub kv_log_store_storage_read_count: LabelGuardedIntCounterVec<4>,
    pub kv_log_store_storage_read_size: LabelGuardedIntCounterVec<4>,
    pub kv_log_store_buffer_unconsumed_item_count: LabelGuardedIntGaugeVec<3>,
    pub kv_log_store_buffer_unconsumed_row_count: LabelGuardedIntGaugeVec<3>,
    pub kv_log_store_buffer_unconsumed_epoch_count: LabelGuardedIntGaugeVec<3>,
    pub kv_log_store_buffer_unconsumed_min_epoch: LabelGuardedIntGaugeVec<3>,

    // Sink iceberg metrics
    pub iceberg_write_qps: LabelGuardedIntCounterVec<2>,
    pub iceberg_write_latency: LabelGuardedHistogramVec<2>,
    pub iceberg_rolling_unflushed_data_file: LabelGuardedIntGaugeVec<2>,
    pub iceberg_position_delete_cache_num: LabelGuardedIntGaugeVec<2>,
    pub iceberg_partition_num: LabelGuardedIntGaugeVec<2>,

    // Memory management
    // FIXME(yuhao): use u64 here
    pub lru_current_watermark_time_ms: IntGauge,
    pub lru_physical_now_ms: IntGauge,
    pub lru_runtime_loop_count: IntCounter,
    pub lru_watermark_step: IntGauge,
    pub lru_evicted_watermark_time_ms: LabelGuardedIntGaugeVec<3>,
    pub jemalloc_allocated_bytes: IntGauge,
    pub jemalloc_active_bytes: IntGauge,
    pub jemalloc_resident_bytes: IntGauge,
    pub jemalloc_metadata_bytes: IntGauge,
    pub jvm_allocated_bytes: IntGauge,
    pub jvm_active_bytes: IntGauge,

    // Materialize
    pub materialize_cache_hit_count: GenericCounterVec<AtomicU64>,
    pub materialize_cache_total_count: GenericCounterVec<AtomicU64>,

    // Memory
    pub stream_memory_usage: LabelGuardedIntGaugeVec<3>,
}

pub static GLOBAL_STREAMING_METRICS: OnceLock<StreamingMetrics> = OnceLock::new();

pub fn global_streaming_metrics(metric_level: MetricLevel) -> StreamingMetrics {
    GLOBAL_STREAMING_METRICS
        .get_or_init(|| StreamingMetrics::new(&GLOBAL_METRICS_REGISTRY, metric_level))
        .clone()
}

impl StreamingMetrics {
    fn new(registry: &Registry, level: MetricLevel) -> Self {
        let executor_row_count = register_int_counter_vec_with_registry!(
            "stream_executor_row_count",
            "Total number of rows that have been output from each executor",
            &["actor_id", "fragment_id", "executor_identity"],
            registry
        )
        .unwrap();

        let source_output_row_count = register_int_counter_vec_with_registry!(
            "stream_source_output_rows_counts",
            "Total number of rows that have been output from source",
            &["source_id", "source_name", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let source_split_change_count = register_int_counter_vec_with_registry!(
            "stream_source_split_change_event_count",
            "Total number of split change events that have been operated by source",
            &["source_id", "source_name", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let source_backfill_row_count = register_guarded_int_counter_vec_with_registry!(
            "stream_source_backfill_rows_counts",
            "Total number of rows that have been backfilled for source",
            &["source_id", "source_name", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let sink_input_row_count = register_guarded_int_counter_vec_with_registry!(
            "stream_sink_input_row_count",
            "Total number of rows streamed into sink executors",
            &["sink_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let mview_input_row_count = register_int_counter_vec_with_registry!(
            "stream_mview_input_row_count",
            "Total number of rows streamed into materialize executors",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let sink_chunk_buffer_size = register_guarded_int_gauge_vec_with_registry!(
            "stream_sink_chunk_buffer_size",
            "Total size of chunks buffered in a barrier",
            &["sink_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let actor_execution_time = register_gauge_vec_with_registry!(
            "stream_actor_actor_execution_time",
            "Total execution time (s) of an actor",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_output_buffer_blocking_duration_ns =
            register_guarded_int_counter_vec_with_registry!(
                "stream_actor_output_buffer_blocking_duration_ns",
                "Total blocking duration (ns) of output buffer",
                &["actor_id", "fragment_id", "downstream_fragment_id"],
                registry
            )
            .unwrap();

        let actor_input_buffer_blocking_duration_ns =
            register_guarded_int_counter_vec_with_registry!(
                "stream_actor_input_buffer_blocking_duration_ns",
                "Total blocking duration (ns) of input buffer",
                &["actor_id", "fragment_id", "upstream_fragment_id"],
                registry
            )
            .unwrap();

        let exchange_frag_recv_size = register_int_counter_vec_with_registry!(
            "stream_exchange_frag_recv_size",
            "Total size of messages that have been received from upstream Fragment",
            &["up_fragment_id", "down_fragment_id"],
            registry
        )
        .unwrap();

        let actor_fast_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_fast_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_fast_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_fast_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_slow_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_slow_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_slow_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_slow_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_poll_duration = register_gauge_vec_with_registry!(
            "stream_actor_poll_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_poll_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_poll_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_scheduled_duration = register_gauge_vec_with_registry!(
            "stream_actor_scheduled_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_scheduled_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_scheduled_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_idle_duration = register_gauge_vec_with_registry!(
            "stream_actor_idle_duration",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_idle_cnt = register_int_gauge_vec_with_registry!(
            "stream_actor_idle_cnt",
            "tokio's metrics",
            &["actor_id"],
            registry
        )
        .unwrap();

        let actor_in_record_cnt = register_guarded_int_counter_vec_with_registry!(
            "stream_actor_in_record_cnt",
            "Total number of rows actor received",
            &["actor_id", "fragment_id", "upstream_fragment_id"],
            registry
        )
        .unwrap();

        let actor_out_record_cnt = register_guarded_int_counter_vec_with_registry!(
            "stream_actor_out_record_cnt",
            "Total number of rows actor sent",
            &["actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let actor_memory_usage = register_int_gauge_vec_with_registry!(
            "actor_memory_usage",
            "Memory usage (bytes)",
            &["actor_id", "fragment_id"],
            registry,
        )
        .unwrap();

        let opts = histogram_opts!(
            "stream_merge_barrier_align_duration",
            "Duration of merge align barrier",
            exponential_buckets(0.0001, 2.0, 21).unwrap() // max 104s
        );
        let merge_barrier_align_duration = register_guarded_histogram_vec_with_registry!(
            opts,
            &["actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let merge_barrier_align_duration =
            RelabeledGuardedHistogramVec::with_metric_level_relabel_n(
                MetricLevel::Debug,
                merge_barrier_align_duration,
                level,
                1,
            );

        let join_lookup_miss_count = register_guarded_int_counter_vec_with_registry!(
            "stream_join_lookup_miss_count",
            "Join executor lookup miss duration",
            &[
                "side",
                "join_table_id",
                "degree_table_id",
                "actor_id",
                "fragment_id"
            ],
            registry
        )
        .unwrap();

        let join_lookup_total_count = register_guarded_int_counter_vec_with_registry!(
            "stream_join_lookup_total_count",
            "Join executor lookup total operation",
            &[
                "side",
                "join_table_id",
                "degree_table_id",
                "actor_id",
                "fragment_id"
            ],
            registry
        )
        .unwrap();

        let join_insert_cache_miss_count = register_guarded_int_counter_vec_with_registry!(
            "stream_join_insert_cache_miss_count",
            "Join executor cache miss when insert operation",
            &[
                "side",
                "join_table_id",
                "degree_table_id",
                "actor_id",
                "fragment_id"
            ],
            registry
        )
        .unwrap();

        let join_actor_input_waiting_duration_ns = register_guarded_int_counter_vec_with_registry!(
            "stream_join_actor_input_waiting_duration_ns",
            "Total waiting duration (ns) of input buffer of join actor",
            &["actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let join_match_duration_ns = register_guarded_int_counter_vec_with_registry!(
            "stream_join_match_duration_ns",
            "Matching duration for each side",
            &["actor_id", "fragment_id", "side"],
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "stream_barrier_align_duration",
            "Duration of join align barrier",
            exponential_buckets(0.0001, 2.0, 21).unwrap() // max 104s
        );
        let barrier_align_duration = register_guarded_histogram_vec_with_registry!(
            opts,
            &["actor_id", "fragment_id", "wait_side", "executor"],
            registry
        )
        .unwrap();

        let barrier_align_duration = RelabeledGuardedHistogramVec::with_metric_level_relabel_n(
            MetricLevel::Debug,
            barrier_align_duration,
            level,
            1,
        );

        let join_cached_entry_count = register_guarded_int_gauge_vec_with_registry!(
            "stream_join_cached_entry_count",
            "Number of cached entries in streaming join operators",
            &["actor_id", "fragment_id", "side"],
            registry
        )
        .unwrap();

        let join_matched_join_keys_opts = histogram_opts!(
            "stream_join_matched_join_keys",
            "The number of keys matched in the opposite side",
            exponential_buckets(16.0, 2.0, 28).unwrap() // max 2^31
        );

        let join_matched_join_keys = register_guarded_histogram_vec_with_registry!(
            join_matched_join_keys_opts,
            &["actor_id", "fragment_id", "table_id"],
            registry
        )
        .unwrap();

        let join_matched_join_keys = RelabeledGuardedHistogramVec::with_metric_level_relabel_n(
            MetricLevel::Debug,
            join_matched_join_keys,
            level,
            1,
        );

        let agg_lookup_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_lookup_miss_count",
            "Aggregation executor lookup miss duration",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_total_lookup_count = register_int_counter_vec_with_registry!(
            "stream_agg_lookup_total_count",
            "Aggregation executor lookup total operation",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_distinct_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_distinct_cache_miss_count",
            "Aggregation executor dinsinct miss duration",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_distinct_total_cache_count = register_int_counter_vec_with_registry!(
            "stream_agg_distinct_total_cache_count",
            "Aggregation executor distinct total operation",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_distinct_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_agg_distinct_cached_entry_count",
            "Total entry counts in distinct aggregation executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_dirty_groups_count = register_int_gauge_vec_with_registry!(
            "stream_agg_dirty_groups_count",
            "Total dirty group counts in aggregation executor",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_dirty_groups_heap_size = register_int_gauge_vec_with_registry!(
            "stream_agg_dirty_groups_heap_size",
            "Total dirty group heap size in aggregation executor",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let group_top_n_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_cache_miss_count",
            "Group top n executor cache miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let group_top_n_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_total_query_cache_count",
            "Group top n executor query cache total count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let group_top_n_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_group_top_n_cached_entry_count",
            "Total entry counts in group top n executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let group_top_n_appendonly_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_group_top_n_appendonly_cache_miss_count",
            "Group top n appendonly executor cache miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let group_top_n_appendonly_total_query_cache_count =
            register_int_counter_vec_with_registry!(
                "stream_group_top_n_appendonly_total_query_cache_count",
                "Group top n appendonly executor total cache count",
                &["table_id", "actor_id", "fragment_id"],
                registry
            )
            .unwrap();

        let group_top_n_appendonly_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_group_top_n_appendonly_cached_entry_count",
            "Total entry counts in group top n appendonly executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let lookup_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_lookup_cache_miss_count",
            "Lookup executor cache miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let lookup_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_lookup_total_query_cache_count",
            "Lookup executor query cache total count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let lookup_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_lookup_cached_entry_count",
            "Total entry counts in lookup executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let temporal_join_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_temporal_join_cache_miss_count",
            "Temporal join executor cache miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let temporal_join_total_query_cache_count = register_int_counter_vec_with_registry!(
            "stream_temporal_join_total_query_cache_count",
            "Temporal join executor query cache total count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let temporal_join_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_temporal_join_cached_entry_count",
            "Total entry count in temporal join executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_agg_cached_entry_count",
            "Number of cached keys in streaming aggregation operators",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_chunk_lookup_miss_count = register_int_counter_vec_with_registry!(
            "stream_agg_chunk_lookup_miss_count",
            "Aggregation executor chunk-level lookup miss duration",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let agg_chunk_total_lookup_count = register_int_counter_vec_with_registry!(
            "stream_agg_chunk_lookup_total_count",
            "Aggregation executor chunk-level lookup total operation",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let backfill_snapshot_read_row_count = register_guarded_int_counter_vec_with_registry!(
            "stream_backfill_snapshot_read_row_count",
            "Total number of rows that have been read from the backfill snapshot",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let backfill_upstream_output_row_count = register_guarded_int_counter_vec_with_registry!(
            "stream_backfill_upstream_output_row_count",
            "Total number of rows that have been output from the backfill upstream",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let cdc_backfill_snapshot_read_row_count = register_int_counter_vec_with_registry!(
            "stream_cdc_backfill_snapshot_read_row_count",
            "Total number of rows that have been read from the cdc_backfill snapshot",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let cdc_backfill_upstream_output_row_count = register_int_counter_vec_with_registry!(
            "stream_cdc_backfill_upstream_output_row_count",
            "Total number of rows that have been output from the cdc_backfill upstream",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let over_window_cached_entry_count = register_int_gauge_vec_with_registry!(
            "stream_over_window_cached_entry_count",
            "Total entry (partition) count in over window executor cache",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let over_window_cache_lookup_count = register_int_counter_vec_with_registry!(
            "stream_over_window_cache_lookup_count",
            "Over window executor cache lookup count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let over_window_cache_miss_count = register_int_counter_vec_with_registry!(
            "stream_over_window_cache_miss_count",
            "Over window executor cache miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let over_window_range_cache_entry_count = register_int_gauge_vec_with_registry!(
            "stream_over_window_range_cache_entry_count",
            "Over window partition range cache entry count",
            &["table_id", "actor_id", "fragment_id"],
            registry,
        )
        .unwrap();

        let over_window_range_cache_lookup_count = register_int_counter_vec_with_registry!(
            "stream_over_window_range_cache_lookup_count",
            "Over window partition range cache lookup count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let over_window_range_cache_left_miss_count = register_int_counter_vec_with_registry!(
            "stream_over_window_range_cache_left_miss_count",
            "Over window partition range cache left miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let over_window_range_cache_right_miss_count = register_int_counter_vec_with_registry!(
            "stream_over_window_range_cache_right_miss_count",
            "Over window partition range cache right miss count",
            &["table_id", "actor_id", "fragment_id"],
            registry
        )
        .unwrap();

        let opts = histogram_opts!(
            "stream_barrier_inflight_duration_seconds",
            "barrier_inflight_latency",
            exponential_buckets(0.1, 1.5, 16).unwrap() // max 43s
        );
        let barrier_inflight_latency = register_histogram_with_registry!(opts, registry).unwrap();

        let opts = histogram_opts!(
            "stream_barrier_sync_storage_duration_seconds",
            "barrier_sync_latency",
            exponential_buckets(0.1, 1.5, 16).unwrap() // max 43
        );
        let barrier_sync_latency = register_histogram_with_registry!(opts, registry).unwrap();

        let barrier_manager_progress = register_int_counter_with_registry!(
            "stream_barrier_manager_progress",
            "The number of actors that have processed the earliest in-flight barriers",
            registry
        )
        .unwrap();

        let sink_commit_duration = register_guarded_histogram_vec_with_registry!(
            "sink_commit_duration",
            "Duration of commit op in sink",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let connector_sink_rows_received = register_guarded_int_counter_vec_with_registry!(
            "connector_sink_rows_received",
            "Number of rows received by sink",
            &["connector_type", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_first_write_epoch = register_guarded_int_gauge_vec_with_registry!(
            "log_store_first_write_epoch",
            "The first write epoch of log store",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_latest_write_epoch = register_guarded_int_gauge_vec_with_registry!(
            "log_store_latest_write_epoch",
            "The latest write epoch of log store",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_write_rows = register_guarded_int_counter_vec_with_registry!(
            "log_store_write_rows",
            "The write rate of rows",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_latest_read_epoch = register_guarded_int_gauge_vec_with_registry!(
            "log_store_latest_read_epoch",
            "The latest read epoch of log store",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_read_rows = register_guarded_int_counter_vec_with_registry!(
            "log_store_read_rows",
            "The read rate of rows",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let log_store_reader_wait_new_future_duration_ns =
            register_guarded_int_counter_vec_with_registry!(
                "log_store_reader_wait_new_future_duration_ns",
                "Accumulated duration of LogReader to wait for next call to create future",
                &["executor_id", "connector", "sink_id"],
                registry
            )
            .unwrap();

        let kv_log_store_storage_write_count = register_guarded_int_counter_vec_with_registry!(
            "kv_log_store_storage_write_count",
            "Write row count throughput of kv log store",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let kv_log_store_storage_write_size = register_guarded_int_counter_vec_with_registry!(
            "kv_log_store_storage_write_size",
            "Write size throughput of kv log store",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let kv_log_store_storage_read_count = register_guarded_int_counter_vec_with_registry!(
            "kv_log_store_storage_read_count",
            "Write row count throughput of kv log store",
            &["executor_id", "connector", "sink_id", "read_type"],
            registry
        )
        .unwrap();

        let kv_log_store_storage_read_size = register_guarded_int_counter_vec_with_registry!(
            "kv_log_store_storage_read_size",
            "Write size throughput of kv log store",
            &["executor_id", "connector", "sink_id", "read_type"],
            registry
        )
        .unwrap();

        let kv_log_store_rewind_count = register_guarded_int_counter_vec_with_registry!(
            "kv_log_store_rewind_count",
            "Kv log store rewind rate",
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let kv_log_store_rewind_delay_opts = {
            assert_eq!(2, REWIND_BACKOFF_FACTOR);
            let bucket_count = (REWIND_MAX_DELAY.as_secs_f64().log2()
                - REWIND_BASE_DELAY.as_secs_f64().log2())
            .ceil() as usize;
            let buckets = exponential_buckets(
                REWIND_BASE_DELAY.as_secs_f64(),
                REWIND_BACKOFF_FACTOR as _,
                bucket_count,
            )
            .unwrap();
            histogram_opts!(
                "kv_log_store_rewind_delay",
                "Kv log store rewind delay",
                buckets,
            )
        };

        let kv_log_store_rewind_delay = register_guarded_histogram_vec_with_registry!(
            kv_log_store_rewind_delay_opts,
            &["executor_id", "connector", "sink_id"],
            registry
        )
        .unwrap();

        let kv_log_store_buffer_unconsumed_item_count =
            register_guarded_int_gauge_vec_with_registry!(
                "kv_log_store_buffer_unconsumed_item_count",
                "Number of Unconsumed Item in buffer",
                &["executor_id", "connector", "sink_id"],
                registry
            )
            .unwrap();

        let kv_log_store_buffer_unconsumed_row_count =
            register_guarded_int_gauge_vec_with_registry!(
                "kv_log_store_buffer_unconsumed_row_count",
                "Number of Unconsumed Row in buffer",
                &["executor_id", "connector", "sink_id"],
                registry
            )
            .unwrap();

        let kv_log_store_buffer_unconsumed_epoch_count =
            register_guarded_int_gauge_vec_with_registry!(
                "kv_log_store_buffer_unconsumed_epoch_count",
                "Number of Unconsumed Epoch in buffer",
                &["executor_id", "connector", "sink_id"],
                registry
            )
            .unwrap();

        let kv_log_store_buffer_unconsumed_min_epoch =
            register_guarded_int_gauge_vec_with_registry!(
                "kv_log_store_buffer_unconsumed_min_epoch",
                "Number of Unconsumed Epoch in buffer",
                &["executor_id", "connector", "sink_id"],
                registry
            )
            .unwrap();

        let lru_current_watermark_time_ms = register_int_gauge_with_registry!(
            "lru_current_watermark_time_ms",
            "Current LRU manager watermark time(ms)",
            registry
        )
        .unwrap();

        let lru_physical_now_ms = register_int_gauge_with_registry!(
            "lru_physical_now_ms",
            "Current physical time in Risingwave(ms)",
            registry
        )
        .unwrap();

        let lru_runtime_loop_count = register_int_counter_with_registry!(
            "lru_runtime_loop_count",
            "The counts of the eviction loop in LRU manager per second",
            registry
        )
        .unwrap();

        let lru_watermark_step = register_int_gauge_with_registry!(
            "lru_watermark_step",
            "The steps increase in 1 loop",
            registry
        )
        .unwrap();

        let lru_evicted_watermark_time_ms = register_guarded_int_gauge_vec_with_registry!(
            "lru_evicted_watermark_time_ms",
            "The latest evicted watermark time by actors",
            &["table_id", "actor_id", "desc"],
            registry
        )
        .unwrap();

        let jemalloc_allocated_bytes = register_int_gauge_with_registry!(
            "jemalloc_allocated_bytes",
            "The allocated memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let jemalloc_active_bytes = register_int_gauge_with_registry!(
            "jemalloc_active_bytes",
            "The active memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let jemalloc_resident_bytes = register_int_gauge_with_registry!(
            "jemalloc_resident_bytes",
            "The active memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let jemalloc_metadata_bytes = register_int_gauge_with_registry!(
            "jemalloc_metadata_bytes",
            "The active memory jemalloc, got from jemalloc_ctl",
            registry
        )
        .unwrap();

        let jvm_allocated_bytes = register_int_gauge_with_registry!(
            "jvm_allocated_bytes",
            "The allocated jvm memory",
            registry
        )
        .unwrap();

        let jvm_active_bytes = register_int_gauge_with_registry!(
            "jvm_active_bytes",
            "The active jvm memory",
            registry
        )
        .unwrap();

        let materialize_cache_hit_count = register_int_counter_vec_with_registry!(
            "stream_materialize_cache_hit_count",
            "Materialize executor cache hit count",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let materialize_cache_total_count = register_int_counter_vec_with_registry!(
            "stream_materialize_cache_total_count",
            "Materialize executor cache total operation",
            &["table_id", "actor_id"],
            registry
        )
        .unwrap();

        let stream_memory_usage = register_guarded_int_gauge_vec_with_registry!(
            "stream_memory_usage",
            "Memory usage for stream executors",
            &["table_id", "actor_id", "desc"],
            registry
        )
        .unwrap();

        let iceberg_write_qps = register_guarded_int_counter_vec_with_registry!(
            "iceberg_write_qps",
            "The qps of iceberg writer",
            &["executor_id", "sink_id"],
            registry
        )
        .unwrap();

        let iceberg_write_latency = register_guarded_histogram_vec_with_registry!(
            "iceberg_write_latency",
            "The latency of iceberg writer",
            &["executor_id", "sink_id"],
            registry
        )
        .unwrap();

        let iceberg_rolling_unflushed_data_file = register_guarded_int_gauge_vec_with_registry!(
            "iceberg_rolling_unflushed_data_file",
            "The unflushed data file count of iceberg rolling writer",
            &["executor_id", "sink_id"],
            registry
        )
        .unwrap();

        let iceberg_position_delete_cache_num = register_guarded_int_gauge_vec_with_registry!(
            "iceberg_position_delete_cache_num",
            "The delete cache num of iceberg position delete writer",
            &["executor_id", "sink_id"],
            registry
        )
        .unwrap();

        let iceberg_partition_num = register_guarded_int_gauge_vec_with_registry!(
            "iceberg_partition_num",
            "The partition num of iceberg partition writer",
            &["executor_id", "sink_id"],
            registry
        )
        .unwrap();

        Self {
            level,
            executor_row_count,
            actor_execution_time,
            actor_scheduled_duration,
            actor_scheduled_cnt,
            actor_fast_poll_duration,
            actor_fast_poll_cnt,
            actor_slow_poll_duration,
            actor_slow_poll_cnt,
            actor_poll_duration,
            actor_poll_cnt,
            actor_idle_duration,
            actor_idle_cnt,
            actor_memory_usage,
            actor_in_record_cnt,
            actor_out_record_cnt,
            source_output_row_count,
            source_split_change_count,
            source_backfill_row_count,
            sink_input_row_count,
            mview_input_row_count,
            sink_chunk_buffer_size,
            exchange_frag_recv_size,
            merge_barrier_align_duration,
            actor_output_buffer_blocking_duration_ns,
            actor_input_buffer_blocking_duration_ns,
            join_lookup_miss_count,
            join_lookup_total_count,
            join_insert_cache_miss_count,
            join_actor_input_waiting_duration_ns,
            join_match_duration_ns,
            join_cached_entry_count,
            join_matched_join_keys,
            barrier_align_duration,
            agg_lookup_miss_count,
            agg_total_lookup_count,
            agg_cached_entry_count,
            agg_chunk_lookup_miss_count,
            agg_chunk_total_lookup_count,
            agg_distinct_cache_miss_count,
            agg_distinct_total_cache_count,
            agg_distinct_cached_entry_count,
            agg_dirty_groups_count,
            agg_dirty_groups_heap_size,
            group_top_n_cache_miss_count,
            group_top_n_total_query_cache_count,
            group_top_n_cached_entry_count,
            group_top_n_appendonly_cache_miss_count,
            group_top_n_appendonly_total_query_cache_count,
            group_top_n_appendonly_cached_entry_count,
            lookup_cache_miss_count,
            lookup_total_query_cache_count,
            lookup_cached_entry_count,
            temporal_join_cache_miss_count,
            temporal_join_total_query_cache_count,
            temporal_join_cached_entry_count,
            backfill_snapshot_read_row_count,
            backfill_upstream_output_row_count,
            cdc_backfill_snapshot_read_row_count,
            cdc_backfill_upstream_output_row_count,
            over_window_cached_entry_count,
            over_window_cache_lookup_count,
            over_window_cache_miss_count,
            over_window_range_cache_entry_count,
            over_window_range_cache_lookup_count,
            over_window_range_cache_left_miss_count,
            over_window_range_cache_right_miss_count,
            barrier_inflight_latency,
            barrier_sync_latency,
            barrier_manager_progress,
            sink_commit_duration,
            connector_sink_rows_received,
            log_store_first_write_epoch,
            log_store_latest_write_epoch,
            log_store_write_rows,
            log_store_latest_read_epoch,
            log_store_read_rows,
            log_store_reader_wait_new_future_duration_ns,
            kv_log_store_storage_write_count,
            kv_log_store_storage_write_size,
            kv_log_store_rewind_count,
            kv_log_store_rewind_delay,
            kv_log_store_storage_read_count,
            kv_log_store_storage_read_size,
            kv_log_store_buffer_unconsumed_item_count,
            kv_log_store_buffer_unconsumed_row_count,
            kv_log_store_buffer_unconsumed_epoch_count,
            kv_log_store_buffer_unconsumed_min_epoch,
            iceberg_write_qps,
            iceberg_write_latency,
            iceberg_rolling_unflushed_data_file,
            iceberg_position_delete_cache_num,
            iceberg_partition_num,
            lru_current_watermark_time_ms,
            lru_physical_now_ms,
            lru_runtime_loop_count,
            lru_watermark_step,
            lru_evicted_watermark_time_ms,
            jemalloc_allocated_bytes,
            jemalloc_active_bytes,
            jemalloc_resident_bytes,
            jemalloc_metadata_bytes,
            jvm_allocated_bytes,
            jvm_active_bytes,
            materialize_cache_hit_count,
            materialize_cache_total_count,
            stream_memory_usage,
        }
    }

    /// Create a new `StreamingMetrics` instance used in tests or other places.
    pub fn unused() -> Self {
        global_streaming_metrics(MetricLevel::Disabled)
    }

    pub fn new_sink_metrics(
        &self,
        identity: &str,
        sink_id_str: &str,
        connector: &str,
    ) -> SinkMetrics {
        let label_list = [identity, connector, sink_id_str];
        let sink_commit_duration_metrics = self
            .sink_commit_duration
            .with_guarded_label_values(&label_list);
        let connector_sink_rows_received = self
            .connector_sink_rows_received
            .with_guarded_label_values(&[connector, sink_id_str]);

        let log_store_latest_read_epoch = self
            .log_store_latest_read_epoch
            .with_guarded_label_values(&label_list);

        let log_store_latest_write_epoch = self
            .log_store_latest_write_epoch
            .with_guarded_label_values(&label_list);

        let log_store_first_write_epoch = self
            .log_store_first_write_epoch
            .with_guarded_label_values(&label_list);

        let log_store_write_rows = self
            .log_store_write_rows
            .with_guarded_label_values(&label_list);
        let log_store_read_rows = self
            .log_store_read_rows
            .with_guarded_label_values(&label_list);
        let log_store_reader_wait_new_future_duration_ns = self
            .log_store_reader_wait_new_future_duration_ns
            .with_guarded_label_values(&label_list);

        let label_list = [identity, sink_id_str];
        let iceberg_write_qps = self
            .iceberg_write_qps
            .with_guarded_label_values(&label_list);
        let iceberg_write_latency = self
            .iceberg_write_latency
            .with_guarded_label_values(&label_list);
        let iceberg_rolling_unflushed_data_file = self
            .iceberg_rolling_unflushed_data_file
            .with_guarded_label_values(&label_list);
        let iceberg_position_delete_cache_num = self
            .iceberg_position_delete_cache_num
            .with_guarded_label_values(&label_list);
        let iceberg_partition_num = self
            .iceberg_partition_num
            .with_guarded_label_values(&label_list);

        SinkMetrics {
            sink_commit_duration_metrics,
            connector_sink_rows_received,
            log_store_first_write_epoch,
            log_store_latest_write_epoch,
            log_store_write_rows,
            log_store_latest_read_epoch,
            log_store_read_rows,
            log_store_reader_wait_new_future_duration_ns,
            iceberg_write_qps,
            iceberg_write_latency,
            iceberg_rolling_unflushed_data_file,
            iceberg_position_delete_cache_num,
            iceberg_partition_num,
        }
    }
}
