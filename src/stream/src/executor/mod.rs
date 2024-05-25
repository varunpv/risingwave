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

mod prelude;

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::sync::Arc;

use await_tree::InstrumentAwait;
use enum_as_inner::EnumAsInner;
use futures::stream::BoxStream;
use futures::{Stream, StreamExt};
use itertools::Itertools;
use risingwave_common::array::StreamChunk;
use risingwave_common::buffer::Bitmap;
use risingwave_common::catalog::{Schema, TableId};
use risingwave_common::row::OwnedRow;
use risingwave_common::types::{DataType, Datum, DefaultOrd, ScalarImpl};
use risingwave_common::util::epoch::{Epoch, EpochPair};
use risingwave_common::util::tracing::TracingContext;
use risingwave_common::util::value_encoding::{DatumFromProtoExt, DatumToProtoExt};
use risingwave_connector::source::SplitImpl;
use risingwave_expr::expr::{Expression, NonStrictExpression};
use risingwave_pb::data::PbEpoch;
use risingwave_pb::expr::PbInputRef;
use risingwave_pb::stream_plan::barrier::BarrierKind;
use risingwave_pb::stream_plan::barrier_mutation::Mutation as PbMutation;
use risingwave_pb::stream_plan::stream_message::StreamMessage;
use risingwave_pb::stream_plan::update_mutation::{DispatcherUpdate, MergeUpdate};
use risingwave_pb::stream_plan::{
    BarrierMutation, CombinedMutation, CreateSubscriptionMutation, Dispatchers,
    DropSubscriptionMutation, PauseMutation, PbAddMutation, PbBarrier, PbDispatcher,
    PbStreamMessage, PbUpdateMutation, PbWatermark, ResumeMutation, SourceChangeSplitMutation,
    StopMutation, ThrottleMutation,
};
use smallvec::SmallVec;

use crate::error::StreamResult;
use crate::task::{ActorId, FragmentId};

mod actor;
mod barrier_align;
pub mod exchange;
pub mod monitor;

pub mod agg_common;
pub mod aggregation;
mod backfill;
mod barrier_recv;
mod batch_query;
mod chain;
mod dedup;
mod dispatch;
pub mod dml;
mod dynamic_filter;
pub mod error;
mod expand;
mod filter;
mod hash_agg;
pub mod hash_join;
mod hop_window;
mod join;
mod lookup;
mod lookup_union;
mod merge;
mod mview;
mod no_op;
mod now;
mod over_window;
mod project;
mod project_set;
mod rearranged_chain;
mod receiver;
pub mod row_id_gen;
mod simple_agg;
mod sink;
mod sort;
mod sort_buffer;
pub mod source;
mod stateless_simple_agg;
mod stream_reader;
pub mod subtask;
mod temporal_join;
mod top_n;
mod troublemaker;
mod union;
mod values;
mod watermark;
mod watermark_filter;
mod wrapper;

#[cfg(test)]
mod integration_tests;
pub mod test_utils;
mod utils;

pub use actor::{Actor, ActorContext, ActorContextRef};
use anyhow::Context;
pub use backfill::arrangement_backfill::*;
pub use backfill::cdc::{CdcBackfillExecutor, CdcScanOptions, ExternalStorageTable};
pub use backfill::no_shuffle_backfill::*;
pub use barrier_recv::BarrierRecvExecutor;
pub use batch_query::BatchQueryExecutor;
pub use chain::ChainExecutor;
pub use dedup::AppendOnlyDedupExecutor;
pub use dispatch::{DispatchExecutor, DispatcherImpl};
pub use dynamic_filter::DynamicFilterExecutor;
pub use error::{StreamExecutorError, StreamExecutorResult};
pub use expand::ExpandExecutor;
pub use filter::FilterExecutor;
pub use hash_agg::HashAggExecutor;
pub use hash_join::*;
pub use hop_window::HopWindowExecutor;
pub use join::JoinType;
pub use lookup::*;
pub use lookup_union::LookupUnionExecutor;
pub use merge::MergeExecutor;
pub use mview::*;
pub use no_op::NoOpExecutor;
pub use now::NowExecutor;
pub use over_window::*;
pub use project::ProjectExecutor;
pub use project_set::*;
pub use rearranged_chain::RearrangedChainExecutor;
pub use receiver::ReceiverExecutor;
use risingwave_pb::source::{ConnectorSplit, ConnectorSplits};
pub use simple_agg::SimpleAggExecutor;
pub use sink::SinkExecutor;
pub use sort::*;
pub use stateless_simple_agg::StatelessSimpleAggExecutor;
pub use temporal_join::*;
pub use top_n::{
    AppendOnlyGroupTopNExecutor, AppendOnlyTopNExecutor, GroupTopNExecutor, TopNExecutor,
};
pub use troublemaker::TroublemakerExecutor;
pub use union::UnionExecutor;
pub use utils::DummyExecutor;
pub use values::ValuesExecutor;
pub use watermark_filter::WatermarkFilterExecutor;
pub use wrapper::WrapperExecutor;

use self::barrier_align::AlignedMessageStream;

pub type MessageStreamItem = StreamExecutorResult<Message>;
pub type BoxedMessageStream = BoxStream<'static, MessageStreamItem>;

pub use risingwave_common::util::epoch::task_local::{curr_epoch, epoch, prev_epoch};
use risingwave_pb::stream_plan::throttle_mutation::RateLimit;

pub trait MessageStream = futures::Stream<Item = MessageStreamItem> + Send;

/// Static information of an executor.
#[derive(Debug, Default, Clone)]
pub struct ExecutorInfo {
    /// The schema of the OUTPUT of the executor.
    pub schema: Schema,

    /// The primary key indices of the OUTPUT of the executor.
    /// Schema is used by both OLAP and streaming, therefore
    /// pk indices are maintained independently.
    pub pk_indices: PkIndices,

    /// Identity of the executor.
    pub identity: String,
}

/// [`Execute`] describes the methods an executor should implement to handle control messages.
pub trait Execute: Send + 'static {
    fn execute(self: Box<Self>) -> BoxedMessageStream;

    fn execute_with_epoch(self: Box<Self>, _epoch: u64) -> BoxedMessageStream {
        self.execute()
    }

    fn boxed(self) -> Box<dyn Execute>
    where
        Self: Sized + Send + 'static,
    {
        Box::new(self)
    }
}

/// [`Executor`] combines the static information ([`ExecutorInfo`]) and the executable object to
/// handle messages ([`Execute`]).
pub struct Executor {
    info: ExecutorInfo,
    execute: Box<dyn Execute>,
}

impl Executor {
    pub fn new(info: ExecutorInfo, execute: Box<dyn Execute>) -> Self {
        Self { info, execute }
    }

    pub fn info(&self) -> &ExecutorInfo {
        &self.info
    }

    pub fn schema(&self) -> &Schema {
        &self.info.schema
    }

    pub fn pk_indices(&self) -> PkIndicesRef<'_> {
        &self.info.pk_indices
    }

    pub fn identity(&self) -> &str {
        &self.info.identity
    }

    pub fn execute(self) -> BoxedMessageStream {
        self.execute.execute()
    }

    pub fn execute_with_epoch(self, epoch: u64) -> BoxedMessageStream {
        self.execute.execute_with_epoch(epoch)
    }
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.identity())
    }
}

impl From<(ExecutorInfo, Box<dyn Execute>)> for Executor {
    fn from((info, execute): (ExecutorInfo, Box<dyn Execute>)) -> Self {
        Self::new(info, execute)
    }
}

impl<E> From<(ExecutorInfo, E)> for Executor
where
    E: Execute,
{
    fn from((info, execute): (ExecutorInfo, E)) -> Self {
        Self::new(info, execute.boxed())
    }
}

pub const INVALID_EPOCH: u64 = 0;

type UpstreamFragmentId = FragmentId;

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateMutation {
    pub dispatchers: HashMap<ActorId, Vec<DispatcherUpdate>>,
    pub merges: HashMap<(ActorId, UpstreamFragmentId), MergeUpdate>,
    pub vnode_bitmaps: HashMap<ActorId, Arc<Bitmap>>,
    pub dropped_actors: HashSet<ActorId>,
    pub actor_splits: HashMap<ActorId, Vec<SplitImpl>>,
    pub actor_new_dispatchers: HashMap<ActorId, Vec<PbDispatcher>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AddMutation {
    pub adds: HashMap<ActorId, Vec<PbDispatcher>>,
    pub added_actors: HashSet<ActorId>,
    // TODO: remove this and use `SourceChangesSplit` after we support multiple mutations.
    pub splits: HashMap<ActorId, Vec<SplitImpl>>,
    pub pause: bool,
}

/// See [`PbMutation`] for the semantics of each mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutation {
    Stop(HashSet<ActorId>),
    Update(UpdateMutation),
    Add(AddMutation),
    SourceChangeSplit(HashMap<ActorId, Vec<SplitImpl>>),
    Pause,
    Resume,
    Throttle(HashMap<ActorId, Option<u32>>),
    AddAndUpdate(AddMutation, UpdateMutation),
    CreateSubscription {
        subscription_id: u32,
        upstream_mv_table_id: TableId,
    },
    DropSubscription {
        subscription_id: u32,
        upstream_mv_table_id: TableId,
    },
}

#[derive(Debug, Clone)]
pub struct Barrier {
    pub epoch: EpochPair,
    pub mutation: Option<Arc<Mutation>>,
    pub kind: BarrierKind,

    /// Tracing context for the **current** epoch of this barrier.
    tracing_context: TracingContext,

    /// The actors that this barrier has passed locally. Used for debugging only.
    pub passed_actors: Vec<ActorId>,
}

impl Barrier {
    /// Create a plain barrier.
    pub fn new_test_barrier(epoch: u64) -> Self {
        Self {
            epoch: EpochPair::new_test_epoch(epoch),
            kind: BarrierKind::Checkpoint,
            tracing_context: TracingContext::none(),
            mutation: Default::default(),
            passed_actors: Default::default(),
        }
    }

    pub fn with_prev_epoch_for_test(epoch: u64, prev_epoch: u64) -> Self {
        Self {
            epoch: EpochPair::new(epoch, prev_epoch),
            kind: BarrierKind::Checkpoint,
            tracing_context: TracingContext::none(),
            mutation: Default::default(),
            passed_actors: Default::default(),
        }
    }

    #[must_use]
    pub fn with_mutation(self, mutation: Mutation) -> Self {
        Self {
            mutation: Some(Arc::new(mutation)),
            ..self
        }
    }

    #[must_use]
    pub fn with_stop(self) -> Self {
        self.with_mutation(Mutation::Stop(HashSet::default()))
    }

    /// Whether this barrier carries stop mutation.
    pub fn is_with_stop_mutation(&self) -> bool {
        matches!(self.mutation.as_deref(), Some(Mutation::Stop(_)))
    }

    /// Whether this barrier is to stop the actor with `actor_id`.
    pub fn is_stop(&self, actor_id: ActorId) -> bool {
        self.all_stop_actors()
            .map_or(false, |actors| actors.contains(&actor_id))
    }

    /// Get all actors that to be stopped (dropped) by this barrier.
    pub fn all_stop_actors(&self) -> Option<&HashSet<ActorId>> {
        match self.mutation.as_deref() {
            Some(Mutation::Stop(actors)) => Some(actors),
            Some(Mutation::Update(UpdateMutation { dropped_actors, .. }))
            | Some(Mutation::AddAndUpdate(_, UpdateMutation { dropped_actors, .. })) => {
                Some(dropped_actors)
            }
            _ => None,
        }
    }

    /// Whether this barrier is to newly add the actor with `actor_id`. This is used for `Chain` and
    /// `Values` to decide whether to output the existing (historical) data.
    ///
    /// By "newly", we mean the actor belongs to a subgraph of a new streaming job. That is, actors
    /// added for scaling are not included.
    pub fn is_newly_added(&self, actor_id: ActorId) -> bool {
        match self.mutation.as_deref() {
            Some(Mutation::Add(AddMutation { added_actors, .. }))
            | Some(Mutation::AddAndUpdate(AddMutation { added_actors, .. }, _)) => {
                added_actors.contains(&actor_id)
            }
            _ => false,
        }
    }

    /// Whether this barrier adds new downstream fragment for the actor with `upstream_actor_id`.
    ///
    /// # Use case
    /// Some optimizations are applied when an actor doesn't have any downstreams ("standalone" actors).
    /// * Pause a standalone shared `SourceExecutor`.
    /// * Disable a standalone `MaterializeExecutor`'s conflict check.
    ///
    /// This is implemented by checking `actor_context.initial_dispatch_num` on startup, and
    /// check `has_more_downstream_fragments` on barrier to see whether the optimization
    /// needs to be turned off.
    ///
    /// ## Some special cases not included
    ///
    /// Note that this is not `has_new_downstream_actor/fragment`. For our use case, we only
    /// care about **number of downstream fragments** (more precisely, existence).
    /// - When scaling, the number of downstream actors is changed, and they are "new", but downstream fragments is not changed.
    /// - When `ALTER TABLE sink_into_table`, the fragment is replaced with a "new" one, but the number is not changed.
    pub fn has_more_downstream_fragments(&self, upstream_actor_id: ActorId) -> bool {
        let Some(mutation) = self.mutation.as_deref() else {
            return false;
        };
        match mutation {
            // Add is for mv, index and sink creation.
            Mutation::Add(AddMutation { adds, .. }) => adds.get(&upstream_actor_id).is_some(),
            // AddAndUpdate is for sink-into-table.
            Mutation::AddAndUpdate(
                AddMutation { adds, .. },
                UpdateMutation {
                    dispatchers,
                    actor_new_dispatchers,
                    ..
                },
            ) => {
                adds.get(&upstream_actor_id).is_some()
                    || actor_new_dispatchers.get(&upstream_actor_id).is_some()
                    || dispatchers.get(&upstream_actor_id).is_some()
            }
            Mutation::Update(_)
            | Mutation::Stop(_)
            | Mutation::Pause
            | Mutation::Resume
            | Mutation::SourceChangeSplit(_)
            | Mutation::Throttle(_)
            | Mutation::CreateSubscription { .. }
            | Mutation::DropSubscription { .. } => false,
        }
    }

    /// Whether this barrier requires the executor to pause its data stream on startup.
    pub fn is_pause_on_startup(&self) -> bool {
        match self.mutation.as_deref() {
            Some(
                  Mutation::Update { .. } // new actors for scaling
                | Mutation::Add(AddMutation { pause: true, .. }) // new streaming job, or recovery
            ) => true,
            _ => false,
        }
    }

    /// Whether this barrier is for resume.
    pub fn is_resume(&self) -> bool {
        matches!(self.mutation.as_deref(), Some(Mutation::Resume))
    }

    /// Returns the [`MergeUpdate`] if this barrier is to update the merge executors for the actor
    /// with `actor_id`.
    pub fn as_update_merge(
        &self,
        actor_id: ActorId,
        upstream_fragment_id: UpstreamFragmentId,
    ) -> Option<&MergeUpdate> {
        self.mutation
            .as_deref()
            .and_then(|mutation| match mutation {
                Mutation::Update(UpdateMutation { merges, .. })
                | Mutation::AddAndUpdate(_, UpdateMutation { merges, .. }) => {
                    merges.get(&(actor_id, upstream_fragment_id))
                }

                _ => None,
            })
    }

    /// Returns the new vnode bitmap if this barrier is to update the vnode bitmap for the actor
    /// with `actor_id`.
    ///
    /// Actually, this vnode bitmap update is only useful for the record accessing validation for
    /// distributed executors, since the read/write pattern will never be across multiple vnodes.
    pub fn as_update_vnode_bitmap(&self, actor_id: ActorId) -> Option<Arc<Bitmap>> {
        self.mutation
            .as_deref()
            .and_then(|mutation| match mutation {
                Mutation::Update(UpdateMutation { vnode_bitmaps, .. })
                | Mutation::AddAndUpdate(_, UpdateMutation { vnode_bitmaps, .. }) => {
                    vnode_bitmaps.get(&actor_id).cloned()
                }
                _ => None,
            })
    }

    pub fn get_curr_epoch(&self) -> Epoch {
        Epoch(self.epoch.curr)
    }

    /// Retrieve the tracing context for the **current** epoch of this barrier.
    pub fn tracing_context(&self) -> &TracingContext {
        &self.tracing_context
    }
}

impl PartialEq for Barrier {
    fn eq(&self, other: &Self) -> bool {
        self.epoch == other.epoch && self.mutation == other.mutation
    }
}

impl Mutation {
    /// Return true if the mutation is stop.
    ///
    /// Note that this does not mean we will stop the current actor.
    #[cfg(test)]
    pub fn is_stop(&self) -> bool {
        matches!(self, Mutation::Stop(_))
    }

    fn to_protobuf(&self) -> PbMutation {
        let actor_splits_to_protobuf = |actor_splits: &HashMap<ActorId, Vec<SplitImpl>>| {
            actor_splits
                .iter()
                .map(|(&actor_id, splits)| {
                    (
                        actor_id,
                        ConnectorSplits {
                            splits: splits.clone().iter().map(ConnectorSplit::from).collect(),
                        },
                    )
                })
                .collect::<HashMap<_, _>>()
        };

        match self {
            Mutation::Stop(actors) => PbMutation::Stop(StopMutation {
                actors: actors.iter().copied().collect::<Vec<_>>(),
            }),
            Mutation::Update(UpdateMutation {
                dispatchers,
                merges,
                vnode_bitmaps,
                dropped_actors,
                actor_splits,
                actor_new_dispatchers,
            }) => PbMutation::Update(PbUpdateMutation {
                dispatcher_update: dispatchers.values().flatten().cloned().collect(),
                merge_update: merges.values().cloned().collect(),
                actor_vnode_bitmap_update: vnode_bitmaps
                    .iter()
                    .map(|(&actor_id, bitmap)| (actor_id, bitmap.to_protobuf()))
                    .collect(),
                dropped_actors: dropped_actors.iter().cloned().collect(),
                actor_splits: actor_splits_to_protobuf(actor_splits),
                actor_new_dispatchers: actor_new_dispatchers
                    .iter()
                    .map(|(&actor_id, dispatchers)| {
                        (
                            actor_id,
                            Dispatchers {
                                dispatchers: dispatchers.clone(),
                            },
                        )
                    })
                    .collect(),
            }),
            Mutation::Add(AddMutation {
                adds,
                added_actors,
                splits,
                pause,
            }) => PbMutation::Add(PbAddMutation {
                actor_dispatchers: adds
                    .iter()
                    .map(|(&actor_id, dispatchers)| {
                        (
                            actor_id,
                            Dispatchers {
                                dispatchers: dispatchers.clone(),
                            },
                        )
                    })
                    .collect(),
                added_actors: added_actors.iter().copied().collect(),
                actor_splits: actor_splits_to_protobuf(splits),
                pause: *pause,
            }),
            Mutation::SourceChangeSplit(changes) => PbMutation::Splits(SourceChangeSplitMutation {
                actor_splits: changes
                    .iter()
                    .map(|(&actor_id, splits)| {
                        (
                            actor_id,
                            ConnectorSplits {
                                splits: splits.clone().iter().map(ConnectorSplit::from).collect(),
                            },
                        )
                    })
                    .collect(),
            }),
            Mutation::Pause => PbMutation::Pause(PauseMutation {}),
            Mutation::Resume => PbMutation::Resume(ResumeMutation {}),
            Mutation::Throttle(changes) => PbMutation::Throttle(ThrottleMutation {
                actor_throttle: changes
                    .iter()
                    .map(|(actor_id, limit)| (*actor_id, RateLimit { rate_limit: *limit }))
                    .collect(),
            }),

            Mutation::AddAndUpdate(add, update) => PbMutation::Combined(CombinedMutation {
                mutations: vec![
                    BarrierMutation {
                        mutation: Some(Mutation::Add(add.clone()).to_protobuf()),
                    },
                    BarrierMutation {
                        mutation: Some(Mutation::Update(update.clone()).to_protobuf()),
                    },
                ],
            }),
            Mutation::CreateSubscription {
                upstream_mv_table_id,
                subscription_id,
            } => PbMutation::CreateSubscription(CreateSubscriptionMutation {
                upstream_mv_table_id: upstream_mv_table_id.table_id,
                subscription_id: *subscription_id,
            }),
            Mutation::DropSubscription {
                upstream_mv_table_id,
                subscription_id,
            } => PbMutation::DropSubscription(DropSubscriptionMutation {
                upstream_mv_table_id: upstream_mv_table_id.table_id,
                subscription_id: *subscription_id,
            }),
        }
    }

    fn from_protobuf(prost: &PbMutation) -> StreamExecutorResult<Self> {
        let mutation = match prost {
            PbMutation::Stop(stop) => Mutation::Stop(HashSet::from_iter(stop.get_actors().clone())),

            PbMutation::Update(update) => Mutation::Update(UpdateMutation {
                dispatchers: update
                    .dispatcher_update
                    .iter()
                    .map(|u| (u.actor_id, u.clone()))
                    .into_group_map(),
                merges: update
                    .merge_update
                    .iter()
                    .map(|u| ((u.actor_id, u.upstream_fragment_id), u.clone()))
                    .collect(),
                vnode_bitmaps: update
                    .actor_vnode_bitmap_update
                    .iter()
                    .map(|(&actor_id, bitmap)| (actor_id, Arc::new(bitmap.into())))
                    .collect(),
                dropped_actors: update.dropped_actors.iter().cloned().collect(),
                actor_splits: update
                    .actor_splits
                    .iter()
                    .map(|(&actor_id, splits)| {
                        (
                            actor_id,
                            splits
                                .splits
                                .iter()
                                .map(|split| split.try_into().unwrap())
                                .collect(),
                        )
                    })
                    .collect(),
                actor_new_dispatchers: update
                    .actor_new_dispatchers
                    .iter()
                    .map(|(&actor_id, dispatchers)| (actor_id, dispatchers.dispatchers.clone()))
                    .collect(),
            }),

            PbMutation::Add(add) => Mutation::Add(AddMutation {
                adds: add
                    .actor_dispatchers
                    .iter()
                    .map(|(&actor_id, dispatchers)| (actor_id, dispatchers.dispatchers.clone()))
                    .collect(),
                added_actors: add.added_actors.iter().copied().collect(),
                // TODO: remove this and use `SourceChangesSplit` after we support multiple
                // mutations.
                splits: add
                    .actor_splits
                    .iter()
                    .map(|(&actor_id, splits)| {
                        (
                            actor_id,
                            splits
                                .splits
                                .iter()
                                .map(|split| split.try_into().unwrap())
                                .collect(),
                        )
                    })
                    .collect(),
                pause: add.pause,
            }),

            PbMutation::Splits(s) => {
                let mut change_splits: Vec<(ActorId, Vec<SplitImpl>)> =
                    Vec::with_capacity(s.actor_splits.len());
                for (&actor_id, splits) in &s.actor_splits {
                    if !splits.splits.is_empty() {
                        change_splits.push((
                            actor_id,
                            splits
                                .splits
                                .iter()
                                .map(SplitImpl::try_from)
                                .try_collect()?,
                        ));
                    }
                }
                Mutation::SourceChangeSplit(change_splits.into_iter().collect())
            }
            PbMutation::Pause(_) => Mutation::Pause,
            PbMutation::Resume(_) => Mutation::Resume,
            PbMutation::Throttle(changes) => Mutation::Throttle(
                changes
                    .actor_throttle
                    .iter()
                    .map(|(actor_id, limit)| (*actor_id, limit.rate_limit))
                    .collect(),
            ),
            PbMutation::CreateSubscription(create) => Mutation::CreateSubscription {
                upstream_mv_table_id: TableId::new(create.upstream_mv_table_id),
                subscription_id: create.subscription_id,
            },
            PbMutation::DropSubscription(drop) => Mutation::DropSubscription {
                upstream_mv_table_id: TableId::new(drop.upstream_mv_table_id),
                subscription_id: drop.subscription_id,
            },
            PbMutation::Combined(CombinedMutation { mutations }) => match &mutations[..] {
                [BarrierMutation {
                    mutation: Some(add),
                }, BarrierMutation {
                    mutation: Some(update),
                }] => {
                    let Mutation::Add(add_mutation) = Mutation::from_protobuf(add)? else {
                        unreachable!();
                    };

                    let Mutation::Update(update_mutation) = Mutation::from_protobuf(update)? else {
                        unreachable!();
                    };

                    Mutation::AddAndUpdate(add_mutation, update_mutation)
                }

                _ => unreachable!(),
            },
        };
        Ok(mutation)
    }
}

impl Barrier {
    pub fn to_protobuf(&self) -> PbBarrier {
        let Barrier {
            epoch,
            mutation,
            kind,
            passed_actors,
            tracing_context,
            ..
        } = self.clone();

        PbBarrier {
            epoch: Some(PbEpoch {
                curr: epoch.curr,
                prev: epoch.prev,
            }),
            mutation: mutation.map(|m| BarrierMutation {
                mutation: Some(m.to_protobuf()),
            }),
            tracing_context: tracing_context.to_protobuf(),
            kind: kind as _,
            passed_actors,
        }
    }

    pub fn from_protobuf(prost: &PbBarrier) -> StreamExecutorResult<Self> {
        let mutation = prost
            .mutation
            .as_ref()
            .map(|m| Mutation::from_protobuf(m.mutation.as_ref().unwrap()))
            .transpose()?
            .map(Arc::new);
        let epoch = prost.get_epoch()?;

        Ok(Barrier {
            kind: prost.kind(),
            epoch: EpochPair::new(epoch.curr, epoch.prev),
            mutation,
            passed_actors: prost.get_passed_actors().clone(),
            tracing_context: TracingContext::from_protobuf(&prost.tracing_context),
        })
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Watermark {
    pub col_idx: usize,
    pub data_type: DataType,
    pub val: ScalarImpl,
}

impl PartialOrd for Watermark {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Watermark {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.val.default_cmp(&other.val)
    }
}

impl Watermark {
    pub fn new(col_idx: usize, data_type: DataType, val: ScalarImpl) -> Self {
        Self {
            col_idx,
            data_type,
            val,
        }
    }

    pub async fn transform_with_expr(
        self,
        expr: &NonStrictExpression<impl Expression>,
        new_col_idx: usize,
    ) -> Option<Self> {
        let Self { col_idx, val, .. } = self;
        let row = {
            let mut row = vec![None; col_idx + 1];
            row[col_idx] = Some(val);
            OwnedRow::new(row)
        };
        let val = expr.eval_row_infallible(&row).await?;
        Some(Self::new(new_col_idx, expr.inner().return_type(), val))
    }

    /// Transform the watermark with the given output indices. If this watermark is not in the
    /// output, return `None`.
    pub fn transform_with_indices(self, output_indices: &[usize]) -> Option<Self> {
        output_indices
            .iter()
            .position(|p| *p == self.col_idx)
            .map(|new_col_idx| self.with_idx(new_col_idx))
    }

    pub fn to_protobuf(&self) -> PbWatermark {
        PbWatermark {
            column: Some(PbInputRef {
                index: self.col_idx as _,
                r#type: Some(self.data_type.to_protobuf()),
            }),
            val: Some(&self.val).to_protobuf().into(),
        }
    }

    pub fn from_protobuf(prost: &PbWatermark) -> StreamExecutorResult<Self> {
        let col_ref = prost.get_column()?;
        let data_type = DataType::from(col_ref.get_type()?);
        let val = Datum::from_protobuf(prost.get_val()?, &data_type)?
            .expect("watermark value cannot be null");
        Ok(Self::new(col_ref.get_index() as _, data_type, val))
    }

    pub fn with_idx(self, idx: usize) -> Self {
        Self::new(idx, self.data_type, self.val)
    }
}

#[derive(Debug, EnumAsInner, PartialEq, Clone)]
pub enum Message {
    Chunk(StreamChunk),
    Barrier(Barrier),
    Watermark(Watermark),
}

impl From<StreamChunk> for Message {
    fn from(chunk: StreamChunk) -> Self {
        Message::Chunk(chunk)
    }
}

impl<'a> TryFrom<&'a Message> for &'a Barrier {
    type Error = ();

    fn try_from(m: &'a Message) -> std::result::Result<Self, Self::Error> {
        match m {
            Message::Chunk(_) => Err(()),
            Message::Barrier(b) => Ok(b),
            Message::Watermark(_) => Err(()),
        }
    }
}

impl Message {
    /// Return true if the message is a stop barrier, meaning the stream
    /// will not continue, false otherwise.
    ///
    /// Note that this does not mean we will stop the current actor.
    #[cfg(test)]
    pub fn is_stop(&self) -> bool {
        matches!(
            self,
            Message::Barrier(Barrier {
                mutation,
                ..
            }) if mutation.as_ref().unwrap().is_stop()
        )
    }

    pub fn to_protobuf(&self) -> PbStreamMessage {
        let prost = match self {
            Self::Chunk(stream_chunk) => {
                let prost_stream_chunk = stream_chunk.to_protobuf();
                StreamMessage::StreamChunk(prost_stream_chunk)
            }
            Self::Barrier(barrier) => StreamMessage::Barrier(barrier.clone().to_protobuf()),
            Self::Watermark(watermark) => StreamMessage::Watermark(watermark.to_protobuf()),
        };
        PbStreamMessage {
            stream_message: Some(prost),
        }
    }

    pub fn from_protobuf(prost: &PbStreamMessage) -> StreamExecutorResult<Self> {
        let res = match prost.get_stream_message()? {
            StreamMessage::StreamChunk(chunk) => Message::Chunk(StreamChunk::from_protobuf(chunk)?),
            StreamMessage::Barrier(barrier) => Message::Barrier(Barrier::from_protobuf(barrier)?),
            StreamMessage::Watermark(watermark) => {
                Message::Watermark(Watermark::from_protobuf(watermark)?)
            }
        };
        Ok(res)
    }

    pub fn get_encoded_len(msg: &impl ::prost::Message) -> usize {
        ::prost::Message::encoded_len(msg)
    }
}

pub type PkIndices = Vec<usize>;
pub type PkIndicesRef<'a> = &'a [usize];
pub type PkDataTypes = SmallVec<[DataType; 1]>;

/// Expect the first message of the given `stream` as a barrier.
pub async fn expect_first_barrier(
    stream: &mut (impl MessageStream + Unpin),
) -> StreamExecutorResult<Barrier> {
    let message = stream
        .next()
        .instrument_await("expect_first_barrier")
        .await
        .context("failed to extract the first message: stream closed unexpectedly")??;
    let barrier = message
        .into_barrier()
        .expect("the first message must be a barrier");
    // TODO: Is this check correct?
    assert!(matches!(
        barrier.kind,
        BarrierKind::Checkpoint | BarrierKind::Initial
    ));
    Ok(barrier)
}

/// Expect the first message of the given `stream` as a barrier.
pub async fn expect_first_barrier_from_aligned_stream(
    stream: &mut (impl AlignedMessageStream + Unpin),
) -> StreamExecutorResult<Barrier> {
    let message = stream
        .next()
        .instrument_await("expect_first_barrier")
        .await
        .context("failed to extract the first message: stream closed unexpectedly")??;
    let barrier = message
        .into_barrier()
        .expect("the first message must be a barrier");
    Ok(barrier)
}

/// `StreamConsumer` is the last step in an actor.
pub trait StreamConsumer: Send + 'static {
    type BarrierStream: Stream<Item = StreamResult<Barrier>> + Send;

    fn execute(self: Box<Self>) -> Self::BarrierStream;
}
