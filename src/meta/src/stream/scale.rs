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

use std::cmp::{min, Ordering};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::iter::repeat;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::future::{try_join_all, BoxFuture};
use itertools::Itertools;
use num_integer::Integer;
use num_traits::abs;
use risingwave_common::bail;
use risingwave_common::buffer::{Bitmap, BitmapBuilder};
use risingwave_common::catalog::TableId;
use risingwave_common::hash::{ActorMapping, ParallelUnitId, VirtualNode};
use risingwave_common::util::iter_util::ZipEqDebug;
use risingwave_meta_model_v2::StreamingParallelism;
use risingwave_pb::common::{
    ActorInfo, Buffer, ParallelUnit, ParallelUnitMapping, WorkerNode, WorkerType,
};
use risingwave_pb::meta::get_reschedule_plan_request::{Policy, StableResizePolicy};
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::table_fragments::actor_status::ActorState;
use risingwave_pb::meta::table_fragments::fragment::{
    FragmentDistributionType, PbFragmentDistributionType,
};
use risingwave_pb::meta::table_fragments::{self, ActorStatus, PbFragment, State};
use risingwave_pb::meta::FragmentParallelUnitMappings;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::{
    Dispatcher, DispatcherType, FragmentTypeFlag, PbStreamActor, StreamNode,
};
use risingwave_pb::stream_service::build_actor_info::SubscriptionIds;
use risingwave_pb::stream_service::BuildActorInfo;
use thiserror_ext::AsReport;
use tokio::sync::oneshot::Receiver;
use tokio::sync::{oneshot, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::warn;

use crate::barrier::{Command, Reschedule, StreamRpcManager};
use crate::manager::{
    IdCategory, IdGenManagerImpl, LocalNotification, MetaSrvEnv, MetadataManager, WorkerId,
};
use crate::model::{ActorId, DispatcherId, FragmentId, TableFragments, TableParallelism};
use crate::serving::{
    to_deleted_fragment_parallel_unit_mapping, to_fragment_parallel_unit_mapping,
    ServingVnodeMapping,
};
use crate::storage::{MetaStore, MetaStoreError, MetaStoreRef, Transaction, DEFAULT_COLUMN_FAMILY};
use crate::stream::{GlobalStreamManager, SourceManagerRef};
use crate::{model, MetaError, MetaResult};

#[derive(Default, Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct TableRevision(u64);

const TABLE_REVISION_KEY: &[u8] = b"table_revision";

impl From<TableRevision> for u64 {
    fn from(value: TableRevision) -> Self {
        value.0
    }
}

impl TableRevision {
    pub async fn get(store: &MetaStoreRef) -> MetaResult<Self> {
        let version = match store
            .get_cf(DEFAULT_COLUMN_FAMILY, TABLE_REVISION_KEY)
            .await
        {
            Ok(byte_vec) => memcomparable::from_slice(&byte_vec).unwrap(),
            Err(MetaStoreError::ItemNotFound(_)) => 0,
            Err(e) => return Err(MetaError::from(e)),
        };

        Ok(Self(version))
    }

    pub fn next(&self) -> Self {
        TableRevision(self.0 + 1)
    }

    pub fn store(&self, txn: &mut Transaction) {
        txn.put(
            DEFAULT_COLUMN_FAMILY.to_string(),
            TABLE_REVISION_KEY.to_vec(),
            memcomparable::to_vec(&self.0).unwrap(),
        );
    }

    pub fn inner(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ParallelUnitReschedule {
    pub added_parallel_units: BTreeSet<ParallelUnitId>,
    pub removed_parallel_units: BTreeSet<ParallelUnitId>,
}

pub struct CustomFragmentInfo {
    pub fragment_id: u32,
    pub fragment_type_mask: u32,
    pub distribution_type: PbFragmentDistributionType,
    pub vnode_mapping: Option<ParallelUnitMapping>,
    pub state_table_ids: Vec<u32>,
    pub upstream_fragment_ids: Vec<u32>,
    pub actor_template: PbStreamActor,
    pub actors: Vec<CustomActorInfo>,
}

#[derive(Default)]
pub struct CustomActorInfo {
    pub actor_id: u32,
    pub fragment_id: u32,
    pub dispatcher: Vec<Dispatcher>,
    pub upstream_actor_id: Vec<u32>,
    pub vnode_bitmap: Option<Buffer>,
}

impl From<&PbStreamActor> for CustomActorInfo {
    fn from(
        PbStreamActor {
            actor_id,
            fragment_id,
            dispatcher,
            upstream_actor_id,
            vnode_bitmap,
            ..
        }: &PbStreamActor,
    ) -> Self {
        CustomActorInfo {
            actor_id: *actor_id,
            fragment_id: *fragment_id,
            dispatcher: dispatcher.clone(),
            upstream_actor_id: upstream_actor_id.clone(),
            vnode_bitmap: vnode_bitmap.clone(),
        }
    }
}

impl From<&PbFragment> for CustomFragmentInfo {
    fn from(fragment: &PbFragment) -> Self {
        CustomFragmentInfo {
            fragment_id: fragment.fragment_id,
            fragment_type_mask: fragment.fragment_type_mask,
            distribution_type: fragment.distribution_type(),
            vnode_mapping: fragment.vnode_mapping.clone(),
            state_table_ids: fragment.state_table_ids.clone(),
            upstream_fragment_ids: fragment.upstream_fragment_ids.clone(),
            actor_template: fragment
                .actors
                .first()
                .cloned()
                .expect("no actor in fragment"),
            actors: fragment.actors.iter().map(CustomActorInfo::from).collect(),
        }
    }
}

impl CustomFragmentInfo {
    pub fn get_fragment_type_mask(&self) -> u32 {
        self.fragment_type_mask
    }

    pub fn distribution_type(&self) -> FragmentDistributionType {
        self.distribution_type
    }
}

pub struct RescheduleContext {
    /// Index used to map `ParallelUnitId` to `WorkerId`
    parallel_unit_id_to_worker_id: BTreeMap<ParallelUnitId, WorkerId>,
    /// Meta information for all Actors
    actor_map: HashMap<ActorId, CustomActorInfo>,
    /// Status of all Actors, used to find the location of the `Actor`
    actor_status: BTreeMap<ActorId, ActorStatus>,
    /// Meta information of all `Fragment`, used to find the `Fragment`'s `Actor`
    fragment_map: HashMap<FragmentId, CustomFragmentInfo>,
    /// Indexes for all `Worker`s
    worker_nodes: HashMap<WorkerId, WorkerNode>,
    /// Index of all `Actor` upstreams, specific to `Dispatcher`
    upstream_dispatchers: HashMap<ActorId, Vec<(FragmentId, DispatcherId, DispatcherType)>>,
    /// Fragments with stream source
    stream_source_fragment_ids: HashSet<FragmentId>,
    /// Target fragments in `NoShuffle` relation
    no_shuffle_target_fragment_ids: HashSet<FragmentId>,
    /// Source fragments in `NoShuffle` relation
    no_shuffle_source_fragment_ids: HashSet<FragmentId>,
    // index for dispatcher type from upstream fragment to downstream fragment
    fragment_dispatcher_map: HashMap<FragmentId, HashMap<FragmentId, DispatcherType>>,
}

impl RescheduleContext {
    fn actor_id_to_parallel_unit(&self, actor_id: &ActorId) -> MetaResult<&ParallelUnit> {
        self.actor_status
            .get(actor_id)
            .and_then(|actor_status| actor_status.parallel_unit.as_ref())
            .ok_or_else(|| anyhow!("could not found parallel unit for actor {}", actor_id).into())
    }

    fn parallel_unit_id_to_worker(
        &self,
        parallel_unit_id: &ParallelUnitId,
    ) -> MetaResult<&WorkerNode> {
        self.parallel_unit_id_to_worker_id
            .get(parallel_unit_id)
            .and_then(|worker_id| self.worker_nodes.get(worker_id))
            .ok_or_else(|| {
                anyhow!(
                    "could not found Worker for ParallelUint {}",
                    parallel_unit_id
                )
                .into()
            })
    }
}

/// This function provides an simple balancing method
/// The specific process is as follows
///
/// 1. Calculate the number of target actors, and calculate the average value and the remainder, and
/// use the average value as expected.
///
/// 2. Filter out the actor to be removed and the actor to be retained, and sort them from largest
/// to smallest (according to the number of virtual nodes held).
///
/// 3. Calculate their balance, 1) For the actors to be removed, the number of virtual nodes per
/// actor is the balance. 2) For retained actors, the number of virtual nodes - expected is the
/// balance. 3) For newly created actors, -expected is the balance (always negative).
///
/// 4. Allocate the remainder, high priority to newly created nodes.
///
/// 5. After that, merge removed, retained and created into a queue, with the head of the queue
/// being the source, and move the virtual nodes to the destination at the end of the queue.
///
/// This can handle scale in, scale out, migration, and simultaneous scaling with as much affinity
/// as possible.
///
/// Note that this function can only rebalance actors whose `vnode_bitmap` is not `None`, in other
/// words, for `Fragment` of `FragmentDistributionType::Single`, using this function will cause
/// assert to fail and should be skipped from the upper level.
///
/// The return value is the bitmap distribution after scaling, which covers all virtual node indexes
pub fn rebalance_actor_vnode(
    actors: &[CustomActorInfo],
    actors_to_remove: &BTreeSet<ActorId>,
    actors_to_create: &BTreeSet<ActorId>,
) -> HashMap<ActorId, Bitmap> {
    let actor_ids: BTreeSet<_> = actors.iter().map(|actor| actor.actor_id).collect();

    assert_eq!(actors_to_remove.difference(&actor_ids).count(), 0);
    assert_eq!(actors_to_create.intersection(&actor_ids).count(), 0);

    assert!(actors.len() >= actors_to_remove.len());

    let target_actor_count = actors.len() - actors_to_remove.len() + actors_to_create.len();
    assert!(target_actor_count > 0);

    // represents the balance of each actor, used to sort later
    #[derive(Debug)]
    struct Balance {
        actor_id: ActorId,
        balance: i32,
        builder: BitmapBuilder,
    }
    let (expected, mut remain) = VirtualNode::COUNT.div_rem(&target_actor_count);

    tracing::debug!(
        "expected {}, remain {}, prev actors {}, target actors {}",
        expected,
        remain,
        actors.len(),
        target_actor_count,
    );

    let (mut removed, mut rest): (Vec<_>, Vec<_>) = actors
        .iter()
        .filter_map(|actor| {
            actor
                .vnode_bitmap
                .as_ref()
                .map(|buffer| (actor.actor_id as ActorId, Bitmap::from(buffer)))
        })
        .partition(|(actor_id, _)| actors_to_remove.contains(actor_id));

    let order_by_bitmap_desc =
        |(_, bitmap_a): &(ActorId, Bitmap), (_, bitmap_b): &(ActorId, Bitmap)| -> Ordering {
            bitmap_a.count_ones().cmp(&bitmap_b.count_ones()).reverse()
        };

    let builder_from_bitmap = |bitmap: &Bitmap| -> BitmapBuilder {
        let mut builder = BitmapBuilder::default();
        builder.append_bitmap(bitmap);
        builder
    };

    let (prev_expected, _) = VirtualNode::COUNT.div_rem(&actors.len());

    let prev_remain = removed
        .iter()
        .map(|(_, bitmap)| {
            assert!(bitmap.count_ones() >= prev_expected);
            bitmap.count_ones() - prev_expected
        })
        .sum::<usize>();

    removed.sort_by(order_by_bitmap_desc);
    rest.sort_by(order_by_bitmap_desc);

    let removed_balances = removed.into_iter().map(|(actor_id, bitmap)| Balance {
        actor_id,
        balance: bitmap.count_ones() as i32,
        builder: builder_from_bitmap(&bitmap),
    });

    let mut rest_balances = rest
        .into_iter()
        .map(|(actor_id, bitmap)| Balance {
            actor_id,
            balance: bitmap.count_ones() as i32 - expected as i32,
            builder: builder_from_bitmap(&bitmap),
        })
        .collect_vec();

    let mut created_balances = actors_to_create
        .iter()
        .map(|actor_id| Balance {
            actor_id: *actor_id,
            balance: -(expected as i32),
            builder: BitmapBuilder::zeroed(VirtualNode::COUNT),
        })
        .collect_vec();

    for balance in created_balances
        .iter_mut()
        .rev()
        .take(prev_remain)
        .chain(rest_balances.iter_mut())
    {
        if remain > 0 {
            balance.balance -= 1;
            remain -= 1;
        }
    }

    // consume the rest `remain`
    for balance in &mut created_balances {
        if remain > 0 {
            balance.balance -= 1;
            remain -= 1;
        }
    }

    assert_eq!(remain, 0);

    let mut v: VecDeque<_> = removed_balances
        .chain(rest_balances)
        .chain(created_balances)
        .collect();

    // We will return the full bitmap here after rebalancing,
    // if we want to return only the changed actors, filter balance = 0 here
    let mut result = HashMap::with_capacity(target_actor_count);

    for balance in &v {
        tracing::debug!(
            "actor {:5}\tbalance {:5}\tR[{:5}]\tC[{:5}]",
            balance.actor_id,
            balance.balance,
            actors_to_remove.contains(&balance.actor_id),
            actors_to_create.contains(&balance.actor_id)
        );
    }

    while !v.is_empty() {
        if v.len() == 1 {
            let single = v.pop_front().unwrap();
            assert_eq!(single.balance, 0);
            if !actors_to_remove.contains(&single.actor_id) {
                result.insert(single.actor_id, single.builder.finish());
            }

            continue;
        }

        let mut src = v.pop_front().unwrap();
        let mut dst = v.pop_back().unwrap();

        let n = min(abs(src.balance), abs(dst.balance));

        let mut moved = 0;
        for idx in (0..VirtualNode::COUNT).rev() {
            if moved >= n {
                break;
            }

            if src.builder.is_set(idx) {
                src.builder.set(idx, false);
                assert!(!dst.builder.is_set(idx));
                dst.builder.set(idx, true);
                moved += 1;
            }
        }

        src.balance -= n;
        dst.balance += n;

        if src.balance != 0 {
            v.push_front(src);
        } else if !actors_to_remove.contains(&src.actor_id) {
            result.insert(src.actor_id, src.builder.finish());
        }

        if dst.balance != 0 {
            v.push_back(dst);
        } else {
            result.insert(dst.actor_id, dst.builder.finish());
        }
    }

    result
}

#[derive(Debug, Clone, Copy)]
pub struct RescheduleOptions {
    /// Whether to resolve the upstream of `NoShuffle` when scaling. It will check whether all the reschedules in the no shuffle dependency tree are corresponding, and rewrite them to the root of the no shuffle dependency tree.
    pub resolve_no_shuffle_upstream: bool,

    /// Whether to skip creating new actors. If it is true, the scaling-out actors will not be created.
    pub skip_create_new_actors: bool,
}

pub type ScaleControllerRef = Arc<ScaleController>;

pub struct ScaleController {
    pub metadata_manager: MetadataManager,

    pub source_manager: SourceManagerRef,

    pub stream_rpc_manager: StreamRpcManager,

    pub env: MetaSrvEnv,

    pub reschedule_lock: RwLock<()>,
}

impl ScaleController {
    pub fn new(
        metadata_manager: &MetadataManager,
        source_manager: SourceManagerRef,
        stream_rpc_manager: StreamRpcManager,
        env: MetaSrvEnv,
    ) -> Self {
        Self {
            stream_rpc_manager,
            metadata_manager: metadata_manager.clone(),
            source_manager,
            env,
            reschedule_lock: RwLock::new(()),
        }
    }

    /// Build the context for rescheduling and do some validation for the request.
    async fn build_reschedule_context(
        &self,
        reschedule: &mut HashMap<FragmentId, ParallelUnitReschedule>,
        options: RescheduleOptions,
        table_parallelisms: Option<&mut HashMap<TableId, TableParallelism>>,
    ) -> MetaResult<RescheduleContext> {
        let worker_nodes: HashMap<WorkerId, WorkerNode> = self
            .metadata_manager
            .list_active_streaming_compute_nodes()
            .await?
            .into_iter()
            .map(|worker_node| (worker_node.id, worker_node))
            .collect();

        if worker_nodes.is_empty() {
            bail!("no available compute node in the cluster");
        }

        // Check if we are trying to move a fragment to a node marked as unschedulable
        let unschedulable_parallel_unit_ids: HashMap<_, _> = worker_nodes
            .values()
            .filter(|w| {
                w.property
                    .as_ref()
                    .map(|property| property.is_unschedulable)
                    .unwrap_or(false)
            })
            .flat_map(|w| {
                w.parallel_units
                    .iter()
                    .map(|parallel_unit| (parallel_unit.id as ParallelUnitId, w.id as WorkerId))
            })
            .collect();

        for (fragment_id, reschedule) in &*reschedule {
            for parallel_unit_id in &reschedule.added_parallel_units {
                if let Some(worker_id) = unschedulable_parallel_unit_ids.get(parallel_unit_id) {
                    bail!(
                        "unable to move fragment {} to unschedulable parallel unit {} from worker {}",
                        fragment_id,
                        parallel_unit_id,
                        worker_id
                    );
                }
            }
        }

        // Associating ParallelUnit with Worker
        let parallel_unit_id_to_worker_id: BTreeMap<_, _> = worker_nodes
            .iter()
            .flat_map(|(worker_id, worker_node)| {
                worker_node
                    .parallel_units
                    .iter()
                    .map(move |parallel_unit| (parallel_unit.id as ParallelUnitId, *worker_id))
            })
            .collect();

        // FIXME: the same as anther place calling `list_table_fragments` in scaling.
        // Index for StreamActor
        let mut actor_map = HashMap::new();
        // Index for Fragment
        let mut fragment_map = HashMap::new();
        // Index for actor status, including actor's parallel unit
        let mut actor_status = BTreeMap::new();
        let mut fragment_state = HashMap::new();
        let mut fragment_to_table = HashMap::new();

        // We are reusing code for the metadata manager of both V1 and V2, which will be deprecated in the future.
        fn fulfill_index_by_table_fragments_ref(
            actor_map: &mut HashMap<u32, CustomActorInfo>,
            fragment_map: &mut HashMap<FragmentId, CustomFragmentInfo>,
            actor_status: &mut BTreeMap<ActorId, ActorStatus>,
            fragment_state: &mut HashMap<FragmentId, State>,
            fragment_to_table: &mut HashMap<FragmentId, TableId>,
            table_fragments: &TableFragments,
        ) {
            fragment_state.extend(
                table_fragments
                    .fragment_ids()
                    .map(|f| (f, table_fragments.state())),
            );

            for (fragment_id, fragment) in &table_fragments.fragments {
                for actor in &fragment.actors {
                    actor_map.insert(actor.actor_id, CustomActorInfo::from(actor));
                }

                fragment_map.insert(*fragment_id, CustomFragmentInfo::from(fragment));
            }

            actor_status.extend(table_fragments.actor_status.clone());

            fragment_to_table.extend(
                table_fragments
                    .fragment_ids()
                    .map(|f| (f, table_fragments.table_id())),
            );
        }

        match &self.metadata_manager {
            MetadataManager::V1(mgr) => {
                let guard = mgr.fragment_manager.get_fragment_read_guard().await;

                for table_fragments in guard.table_fragments().values() {
                    fulfill_index_by_table_fragments_ref(
                        &mut actor_map,
                        &mut fragment_map,
                        &mut actor_status,
                        &mut fragment_state,
                        &mut fragment_to_table,
                        table_fragments,
                    );
                }
            }
            MetadataManager::V2(_) => {
                let all_table_fragments = self.list_all_table_fragments().await?;

                for table_fragments in &all_table_fragments {
                    fulfill_index_by_table_fragments_ref(
                        &mut actor_map,
                        &mut fragment_map,
                        &mut actor_status,
                        &mut fragment_state,
                        &mut fragment_to_table,
                        table_fragments,
                    );
                }
            }
        };

        // NoShuffle relation index
        let mut no_shuffle_source_fragment_ids = HashSet::new();
        let mut no_shuffle_target_fragment_ids = HashSet::new();

        Self::build_no_shuffle_relation_index(
            &actor_map,
            &mut no_shuffle_source_fragment_ids,
            &mut no_shuffle_target_fragment_ids,
        );

        if options.resolve_no_shuffle_upstream {
            let original_reschedule_keys = reschedule.keys().cloned().collect();

            Self::resolve_no_shuffle_upstream_fragments(
                reschedule,
                &fragment_map,
                &no_shuffle_source_fragment_ids,
                &no_shuffle_target_fragment_ids,
            )?;

            if let Some(table_parallelisms) = table_parallelisms {
                // We need to reiterate through the NO_SHUFFLE dependencies in order to ascertain which downstream table the custom modifications of the table have been propagated from.
                Self::resolve_no_shuffle_upstream_tables(
                    original_reschedule_keys,
                    &fragment_map,
                    &no_shuffle_source_fragment_ids,
                    &no_shuffle_target_fragment_ids,
                    &fragment_to_table,
                    table_parallelisms,
                )?;
            }
        }

        let mut fragment_dispatcher_map = HashMap::new();
        Self::build_fragment_dispatcher_index(&actor_map, &mut fragment_dispatcher_map);

        // Then, we collect all available upstreams
        let mut upstream_dispatchers: HashMap<
            ActorId,
            Vec<(FragmentId, DispatcherId, DispatcherType)>,
        > = HashMap::new();
        for stream_actor in actor_map.values() {
            for dispatcher in &stream_actor.dispatcher {
                for downstream_actor_id in &dispatcher.downstream_actor_id {
                    upstream_dispatchers
                        .entry(*downstream_actor_id as ActorId)
                        .or_default()
                        .push((
                            stream_actor.fragment_id as FragmentId,
                            dispatcher.dispatcher_id as DispatcherId,
                            dispatcher.r#type(),
                        ));
                }
            }
        }

        let mut stream_source_fragment_ids = HashSet::new();
        let mut no_shuffle_reschedule = HashMap::new();
        for (
            fragment_id,
            ParallelUnitReschedule {
                added_parallel_units,
                removed_parallel_units,
            },
        ) in &*reschedule
        {
            let fragment = fragment_map
                .get(fragment_id)
                .ok_or_else(|| anyhow!("fragment {fragment_id} does not exist"))?;

            // Check if the reschedule is supported.
            match fragment_state[fragment_id] {
                table_fragments::State::Unspecified => unreachable!(),
                state @ table_fragments::State::Initial
                | state @ table_fragments::State::Creating => {
                    bail!(
                        "the materialized view of fragment {fragment_id} is in state {}",
                        state.as_str_name()
                    )
                }
                table_fragments::State::Created => {}
            }

            if no_shuffle_target_fragment_ids.contains(fragment_id) {
                bail!("rescheduling NoShuffle downstream fragment (maybe Chain fragment) is forbidden, please use NoShuffle upstream fragment (like Materialized fragment) to scale");
            }

            // For the relation of NoShuffle (e.g. Materialize and Chain), we need a special
            // treatment because the upstream and downstream of NoShuffle are always 1-1
            // correspondence, so we need to clone the reschedule plan to the downstream of all
            // cascading relations.
            if no_shuffle_source_fragment_ids.contains(fragment_id) {
                let mut queue: VecDeque<_> = fragment_dispatcher_map
                    .get(fragment_id)
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect();

                while let Some(downstream_id) = queue.pop_front() {
                    if !no_shuffle_target_fragment_ids.contains(&downstream_id) {
                        continue;
                    }

                    if let Some(downstream_fragments) = fragment_dispatcher_map.get(&downstream_id)
                    {
                        let no_shuffle_downstreams = downstream_fragments
                            .iter()
                            .filter(|(_, ty)| **ty == DispatcherType::NoShuffle)
                            .map(|(fragment_id, _)| fragment_id);

                        queue.extend(no_shuffle_downstreams.copied());
                    }

                    no_shuffle_reschedule.insert(
                        downstream_id,
                        ParallelUnitReschedule {
                            added_parallel_units: added_parallel_units.clone(),
                            removed_parallel_units: removed_parallel_units.clone(),
                        },
                    );
                }
            }

            if (fragment.get_fragment_type_mask() & FragmentTypeFlag::Source as u32) != 0 {
                let stream_node = fragment.actor_template.nodes.as_ref().unwrap();
                if stream_node.find_stream_source().is_some() {
                    stream_source_fragment_ids.insert(*fragment_id);
                }
            }

            // Check if the reschedule plan is valid.
            let current_parallel_units = fragment
                .actors
                .iter()
                .map(|a| {
                    actor_status
                        .get(&a.actor_id)
                        .unwrap()
                        .get_parallel_unit()
                        .unwrap()
                        .id
                })
                .collect::<HashSet<_>>();
            for removed in removed_parallel_units {
                if !current_parallel_units.contains(removed) {
                    bail!(
                        "no actor on the parallel unit {} of fragment {}",
                        removed,
                        fragment_id
                    );
                }
            }
            for added in added_parallel_units {
                if !parallel_unit_id_to_worker_id.contains_key(added) {
                    bail!("parallel unit {} not available", added);
                }
                if current_parallel_units.contains(added) && !removed_parallel_units.contains(added)
                {
                    bail!(
                        "parallel unit {} of fragment {} is already in use",
                        added,
                        fragment_id
                    );
                }
            }

            match fragment.distribution_type() {
                FragmentDistributionType::Hash => {
                    if current_parallel_units.len() + added_parallel_units.len()
                        <= removed_parallel_units.len()
                    {
                        bail!(
                            "can't remove all parallel units from fragment {}",
                            fragment_id
                        );
                    }
                }
                FragmentDistributionType::Single => {
                    if added_parallel_units.len() != removed_parallel_units.len() {
                        bail!("single distribution fragment only support migration");
                    }
                }
                FragmentDistributionType::Unspecified => unreachable!(),
            }
        }

        if !no_shuffle_reschedule.is_empty() {
            tracing::info!(
                "reschedule plan rewritten with NoShuffle reschedule {:?}",
                no_shuffle_reschedule
            );
        }

        // Modifications for NoShuffle downstream.
        reschedule.extend(no_shuffle_reschedule.into_iter());

        Ok(RescheduleContext {
            parallel_unit_id_to_worker_id,
            actor_map,
            actor_status,
            fragment_map,
            worker_nodes,
            upstream_dispatchers,
            stream_source_fragment_ids,
            no_shuffle_target_fragment_ids,
            no_shuffle_source_fragment_ids,
            fragment_dispatcher_map,
        })
    }

    async fn create_actors_on_compute_node(
        &self,
        worker_nodes: &HashMap<WorkerId, WorkerNode>,
        actor_infos_to_broadcast: BTreeMap<ActorId, ActorInfo>,
        node_actors_to_create: HashMap<WorkerId, Vec<BuildActorInfo>>,
        broadcast_worker_ids: HashSet<WorkerId>,
    ) -> MetaResult<()> {
        self.stream_rpc_manager
            .broadcast_update_actor_info(
                worker_nodes,
                broadcast_worker_ids.into_iter(),
                actor_infos_to_broadcast.values().cloned(),
                node_actors_to_create.clone().into_iter(),
            )
            .await?;

        self.stream_rpc_manager
            .build_actors(
                worker_nodes,
                node_actors_to_create
                    .iter()
                    .map(|(node_id, stream_actors)| {
                        (
                            *node_id,
                            stream_actors
                                .iter()
                                .map(|stream_actor| stream_actor.actor.as_ref().unwrap().actor_id)
                                .collect_vec(),
                        )
                    }),
            )
            .await?;

        Ok(())
    }

    // Results are the generated reschedule plan and the changes that need to be updated to the meta store.
    pub(crate) async fn prepare_reschedule_command(
        &self,
        mut reschedules: HashMap<FragmentId, ParallelUnitReschedule>,
        options: RescheduleOptions,
        table_parallelisms: Option<&mut HashMap<TableId, TableParallelism>>,
    ) -> MetaResult<(
        HashMap<FragmentId, Reschedule>,
        HashMap<FragmentId, HashSet<ActorId>>,
    )> {
        let ctx = self
            .build_reschedule_context(&mut reschedules, options, table_parallelisms)
            .await?;
        // Index of actors to create/remove
        // Fragment Id => ( Actor Id => Parallel Unit Id )

        let (fragment_actors_to_remove, fragment_actors_to_create) =
            self.arrange_reschedules(&reschedules, &ctx).await?;

        let mut fragment_actor_bitmap = HashMap::new();
        for fragment_id in reschedules.keys() {
            if ctx.no_shuffle_target_fragment_ids.contains(fragment_id) {
                // skipping chain fragment, we need to clone the upstream materialize fragment's
                // mapping later
                continue;
            }

            let actors_to_create = fragment_actors_to_create
                .get(fragment_id)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();

            let actors_to_remove = fragment_actors_to_remove
                .get(fragment_id)
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();

            let fragment = ctx.fragment_map.get(fragment_id).unwrap();

            match fragment.distribution_type() {
                FragmentDistributionType::Single => {
                    // Skip rebalance action for single distribution (always None)
                    fragment_actor_bitmap
                        .insert(fragment.fragment_id as FragmentId, Default::default());
                }
                FragmentDistributionType::Hash => {
                    let actor_vnode = rebalance_actor_vnode(
                        &fragment.actors,
                        &actors_to_remove,
                        &actors_to_create,
                    );

                    fragment_actor_bitmap.insert(fragment.fragment_id as FragmentId, actor_vnode);
                }

                FragmentDistributionType::Unspecified => unreachable!(),
            }
        }

        // Index for fragment -> { actor -> parallel_unit } after reschedule.
        // Since we need to organize the upstream and downstream relationships of NoShuffle,
        // we need to organize the actor distribution after a scaling.
        let mut fragment_actors_after_reschedule = HashMap::with_capacity(reschedules.len());
        for fragment_id in reschedules.keys() {
            let fragment = ctx.fragment_map.get(fragment_id).unwrap();
            let mut new_actor_ids = BTreeMap::new();
            for actor in &fragment.actors {
                if let Some(actors_to_remove) = fragment_actors_to_remove.get(fragment_id) {
                    if actors_to_remove.contains_key(&actor.actor_id) {
                        continue;
                    }
                }
                let parallel_unit_id = ctx.actor_id_to_parallel_unit(&actor.actor_id)?.id;
                new_actor_ids.insert(
                    actor.actor_id as ActorId,
                    parallel_unit_id as ParallelUnitId,
                );
            }

            if let Some(actors_to_create) = fragment_actors_to_create.get(fragment_id) {
                for (actor_id, parallel_unit_id) in actors_to_create {
                    new_actor_ids.insert(*actor_id, *parallel_unit_id as ParallelUnitId);
                }
            }

            assert!(
                !new_actor_ids.is_empty(),
                "should be at least one actor in fragment {} after rescheduling",
                fragment_id
            );

            fragment_actors_after_reschedule.insert(*fragment_id, new_actor_ids);
        }

        let fragment_actors_after_reschedule = fragment_actors_after_reschedule;

        // In order to maintain consistency with the original structure, the upstream and downstream
        // actors of NoShuffle need to be in the same parallel unit and hold the same virtual nodes,
        // so for the actors after the upstream rebalancing, we need to find the parallel
        // unit corresponding to each actor, and find the downstream actor corresponding to
        // the parallel unit, and then copy the Bitmap to the corresponding actor. At the
        // same time, we need to sort out the relationship between upstream and downstream
        // actors
        fn arrange_no_shuffle_relation(
            ctx: &RescheduleContext,
            fragment_id: &FragmentId,
            upstream_fragment_id: &FragmentId,
            fragment_actors_after_reschedule: &HashMap<
                FragmentId,
                BTreeMap<ActorId, ParallelUnitId>,
            >,
            fragment_updated_bitmap: &mut HashMap<FragmentId, HashMap<ActorId, Bitmap>>,
            no_shuffle_upstream_actor_map: &mut HashMap<ActorId, HashMap<FragmentId, ActorId>>,
            no_shuffle_downstream_actors_map: &mut HashMap<ActorId, HashMap<FragmentId, ActorId>>,
        ) {
            if !ctx.no_shuffle_target_fragment_ids.contains(fragment_id) {
                return;
            }

            let fragment = ctx.fragment_map.get(fragment_id).unwrap();

            // If the upstream is a Singleton Fragment, there will be no Bitmap changes
            let mut upstream_fragment_bitmap = fragment_updated_bitmap
                .get(upstream_fragment_id)
                .cloned()
                .unwrap_or_default();

            let upstream_fragment_actor_map = fragment_actors_after_reschedule
                .get(upstream_fragment_id)
                .cloned()
                .unwrap();

            let mut parallel_unit_id_to_actor_id = HashMap::new();
            for (actor_id, parallel_unit_id) in
                fragment_actors_after_reschedule.get(fragment_id).unwrap()
            {
                parallel_unit_id_to_actor_id.insert(*parallel_unit_id, *actor_id);
            }

            let mut fragment_bitmap = HashMap::new();
            for (upstream_actor_id, parallel_unit_id) in upstream_fragment_actor_map {
                let actor_id = parallel_unit_id_to_actor_id.get(&parallel_unit_id).unwrap();

                if let Some(bitmap) = upstream_fragment_bitmap.remove(&upstream_actor_id) {
                    // Copy the bitmap
                    fragment_bitmap.insert(*actor_id, bitmap);
                }

                no_shuffle_upstream_actor_map
                    .entry(*actor_id as ActorId)
                    .or_default()
                    .insert(*upstream_fragment_id, upstream_actor_id);
                no_shuffle_downstream_actors_map
                    .entry(upstream_actor_id)
                    .or_default()
                    .insert(*fragment_id, *actor_id);
            }

            match fragment.distribution_type() {
                FragmentDistributionType::Hash => {}
                FragmentDistributionType::Single => {
                    // single distribution should update nothing
                    assert!(fragment_bitmap.is_empty());
                }
                FragmentDistributionType::Unspecified => unreachable!(),
            }

            if let Err(e) = fragment_updated_bitmap.try_insert(*fragment_id, fragment_bitmap) {
                assert_eq!(
                    e.entry.get(),
                    &e.value,
                    "bitmaps derived from different no-shuffle upstreams mismatch"
                );
            }

            // Visit downstream fragments recursively.
            if let Some(downstream_fragments) = ctx.fragment_dispatcher_map.get(fragment_id) {
                let no_shuffle_downstreams = downstream_fragments
                    .iter()
                    .filter(|(_, ty)| **ty == DispatcherType::NoShuffle)
                    .map(|(fragment_id, _)| fragment_id);

                for downstream_fragment_id in no_shuffle_downstreams {
                    arrange_no_shuffle_relation(
                        ctx,
                        downstream_fragment_id,
                        fragment_id,
                        fragment_actors_after_reschedule,
                        fragment_updated_bitmap,
                        no_shuffle_upstream_actor_map,
                        no_shuffle_downstream_actors_map,
                    );
                }
            }
        }

        let mut no_shuffle_upstream_actor_map = HashMap::new();
        let mut no_shuffle_downstream_actors_map = HashMap::new();
        // For all roots in the upstream and downstream dependency trees of NoShuffle, recursively
        // find all correspondences
        for fragment_id in reschedules.keys() {
            if ctx.no_shuffle_source_fragment_ids.contains(fragment_id)
                && !ctx.no_shuffle_target_fragment_ids.contains(fragment_id)
            {
                if let Some(downstream_fragments) = ctx.fragment_dispatcher_map.get(fragment_id) {
                    for downstream_fragment_id in downstream_fragments.keys() {
                        arrange_no_shuffle_relation(
                            &ctx,
                            downstream_fragment_id,
                            fragment_id,
                            &fragment_actors_after_reschedule,
                            &mut fragment_actor_bitmap,
                            &mut no_shuffle_upstream_actor_map,
                            &mut no_shuffle_downstream_actors_map,
                        );
                    }
                }
            }
        }

        let mut new_created_actors = HashMap::new();
        for fragment_id in reschedules.keys() {
            let actors_to_create = fragment_actors_to_create
                .get(fragment_id)
                .cloned()
                .unwrap_or_default();

            let fragment = ctx.fragment_map.get(fragment_id).unwrap();

            assert!(!fragment.actors.is_empty());

            for (actor_to_create, sample_actor) in actors_to_create
                .iter()
                .zip_eq_debug(repeat(&fragment.actor_template).take(actors_to_create.len()))
            {
                let new_actor_id = actor_to_create.0;
                let mut new_actor = sample_actor.clone();

                // This should be assigned before the `modify_actor_upstream_and_downstream` call,
                // because we need to use the new actor id to find the upstream and
                // downstream in the NoShuffle relationship
                new_actor.actor_id = *new_actor_id;

                Self::modify_actor_upstream_and_downstream(
                    &ctx,
                    &fragment_actors_to_remove,
                    &fragment_actors_to_create,
                    &fragment_actor_bitmap,
                    &no_shuffle_upstream_actor_map,
                    &no_shuffle_downstream_actors_map,
                    &mut new_actor,
                )?;

                if let Some(bitmap) = fragment_actor_bitmap
                    .get(fragment_id)
                    .and_then(|actor_bitmaps| actor_bitmaps.get(new_actor_id))
                {
                    new_actor.vnode_bitmap = Some(bitmap.to_protobuf());
                }

                new_created_actors.insert(*new_actor_id, new_actor);
            }
        }

        if !options.skip_create_new_actors {
            // After modification, for newly created actors, both upstream and downstream actor ids
            // have been modified
            let mut actor_infos_to_broadcast = BTreeMap::new();
            let mut node_actors_to_create: HashMap<WorkerId, Vec<BuildActorInfo>> = HashMap::new();
            let mut broadcast_worker_ids = HashSet::new();

            let subscriptions: HashMap<_, SubscriptionIds> = self
                .metadata_manager
                .get_mv_depended_subscriptions()
                .await?
                .iter()
                .map(|(table_id, subscriptions)| {
                    (
                        table_id.table_id,
                        SubscriptionIds {
                            subscription_ids: subscriptions.keys().cloned().collect(),
                        },
                    )
                })
                .collect();

            for actors_to_create in fragment_actors_to_create.values() {
                for (new_actor_id, new_parallel_unit_id) in actors_to_create {
                    let new_actor = new_created_actors.get(new_actor_id).unwrap();
                    for upstream_actor_id in &new_actor.upstream_actor_id {
                        if new_created_actors.contains_key(upstream_actor_id) {
                            continue;
                        }

                        let upstream_worker_id = ctx
                            .actor_id_to_parallel_unit(upstream_actor_id)?
                            .worker_node_id;
                        let upstream_worker =
                            ctx.worker_nodes.get(&upstream_worker_id).with_context(|| {
                                format!("upstream worker {} not found", upstream_worker_id)
                            })?;

                        // Force broadcast upstream actor info, because the actor information of the new
                        // node may not have been synchronized yet
                        actor_infos_to_broadcast.insert(
                            *upstream_actor_id,
                            ActorInfo {
                                actor_id: *upstream_actor_id,
                                host: upstream_worker.host.clone(),
                            },
                        );

                        broadcast_worker_ids.insert(upstream_worker_id);
                    }

                    for dispatcher in &new_actor.dispatcher {
                        for downstream_actor_id in &dispatcher.downstream_actor_id {
                            if new_created_actors.contains_key(downstream_actor_id) {
                                continue;
                            }
                            let downstream_worker_id = ctx
                                .actor_id_to_parallel_unit(downstream_actor_id)?
                                .worker_node_id;
                            let downstream_worker = ctx
                                .worker_nodes
                                .get(&downstream_worker_id)
                                .with_context(|| {
                                    format!("downstream worker {} not found", downstream_worker_id)
                                })?;

                            actor_infos_to_broadcast.insert(
                                *downstream_actor_id,
                                ActorInfo {
                                    actor_id: *downstream_actor_id,
                                    host: downstream_worker.host.clone(),
                                },
                            );

                            broadcast_worker_ids.insert(downstream_worker_id);
                        }
                    }

                    let worker = ctx.parallel_unit_id_to_worker(new_parallel_unit_id)?;

                    node_actors_to_create
                        .entry(worker.id)
                        .or_default()
                        .push(BuildActorInfo {
                            actor: Some(new_actor.clone()),
                            // TODO: may include only the subscriptions related to the table fragment
                            // of the actor.
                            related_subscriptions: subscriptions.clone(),
                        });

                    broadcast_worker_ids.insert(worker.id);

                    actor_infos_to_broadcast.insert(
                        *new_actor_id,
                        ActorInfo {
                            actor_id: *new_actor_id,
                            host: worker.host.clone(),
                        },
                    );
                }
            }

            self.create_actors_on_compute_node(
                &ctx.worker_nodes,
                actor_infos_to_broadcast,
                node_actors_to_create,
                broadcast_worker_ids,
            )
            .await?;
        }

        // For stream source fragments, we need to reallocate the splits.
        // Because we are in the Pause state, so it's no problem to reallocate
        let mut fragment_stream_source_actor_splits = HashMap::new();
        for fragment_id in reschedules.keys() {
            let actors_after_reschedule =
                fragment_actors_after_reschedule.get(fragment_id).unwrap();

            if ctx.stream_source_fragment_ids.contains(fragment_id) {
                let fragment = ctx.fragment_map.get(fragment_id).unwrap();

                let prev_actor_ids = fragment
                    .actors
                    .iter()
                    .map(|actor| actor.actor_id)
                    .collect_vec();

                let curr_actor_ids = actors_after_reschedule.keys().cloned().collect_vec();

                let actor_splits = self
                    .source_manager
                    .migrate_splits(*fragment_id, &prev_actor_ids, &curr_actor_ids)
                    .await?;

                fragment_stream_source_actor_splits.insert(*fragment_id, actor_splits);
            }
        }
        // TODO: support migrate splits for SourceBackfill

        // Generate fragment reschedule plan
        let mut reschedule_fragment: HashMap<FragmentId, Reschedule> =
            HashMap::with_capacity(reschedules.len());

        for (fragment_id, _) in reschedules {
            let mut actors_to_create: HashMap<_, Vec<_>> = HashMap::new();
            let fragment_type_mask = ctx
                .fragment_map
                .get(&fragment_id)
                .unwrap()
                .fragment_type_mask;
            let injectable = TableFragments::is_injectable(fragment_type_mask);

            if let Some(actor_pu_maps) = fragment_actors_to_create.get(&fragment_id).cloned() {
                for (actor_id, parallel_unit_id) in actor_pu_maps {
                    let worker_id = ctx
                        .parallel_unit_id_to_worker_id
                        .get(&parallel_unit_id)
                        .with_context(|| format!("parallel unit {} not found", parallel_unit_id))?;
                    actors_to_create
                        .entry(*worker_id)
                        .or_default()
                        .push(actor_id);
                }
            }

            let actors_to_remove = fragment_actors_to_remove
                .get(&fragment_id)
                .cloned()
                .unwrap_or_default()
                .into_keys()
                .collect();

            let actors_after_reschedule =
                fragment_actors_after_reschedule.get(&fragment_id).unwrap();

            let parallel_unit_to_actor_after_reschedule: BTreeMap<_, _> = actors_after_reschedule
                .iter()
                .map(|(actor_id, parallel_unit_id)| {
                    (*parallel_unit_id as ParallelUnitId, *actor_id as ActorId)
                })
                .collect();

            assert!(!parallel_unit_to_actor_after_reschedule.is_empty());

            let fragment = ctx.fragment_map.get(&fragment_id).unwrap();

            let in_degree_types: HashSet<_> = fragment
                .upstream_fragment_ids
                .iter()
                .flat_map(|upstream_fragment_id| {
                    ctx.fragment_dispatcher_map
                        .get(upstream_fragment_id)
                        .and_then(|dispatcher_map| {
                            dispatcher_map.get(&fragment.fragment_id).cloned()
                        })
                })
                .collect();

            let upstream_dispatcher_mapping = match fragment.distribution_type() {
                FragmentDistributionType::Hash => {
                    if !in_degree_types.contains(&DispatcherType::Hash) {
                        None
                    } else if parallel_unit_to_actor_after_reschedule.len() == 1 {
                        let actor_id = parallel_unit_to_actor_after_reschedule
                            .into_values()
                            .next()
                            .unwrap();
                        Some(ActorMapping::new_single(actor_id))
                    } else {
                        // Changes of the bitmap must occur in the case of HashDistribution
                        Some(ActorMapping::from_bitmaps(
                            &fragment_actor_bitmap[&fragment_id],
                        ))
                    }
                }

                FragmentDistributionType::Single => {
                    assert!(fragment_actor_bitmap.get(&fragment_id).unwrap().is_empty());
                    None
                }
                FragmentDistributionType::Unspecified => unreachable!(),
            };

            let mut upstream_fragment_dispatcher_set = BTreeSet::new();

            for actor in &fragment.actors {
                if let Some(upstream_actor_tuples) = ctx.upstream_dispatchers.get(&actor.actor_id) {
                    for (upstream_fragment_id, upstream_dispatcher_id, upstream_dispatcher_type) in
                        upstream_actor_tuples
                    {
                        match upstream_dispatcher_type {
                            DispatcherType::Unspecified => unreachable!(),
                            DispatcherType::NoShuffle => {}
                            _ => {
                                upstream_fragment_dispatcher_set
                                    .insert((*upstream_fragment_id, *upstream_dispatcher_id));
                            }
                        }
                    }
                }
            }

            let downstream_fragment_ids = if let Some(downstream_fragments) =
                ctx.fragment_dispatcher_map.get(&fragment_id)
            {
                // Skip fragments' no-shuffle downstream, as there's no need to update the merger
                // (receiver) of a no-shuffle downstream
                downstream_fragments
                    .iter()
                    .filter(|(_, dispatcher_type)| *dispatcher_type != &DispatcherType::NoShuffle)
                    .map(|(fragment_id, _)| *fragment_id)
                    .collect_vec()
            } else {
                vec![]
            };

            let vnode_bitmap_updates = match fragment.distribution_type() {
                FragmentDistributionType::Hash => {
                    let mut vnode_bitmap_updates =
                        fragment_actor_bitmap.remove(&fragment_id).unwrap();

                    // We need to keep the bitmaps from changed actors only,
                    // otherwise the barrier will become very large with many actors
                    for actor_id in actors_after_reschedule.keys() {
                        assert!(vnode_bitmap_updates.contains_key(actor_id));

                        // retain actor
                        if let Some(actor) = ctx.actor_map.get(actor_id) {
                            let bitmap = vnode_bitmap_updates.get(actor_id).unwrap();

                            if let Some(buffer) = actor.vnode_bitmap.as_ref() {
                                let prev_bitmap = Bitmap::from(buffer);

                                if prev_bitmap.eq(bitmap) {
                                    vnode_bitmap_updates.remove(actor_id);
                                }
                            }
                        }
                    }

                    vnode_bitmap_updates
                }
                FragmentDistributionType::Single => HashMap::new(),
                FragmentDistributionType::Unspecified => unreachable!(),
            };

            let upstream_fragment_dispatcher_ids =
                upstream_fragment_dispatcher_set.into_iter().collect_vec();

            let actor_splits = fragment_stream_source_actor_splits
                .get(&fragment_id)
                .cloned()
                .unwrap_or_default();

            reschedule_fragment.insert(
                fragment_id,
                Reschedule {
                    added_actors: actors_to_create,
                    removed_actors: actors_to_remove,
                    vnode_bitmap_updates,
                    upstream_fragment_dispatcher_ids,
                    upstream_dispatcher_mapping,
                    downstream_fragment_ids,
                    actor_splits,
                    injectable,
                    newly_created_actors: vec![],
                },
            );
        }

        let mut fragment_created_actors = HashMap::new();
        for (fragment_id, actors_to_create) in &fragment_actors_to_create {
            let mut created_actors = HashMap::new();
            for (actor_id, parallel_unit_id) in actors_to_create {
                let actor = new_created_actors.get(actor_id).cloned().unwrap();
                let worker_id = ctx
                    .parallel_unit_id_to_worker_id
                    .get(parallel_unit_id)
                    .with_context(|| format!("parallel unit {} not found", parallel_unit_id))?;

                created_actors.insert(
                    *actor_id,
                    (
                        actor,
                        ActorStatus {
                            parallel_unit: Some(ParallelUnit {
                                id: *parallel_unit_id,
                                worker_node_id: *worker_id,
                            }),
                            state: ActorState::Inactive as i32,
                        },
                    ),
                );
            }

            fragment_created_actors.insert(*fragment_id, created_actors);
        }

        for (fragment_id, to_create) in &fragment_created_actors {
            let reschedule = reschedule_fragment.get_mut(fragment_id).unwrap();
            reschedule.newly_created_actors = to_create.values().cloned().collect();
        }

        let applied_reschedules = self
            .metadata_manager
            .pre_apply_reschedules(fragment_created_actors)
            .await;

        Ok((reschedule_fragment, applied_reschedules))
    }

    async fn arrange_reschedules(
        &self,
        reschedule: &HashMap<FragmentId, ParallelUnitReschedule>,
        ctx: &RescheduleContext,
    ) -> MetaResult<(
        HashMap<FragmentId, BTreeMap<ActorId, ParallelUnitId>>,
        HashMap<FragmentId, BTreeMap<ActorId, ParallelUnitId>>,
    )> {
        let mut fragment_actors_to_remove = HashMap::with_capacity(reschedule.len());
        let mut fragment_actors_to_create = HashMap::with_capacity(reschedule.len());

        for (
            fragment_id,
            ParallelUnitReschedule {
                added_parallel_units,
                removed_parallel_units,
            },
        ) in reschedule
        {
            let fragment = ctx.fragment_map.get(fragment_id).unwrap();

            // Actor Id => Parallel Unit Id
            let mut actors_to_remove = BTreeMap::new();
            let mut actors_to_create = BTreeMap::new();

            let parallel_unit_to_actor: HashMap<_, _> = fragment
                .actors
                .iter()
                .map(|actor| {
                    ctx.actor_id_to_parallel_unit(&actor.actor_id)
                        .map(|parallel_unit| {
                            (
                                parallel_unit.id as ParallelUnitId,
                                actor.actor_id as ActorId,
                            )
                        })
                })
                .try_collect()?;

            for removed_parallel_unit_id in removed_parallel_units {
                if let Some(removed_actor_id) = parallel_unit_to_actor.get(removed_parallel_unit_id)
                {
                    actors_to_remove.insert(*removed_actor_id, *removed_parallel_unit_id);
                }
            }

            for created_parallel_unit_id in added_parallel_units {
                let id = match self.env.id_gen_manager() {
                    IdGenManagerImpl::Kv(mgr) => {
                        mgr.generate::<{ IdCategory::Actor }>().await? as ActorId
                    }
                    IdGenManagerImpl::Sql(mgr) => {
                        let id = mgr.generate_interval::<{ IdCategory::Actor }>(1);
                        id as ActorId
                    }
                };

                actors_to_create.insert(id, *created_parallel_unit_id);
            }

            if !actors_to_remove.is_empty() {
                fragment_actors_to_remove.insert(*fragment_id as FragmentId, actors_to_remove);
            }

            if !actors_to_create.is_empty() {
                fragment_actors_to_create.insert(*fragment_id as FragmentId, actors_to_create);
            }
        }

        Ok((fragment_actors_to_remove, fragment_actors_to_create))
    }

    /// Modifies the upstream and downstream actors of the new created actor according to the
    /// overall changes, and is used to handle cascading updates
    fn modify_actor_upstream_and_downstream(
        ctx: &RescheduleContext,
        fragment_actors_to_remove: &HashMap<FragmentId, BTreeMap<ActorId, ParallelUnitId>>,
        fragment_actors_to_create: &HashMap<FragmentId, BTreeMap<ActorId, ParallelUnitId>>,
        fragment_actor_bitmap: &HashMap<FragmentId, HashMap<ActorId, Bitmap>>,
        no_shuffle_upstream_actor_map: &HashMap<ActorId, HashMap<FragmentId, ActorId>>,
        no_shuffle_downstream_actors_map: &HashMap<ActorId, HashMap<FragmentId, ActorId>>,
        new_actor: &mut PbStreamActor,
    ) -> MetaResult<()> {
        let fragment = &ctx.fragment_map.get(&new_actor.fragment_id).unwrap();
        let mut applied_upstream_fragment_actor_ids = HashMap::new();

        for upstream_fragment_id in &fragment.upstream_fragment_ids {
            let upstream_dispatch_type = &ctx
                .fragment_dispatcher_map
                .get(upstream_fragment_id)
                .and_then(|map| map.get(&fragment.fragment_id))
                .unwrap();

            match upstream_dispatch_type {
                DispatcherType::Unspecified => unreachable!(),
                DispatcherType::Hash | DispatcherType::Broadcast | DispatcherType::Simple => {
                    let upstream_fragment = &ctx.fragment_map.get(upstream_fragment_id).unwrap();
                    let mut upstream_actor_ids = upstream_fragment
                        .actors
                        .iter()
                        .map(|actor| actor.actor_id as ActorId)
                        .collect_vec();

                    if let Some(upstream_actors_to_remove) =
                        fragment_actors_to_remove.get(upstream_fragment_id)
                    {
                        upstream_actor_ids
                            .retain(|actor_id| !upstream_actors_to_remove.contains_key(actor_id));
                    }

                    if let Some(upstream_actors_to_create) =
                        fragment_actors_to_create.get(upstream_fragment_id)
                    {
                        upstream_actor_ids.extend(upstream_actors_to_create.keys().cloned());
                    }

                    applied_upstream_fragment_actor_ids.insert(
                        *upstream_fragment_id as FragmentId,
                        upstream_actor_ids.clone(),
                    );
                }
                DispatcherType::NoShuffle => {
                    let no_shuffle_upstream_actor_id = *no_shuffle_upstream_actor_map
                        .get(&new_actor.actor_id)
                        .and_then(|map| map.get(upstream_fragment_id))
                        .unwrap();

                    applied_upstream_fragment_actor_ids.insert(
                        *upstream_fragment_id as FragmentId,
                        vec![no_shuffle_upstream_actor_id as ActorId],
                    );
                }
            }
        }

        new_actor.upstream_actor_id = applied_upstream_fragment_actor_ids
            .values()
            .flatten()
            .cloned()
            .collect_vec();

        fn replace_merge_node_upstream(
            stream_node: &mut StreamNode,
            applied_upstream_fragment_actor_ids: &HashMap<FragmentId, Vec<ActorId>>,
        ) {
            if let Some(NodeBody::Merge(s)) = stream_node.node_body.as_mut() {
                s.upstream_actor_id = applied_upstream_fragment_actor_ids
                    .get(&s.upstream_fragment_id)
                    .cloned()
                    .unwrap();
            }

            for child in &mut stream_node.input {
                replace_merge_node_upstream(child, applied_upstream_fragment_actor_ids);
            }
        }

        if let Some(node) = new_actor.nodes.as_mut() {
            replace_merge_node_upstream(node, &applied_upstream_fragment_actor_ids);
        }

        // Update downstream actor ids
        for dispatcher in &mut new_actor.dispatcher {
            let downstream_fragment_id = dispatcher
                .downstream_actor_id
                .iter()
                .filter_map(|actor_id| ctx.actor_map.get(actor_id).map(|actor| actor.fragment_id))
                .dedup()
                .exactly_one()
                .unwrap() as FragmentId;

            let downstream_fragment_actors_to_remove =
                fragment_actors_to_remove.get(&downstream_fragment_id);
            let downstream_fragment_actors_to_create =
                fragment_actors_to_create.get(&downstream_fragment_id);

            match dispatcher.r#type() {
                d @ (DispatcherType::Hash | DispatcherType::Simple | DispatcherType::Broadcast) => {
                    if let Some(downstream_actors_to_remove) = downstream_fragment_actors_to_remove
                    {
                        dispatcher
                            .downstream_actor_id
                            .retain(|id| !downstream_actors_to_remove.contains_key(id));
                    }

                    if let Some(downstream_actors_to_create) = downstream_fragment_actors_to_create
                    {
                        dispatcher
                            .downstream_actor_id
                            .extend(downstream_actors_to_create.keys().cloned())
                    }

                    // There should be still exactly one downstream actor
                    if d == DispatcherType::Simple {
                        assert_eq!(dispatcher.downstream_actor_id.len(), 1);
                    }
                }
                DispatcherType::NoShuffle => {
                    assert_eq!(dispatcher.downstream_actor_id.len(), 1);
                    let downstream_actor_id = no_shuffle_downstream_actors_map
                        .get(&new_actor.actor_id)
                        .and_then(|map| map.get(&downstream_fragment_id))
                        .unwrap();
                    dispatcher.downstream_actor_id = vec![*downstream_actor_id as ActorId];
                }
                DispatcherType::Unspecified => unreachable!(),
            }

            if let Some(mapping) = dispatcher.hash_mapping.as_mut() {
                if let Some(downstream_updated_bitmap) =
                    fragment_actor_bitmap.get(&downstream_fragment_id)
                {
                    // If downstream scale in/out
                    *mapping = ActorMapping::from_bitmaps(downstream_updated_bitmap).to_protobuf();
                }
            }
        }

        Ok(())
    }

    pub async fn post_apply_reschedule(
        &self,
        reschedules: &HashMap<FragmentId, Reschedule>,
        table_parallelism: &HashMap<TableId, TableParallelism>,
    ) -> MetaResult<()> {
        // Update fragment info after rescheduling in meta store.
        self.metadata_manager
            .post_apply_reschedules(reschedules.clone(), table_parallelism.clone())
            .await?;

        // Update serving fragment info after rescheduling in meta store.
        if !reschedules.is_empty() {
            let workers = self
                .metadata_manager
                .list_active_streaming_compute_nodes()
                .await?;
            let streaming_parallelisms = self
                .metadata_manager
                .running_fragment_parallelisms(Some(reschedules.keys().cloned().collect()))
                .await?;
            let serving_vnode_mapping = Arc::new(ServingVnodeMapping::default());
            let (upserted, failed) = serving_vnode_mapping.upsert(streaming_parallelisms, &workers);
            if !upserted.is_empty() {
                tracing::debug!(
                    "Update serving vnode mapping for fragments {:?}.",
                    upserted.keys()
                );
                self.env
                    .notification_manager()
                    .notify_frontend_without_version(
                        Operation::Update,
                        Info::ServingParallelUnitMappings(FragmentParallelUnitMappings {
                            mappings: to_fragment_parallel_unit_mapping(&upserted),
                        }),
                    );
            }
            if !failed.is_empty() {
                tracing::debug!(
                    "Fail to update serving vnode mapping for fragments {:?}.",
                    failed
                );
                self.env
                    .notification_manager()
                    .notify_frontend_without_version(
                        Operation::Delete,
                        Info::ServingParallelUnitMappings(FragmentParallelUnitMappings {
                            mappings: to_deleted_fragment_parallel_unit_mapping(&failed),
                        }),
                    );
            }
        }

        let mut stream_source_actor_splits = HashMap::new();
        let mut stream_source_dropped_actors = HashSet::new();

        for (fragment_id, reschedule) in reschedules {
            if !reschedule.actor_splits.is_empty() {
                stream_source_actor_splits
                    .insert(*fragment_id as FragmentId, reschedule.actor_splits.clone());
                stream_source_dropped_actors.extend(reschedule.removed_actors.clone());
            }
        }

        if !stream_source_actor_splits.is_empty() {
            self.source_manager
                .apply_source_change(
                    None,
                    None,
                    Some(stream_source_actor_splits),
                    Some(stream_source_dropped_actors),
                )
                .await;
        }

        Ok(())
    }

    // FIXME: should be removed
    async fn list_all_table_fragments(&self) -> MetaResult<Vec<model::TableFragments>> {
        use crate::model::MetadataModel;
        let all_table_fragments = match &self.metadata_manager {
            MetadataManager::V1(mgr) => mgr.fragment_manager.list_table_fragments().await,
            MetadataManager::V2(mgr) => mgr
                .catalog_controller
                .table_fragments()
                .await?
                .into_values()
                .map(model::TableFragments::from_protobuf)
                .collect(),
        };

        Ok(all_table_fragments)
    }

    pub async fn generate_table_resize_plan(
        &self,
        policy: TableResizePolicy,
    ) -> MetaResult<HashMap<FragmentId, ParallelUnitReschedule>> {
        let TableResizePolicy {
            worker_ids,
            table_parallelisms,
        } = policy;

        let workers = self
            .metadata_manager
            .list_active_streaming_compute_nodes()
            .await?;

        let unschedulable_worker_ids = Self::filter_unschedulable_workers(&workers);

        for worker_id in &worker_ids {
            if unschedulable_worker_ids.contains(worker_id) {
                bail!("Cannot include unschedulable worker {}", worker_id)
            }
        }

        let workers = workers
            .into_iter()
            .filter(|worker| worker_ids.contains(&worker.id))
            .collect::<Vec<_>>();

        let worker_parallel_units = workers
            .iter()
            .map(|worker| {
                (
                    worker.id,
                    worker
                        .parallel_units
                        .iter()
                        .map(|parallel_unit| parallel_unit.id as ParallelUnitId)
                        .collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();

        // index for no shuffle relation
        let mut no_shuffle_source_fragment_ids = HashSet::new();
        let mut no_shuffle_target_fragment_ids = HashSet::new();

        // index for fragment_id -> distribution_type
        let mut fragment_distribution_map = HashMap::new();
        // index for actor -> parallel_unit
        let mut actor_status = HashMap::new();
        // index for table_id -> [fragment_id]
        let mut table_fragment_id_map = HashMap::new();
        // index for fragment_id -> [actor_id]
        let mut fragment_actor_id_map = HashMap::new();

        // internal helper func for building index
        fn build_index(
            no_shuffle_source_fragment_ids: &mut HashSet<FragmentId>,
            no_shuffle_target_fragment_ids: &mut HashSet<FragmentId>,
            fragment_distribution_map: &mut HashMap<FragmentId, FragmentDistributionType>,
            actor_status: &mut HashMap<ActorId, ActorStatus>,
            table_fragment_id_map: &mut HashMap<u32, HashSet<FragmentId>>,
            fragment_actor_id_map: &mut HashMap<FragmentId, HashSet<u32>>,
            table_fragments: &BTreeMap<TableId, TableFragments>,
        ) -> MetaResult<()> {
            // This is only for assertion purposes and will be removed once the dispatcher_id is guaranteed to always correspond to the downstream fragment_id,
            // such as through the foreign key constraints in the SQL backend.
            let mut actor_fragment_id_map_for_check = HashMap::new();
            for table_fragments in table_fragments.values() {
                for (fragment_id, fragment) in &table_fragments.fragments {
                    for actor in &fragment.actors {
                        let prev =
                            actor_fragment_id_map_for_check.insert(actor.actor_id, *fragment_id);

                        debug_assert!(prev.is_none());
                    }
                }
            }

            for (table_id, table_fragments) in table_fragments {
                for (fragment_id, fragment) in &table_fragments.fragments {
                    for actor in &fragment.actors {
                        fragment_actor_id_map
                            .entry(*fragment_id)
                            .or_default()
                            .insert(actor.actor_id);

                        for dispatcher in &actor.dispatcher {
                            if dispatcher.r#type() == DispatcherType::NoShuffle {
                                no_shuffle_source_fragment_ids
                                    .insert(actor.fragment_id as FragmentId);

                                let downstream_actor_id =
                                    dispatcher.downstream_actor_id.iter().exactly_one().expect(
                                        "no shuffle should have exactly one downstream actor id",
                                    );

                                if let Some(downstream_fragment_id) =
                                    actor_fragment_id_map_for_check.get(downstream_actor_id)
                                {
                                    // dispatcher_id of dispatcher should be exactly same as downstream fragment id
                                    // but we need to check it to make sure
                                    debug_assert_eq!(
                                        *downstream_fragment_id,
                                        dispatcher.dispatcher_id as FragmentId
                                    );
                                } else {
                                    bail!(
                                        "downstream actor id {} from actor {} not found in fragment_actor_id_map",
                                        downstream_actor_id,
                                        actor.actor_id,
                                    );
                                }

                                no_shuffle_target_fragment_ids
                                    .insert(dispatcher.dispatcher_id as FragmentId);
                            }
                        }
                    }

                    fragment_distribution_map.insert(*fragment_id, fragment.distribution_type());

                    table_fragment_id_map
                        .entry(table_id.table_id())
                        .or_default()
                        .insert(*fragment_id);
                }

                actor_status.extend(table_fragments.actor_status.clone());
            }

            Ok(())
        }

        match &self.metadata_manager {
            MetadataManager::V1(mgr) => {
                let guard = mgr.fragment_manager.get_fragment_read_guard().await;
                build_index(
                    &mut no_shuffle_source_fragment_ids,
                    &mut no_shuffle_target_fragment_ids,
                    &mut fragment_distribution_map,
                    &mut actor_status,
                    &mut table_fragment_id_map,
                    &mut fragment_actor_id_map,
                    guard.table_fragments(),
                )?;
            }
            MetadataManager::V2(_) => {
                let all_table_fragments = self.list_all_table_fragments().await?;
                let all_table_fragments = all_table_fragments
                    .into_iter()
                    .map(|table_fragments| (table_fragments.table_id(), table_fragments))
                    .collect::<BTreeMap<_, _>>();

                build_index(
                    &mut no_shuffle_source_fragment_ids,
                    &mut no_shuffle_target_fragment_ids,
                    &mut fragment_distribution_map,
                    &mut actor_status,
                    &mut table_fragment_id_map,
                    &mut fragment_actor_id_map,
                    &all_table_fragments,
                )?;
            }
        }

        let mut target_plan = HashMap::new();

        for (table_id, parallelism) in table_parallelisms {
            let fragment_map = table_fragment_id_map.remove(&table_id).unwrap();

            for fragment_id in fragment_map {
                // Currently, all of our NO_SHUFFLE relation propagations are only transmitted from upstream to downstream.
                if no_shuffle_target_fragment_ids.contains(&fragment_id) {
                    continue;
                }

                let fragment_parallel_unit_ids: BTreeSet<ParallelUnitId> = fragment_actor_id_map
                    .get(&fragment_id)
                    .unwrap()
                    .iter()
                    .map(|actor_id| {
                        actor_status
                            .get(actor_id)
                            .and_then(|status| status.parallel_unit.clone())
                            .unwrap()
                            .id as ParallelUnitId
                    })
                    .collect();

                let all_available_parallel_unit_ids: BTreeSet<_> =
                    worker_parallel_units.values().flatten().cloned().collect();

                if all_available_parallel_unit_ids.is_empty() {
                    bail!(
                        "No schedulable ParallelUnits available for fragment {}",
                        fragment_id
                    );
                }

                match fragment_distribution_map.get(&fragment_id).unwrap() {
                    FragmentDistributionType::Unspecified => unreachable!(),
                    FragmentDistributionType::Single => {
                        let single_parallel_unit_id =
                            fragment_parallel_unit_ids.iter().exactly_one().unwrap();

                        if all_available_parallel_unit_ids.contains(single_parallel_unit_id) {
                            // NOTE: shall we continue?
                            continue;
                        }

                        let units = schedule_units_for_slots(&worker_parallel_units, 1, table_id)?;

                        let chosen_target_parallel_unit_id = units
                            .values()
                            .flatten()
                            .cloned()
                            .exactly_one()
                            .ok()
                            .with_context(|| format!("Cannot find a single target ParallelUnit for fragment {fragment_id}"))?;

                        target_plan.insert(
                            fragment_id,
                            ParallelUnitReschedule {
                                added_parallel_units: BTreeSet::from([
                                    chosen_target_parallel_unit_id,
                                ]),
                                removed_parallel_units: BTreeSet::from([*single_parallel_unit_id]),
                            },
                        );
                    }
                    FragmentDistributionType::Hash => match parallelism {
                        TableParallelism::Adaptive => {
                            target_plan.insert(
                                fragment_id,
                                Self::diff_parallel_unit_change(
                                    &fragment_parallel_unit_ids,
                                    &all_available_parallel_unit_ids,
                                ),
                            );
                        }
                        TableParallelism::Fixed(mut n) => {
                            let available_parallelism = all_available_parallel_unit_ids.len();

                            if n > available_parallelism {
                                warn!(
                                    "not enough parallel units available for job {} fragment {}, required {}, resetting to {}",
                                    table_id,
                                    fragment_id,
                                    n,
                                    available_parallelism,
                                );

                                n = available_parallelism;
                            }

                            let rebalance_result =
                                schedule_units_for_slots(&worker_parallel_units, n, table_id)?;

                            let target_parallel_unit_ids =
                                rebalance_result.into_values().flatten().collect();

                            target_plan.insert(
                                fragment_id,
                                Self::diff_parallel_unit_change(
                                    &fragment_parallel_unit_ids,
                                    &target_parallel_unit_ids,
                                ),
                            );
                        }
                        TableParallelism::Custom => {
                            // skipping for custom
                        }
                    },
                }
            }
        }

        target_plan.retain(|_, plan| {
            !(plan.added_parallel_units.is_empty() && plan.removed_parallel_units.is_empty())
        });

        Ok(target_plan)
    }

    pub async fn generate_stable_resize_plan(
        &self,
        policy: StableResizePolicy,
        parallel_unit_hints: Option<HashMap<WorkerId, HashSet<ParallelUnitId>>>,
    ) -> MetaResult<HashMap<FragmentId, ParallelUnitReschedule>> {
        let StableResizePolicy {
            fragment_worker_changes,
        } = policy;

        let mut target_plan = HashMap::with_capacity(fragment_worker_changes.len());

        let workers = self
            .metadata_manager
            .list_active_streaming_compute_nodes()
            .await?;

        let unschedulable_worker_ids = Self::filter_unschedulable_workers(&workers);

        for changes in fragment_worker_changes.values() {
            for worker_id in &changes.include_worker_ids {
                if unschedulable_worker_ids.contains(worker_id) {
                    bail!("Cannot include unscheduable worker {}", worker_id)
                }
            }
        }

        let worker_parallel_units = workers
            .iter()
            .map(|worker| {
                (
                    worker.id,
                    worker
                        .parallel_units
                        .iter()
                        .map(|parallel_unit| parallel_unit.id as ParallelUnitId)
                        .collect::<HashSet<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        // FIXME: only need actor id and dispatcher info, avoid clone it.
        let mut actor_map = HashMap::new();
        let mut actor_status = HashMap::new();
        // FIXME: only need fragment distribution info, should avoid clone it.
        let mut fragment_map = HashMap::new();
        let mut fragment_parallelism = HashMap::new();

        // We are reusing code for the metadata manager of both V1 and V2, which will be deprecated in the future.
        fn fulfill_index_by_table_fragments_ref(
            actor_map: &mut HashMap<u32, CustomActorInfo>,
            actor_status: &mut HashMap<ActorId, ActorStatus>,
            fragment_map: &mut HashMap<FragmentId, CustomFragmentInfo>,
            fragment_parallelism: &mut HashMap<FragmentId, TableParallelism>,
            table_fragments: &TableFragments,
        ) {
            for (fragment_id, fragment) in &table_fragments.fragments {
                for actor in &fragment.actors {
                    actor_map.insert(actor.actor_id, CustomActorInfo::from(actor));
                }

                fragment_map.insert(*fragment_id, CustomFragmentInfo::from(fragment));

                fragment_parallelism.insert(*fragment_id, table_fragments.assigned_parallelism);
            }

            actor_status.extend(table_fragments.actor_status.clone());
        }

        match &self.metadata_manager {
            MetadataManager::V1(mgr) => {
                let guard = mgr.fragment_manager.get_fragment_read_guard().await;

                for table_fragments in guard.table_fragments().values() {
                    fulfill_index_by_table_fragments_ref(
                        &mut actor_map,
                        &mut actor_status,
                        &mut fragment_map,
                        &mut fragment_parallelism,
                        table_fragments,
                    );
                }
            }
            MetadataManager::V2(_) => {
                let all_table_fragments = self.list_all_table_fragments().await?;

                for table_fragments in &all_table_fragments {
                    fulfill_index_by_table_fragments_ref(
                        &mut actor_map,
                        &mut actor_status,
                        &mut fragment_map,
                        &mut fragment_parallelism,
                        table_fragments,
                    );
                }
            }
        };

        let mut no_shuffle_source_fragment_ids = HashSet::new();
        let mut no_shuffle_target_fragment_ids = HashSet::new();

        Self::build_no_shuffle_relation_index(
            &actor_map,
            &mut no_shuffle_source_fragment_ids,
            &mut no_shuffle_target_fragment_ids,
        );

        let mut fragment_dispatcher_map = HashMap::new();
        Self::build_fragment_dispatcher_index(&actor_map, &mut fragment_dispatcher_map);

        #[derive(PartialEq, Eq, Clone)]
        struct WorkerChanges {
            include_worker_ids: BTreeSet<WorkerId>,
            exclude_worker_ids: BTreeSet<WorkerId>,
            target_parallelism: Option<usize>,
            target_parallelism_per_worker: Option<usize>,
        }

        let mut fragment_worker_changes: HashMap<_, _> = fragment_worker_changes
            .into_iter()
            .map(|(fragment_id, changes)| {
                (
                    fragment_id as FragmentId,
                    WorkerChanges {
                        include_worker_ids: changes.include_worker_ids.into_iter().collect(),
                        exclude_worker_ids: changes.exclude_worker_ids.into_iter().collect(),
                        target_parallelism: changes.target_parallelism.map(|p| p as usize),
                        target_parallelism_per_worker: changes
                            .target_parallelism_per_worker
                            .map(|p| p as usize),
                    },
                )
            })
            .collect();

        Self::resolve_no_shuffle_upstream_fragments(
            &mut fragment_worker_changes,
            &fragment_map,
            &no_shuffle_source_fragment_ids,
            &no_shuffle_target_fragment_ids,
        )?;

        for (
            fragment_id,
            WorkerChanges {
                include_worker_ids,
                exclude_worker_ids,
                target_parallelism,
                target_parallelism_per_worker,
            },
        ) in fragment_worker_changes
        {
            let fragment = match fragment_map.get(&fragment_id) {
                None => bail!("Fragment id {} not found", fragment_id),
                Some(fragment) => fragment,
            };

            let intersection_ids = include_worker_ids
                .intersection(&exclude_worker_ids)
                .collect_vec();

            if !intersection_ids.is_empty() {
                bail!(
                    "Include worker ids {:?} and exclude worker ids {:?} have intersection {:?}",
                    include_worker_ids,
                    exclude_worker_ids,
                    intersection_ids
                );
            }

            for worker_id in include_worker_ids.iter().chain(exclude_worker_ids.iter()) {
                if !worker_parallel_units.contains_key(worker_id)
                    && !parallel_unit_hints
                        .as_ref()
                        .map(|hints| hints.contains_key(worker_id))
                        .unwrap_or(false)
                {
                    bail!("Worker id {} not found", worker_id);
                }
            }

            let fragment_parallel_unit_ids: BTreeSet<_> = fragment
                .actors
                .iter()
                .map(|actor| {
                    actor_status
                        .get(&actor.actor_id)
                        .and_then(|status| status.parallel_unit.clone())
                        .unwrap()
                        .id as ParallelUnitId
                })
                .collect();

            let worker_to_parallel_unit_ids = |worker_ids: &BTreeSet<WorkerId>| {
                worker_ids
                    .iter()
                    .flat_map(|worker_id| {
                        worker_parallel_units
                            .get(worker_id)
                            .or_else(|| {
                                parallel_unit_hints
                                    .as_ref()
                                    .and_then(|hints| hints.get(worker_id))
                            })
                            .expect("worker id should be valid")
                    })
                    .cloned()
                    .collect_vec()
            };

            let include_worker_parallel_unit_ids = worker_to_parallel_unit_ids(&include_worker_ids);
            let exclude_worker_parallel_unit_ids = worker_to_parallel_unit_ids(&exclude_worker_ids);

            fn refilter_parallel_unit_id_by_target_parallelism(
                worker_parallel_units: &HashMap<u32, HashSet<ParallelUnitId>>,
                include_worker_ids: &BTreeSet<WorkerId>,
                include_worker_parallel_unit_ids: &[ParallelUnitId],
                target_parallel_unit_ids: &mut BTreeSet<ParallelUnitId>,
                target_parallelism_per_worker: usize,
            ) {
                let limited_worker_parallel_unit_ids = include_worker_ids
                    .iter()
                    .flat_map(|worker_id| {
                        worker_parallel_units
                            .get(worker_id)
                            .cloned()
                            .unwrap()
                            .into_iter()
                            .sorted()
                            .take(target_parallelism_per_worker)
                    })
                    .collect_vec();

                // remove all the parallel units in the limited workers
                target_parallel_unit_ids
                    .retain(|id| !include_worker_parallel_unit_ids.contains(id));

                // then we re-add the limited parallel units from the limited workers
                target_parallel_unit_ids.extend(limited_worker_parallel_unit_ids.into_iter());
            }
            match fragment.distribution_type() {
                FragmentDistributionType::Unspecified => unreachable!(),
                FragmentDistributionType::Single => {
                    let single_parallel_unit_id =
                        fragment_parallel_unit_ids.iter().exactly_one().unwrap();

                    let mut target_parallel_unit_ids: BTreeSet<_> = worker_parallel_units
                        .keys()
                        .filter(|id| !unschedulable_worker_ids.contains(*id))
                        .filter(|id| !exclude_worker_ids.contains(*id))
                        .flat_map(|id| worker_parallel_units.get(id).cloned().unwrap())
                        .collect();

                    if let Some(target_parallelism_per_worker) = target_parallelism_per_worker {
                        refilter_parallel_unit_id_by_target_parallelism(
                            &worker_parallel_units,
                            &include_worker_ids,
                            &include_worker_parallel_unit_ids,
                            &mut target_parallel_unit_ids,
                            target_parallelism_per_worker,
                        );
                    }

                    if target_parallel_unit_ids.is_empty() {
                        bail!(
                            "No schedulable ParallelUnits available for single distribution fragment {}",
                            fragment_id
                        );
                    }

                    if !target_parallel_unit_ids.contains(single_parallel_unit_id) {
                        let sorted_target_parallel_unit_ids =
                            target_parallel_unit_ids.into_iter().sorted().collect_vec();

                        let chosen_target_parallel_unit_id = sorted_target_parallel_unit_ids
                            [fragment_id as usize % sorted_target_parallel_unit_ids.len()];

                        target_plan.insert(
                            fragment_id,
                            ParallelUnitReschedule {
                                added_parallel_units: BTreeSet::from([
                                    chosen_target_parallel_unit_id,
                                ]),
                                removed_parallel_units: BTreeSet::from([*single_parallel_unit_id]),
                            },
                        );
                    }
                }
                FragmentDistributionType::Hash => {
                    let mut target_parallel_unit_ids: BTreeSet<_> =
                        fragment_parallel_unit_ids.clone();
                    target_parallel_unit_ids.extend(include_worker_parallel_unit_ids.iter());
                    target_parallel_unit_ids
                        .retain(|id| !exclude_worker_parallel_unit_ids.contains(id));

                    if target_parallel_unit_ids.is_empty() {
                        bail!(
                            "No schedulable ParallelUnits available for fragment {}",
                            fragment_id
                        );
                    }

                    match (target_parallelism, target_parallelism_per_worker) {
                        (Some(_), Some(_)) => {
                            bail!("Cannot specify both target parallelism and target parallelism per worker");
                        }
                        (Some(target_parallelism), _) => {
                            if target_parallel_unit_ids.len() < target_parallelism {
                                bail!("Target parallelism {} is greater than schedulable ParallelUnits {}", target_parallelism, target_parallel_unit_ids.len());
                            }

                            target_parallel_unit_ids = target_parallel_unit_ids
                                .into_iter()
                                .take(target_parallelism)
                                .collect();
                        }
                        (_, Some(target_parallelism_per_worker)) => {
                            refilter_parallel_unit_id_by_target_parallelism(
                                &worker_parallel_units,
                                &include_worker_ids,
                                &include_worker_parallel_unit_ids,
                                &mut target_parallel_unit_ids,
                                target_parallelism_per_worker,
                            );
                        }
                        _ => {}
                    }

                    target_plan.insert(
                        fragment_id,
                        Self::diff_parallel_unit_change(
                            &fragment_parallel_unit_ids,
                            &target_parallel_unit_ids,
                        ),
                    );
                }
            }
        }

        target_plan.retain(|_, plan| {
            !(plan.added_parallel_units.is_empty() && plan.removed_parallel_units.is_empty())
        });

        Ok(target_plan)
    }

    fn filter_unschedulable_workers(workers: &[WorkerNode]) -> HashSet<WorkerId> {
        workers
            .iter()
            .filter(|worker| {
                worker
                    .property
                    .as_ref()
                    .map(|p| p.is_unschedulable)
                    .unwrap_or(false)
            })
            .map(|worker| worker.id as WorkerId)
            .collect()
    }

    fn diff_parallel_unit_change(
        fragment_parallel_unit_ids: &BTreeSet<ParallelUnitId>,
        target_parallel_unit_ids: &BTreeSet<ParallelUnitId>,
    ) -> ParallelUnitReschedule {
        let to_expand_parallel_units = target_parallel_unit_ids
            .difference(fragment_parallel_unit_ids)
            .cloned()
            .collect();

        let to_shrink_parallel_units = fragment_parallel_unit_ids
            .difference(target_parallel_unit_ids)
            .cloned()
            .collect();

        ParallelUnitReschedule {
            added_parallel_units: to_expand_parallel_units,
            removed_parallel_units: to_shrink_parallel_units,
        }
    }

    pub async fn get_reschedule_plan(
        &self,
        policy: Policy,
    ) -> MetaResult<HashMap<FragmentId, ParallelUnitReschedule>> {
        match policy {
            Policy::StableResizePolicy(resize) => {
                self.generate_stable_resize_plan(resize, None).await
            }
        }
    }

    pub fn build_no_shuffle_relation_index(
        actor_map: &HashMap<ActorId, CustomActorInfo>,
        no_shuffle_source_fragment_ids: &mut HashSet<FragmentId>,
        no_shuffle_target_fragment_ids: &mut HashSet<FragmentId>,
    ) {
        let mut fragment_cache = HashSet::new();
        for actor in actor_map.values() {
            if fragment_cache.contains(&actor.fragment_id) {
                continue;
            }

            for dispatcher in &actor.dispatcher {
                for downstream_actor_id in &dispatcher.downstream_actor_id {
                    if let Some(downstream_actor) = actor_map.get(downstream_actor_id) {
                        // Checking for no shuffle dispatchers
                        if dispatcher.r#type() == DispatcherType::NoShuffle {
                            no_shuffle_source_fragment_ids.insert(actor.fragment_id as FragmentId);
                            no_shuffle_target_fragment_ids
                                .insert(downstream_actor.fragment_id as FragmentId);
                        }
                    }
                }
            }

            fragment_cache.insert(actor.fragment_id);
        }
    }

    pub fn build_fragment_dispatcher_index(
        actor_map: &HashMap<ActorId, CustomActorInfo>,
        fragment_dispatcher_map: &mut HashMap<FragmentId, HashMap<FragmentId, DispatcherType>>,
    ) {
        for actor in actor_map.values() {
            for dispatcher in &actor.dispatcher {
                for downstream_actor_id in &dispatcher.downstream_actor_id {
                    if let Some(downstream_actor) = actor_map.get(downstream_actor_id) {
                        fragment_dispatcher_map
                            .entry(actor.fragment_id as FragmentId)
                            .or_default()
                            .insert(
                                downstream_actor.fragment_id as FragmentId,
                                dispatcher.r#type(),
                            );
                    }
                }
            }
        }
    }

    pub fn resolve_no_shuffle_upstream_tables(
        fragment_ids: HashSet<FragmentId>,
        fragment_map: &HashMap<FragmentId, CustomFragmentInfo>,
        no_shuffle_source_fragment_ids: &HashSet<FragmentId>,
        no_shuffle_target_fragment_ids: &HashSet<FragmentId>,
        fragment_to_table: &HashMap<FragmentId, TableId>,
        table_parallelisms: &mut HashMap<TableId, TableParallelism>,
    ) -> MetaResult<()> {
        let mut queue: VecDeque<FragmentId> = fragment_ids.iter().cloned().collect();

        let mut fragment_ids = fragment_ids;

        // We trace the upstreams of each downstream under the hierarchy until we reach the top
        // for every no_shuffle relation.
        while let Some(fragment_id) = queue.pop_front() {
            if !no_shuffle_target_fragment_ids.contains(&fragment_id)
                && !no_shuffle_source_fragment_ids.contains(&fragment_id)
            {
                continue;
            }

            // for upstream
            for upstream_fragment_id in &fragment_map
                .get(&fragment_id)
                .unwrap()
                .upstream_fragment_ids
            {
                if !no_shuffle_source_fragment_ids.contains(upstream_fragment_id) {
                    continue;
                }

                let table_id = fragment_to_table.get(&fragment_id).unwrap();
                let upstream_table_id = fragment_to_table.get(upstream_fragment_id).unwrap();

                // Only custom parallelism will be propagated to the no shuffle upstream.
                if let Some(TableParallelism::Custom) = table_parallelisms.get(table_id) {
                    if let Some(upstream_table_parallelism) =
                        table_parallelisms.get(upstream_table_id)
                    {
                        if upstream_table_parallelism != &TableParallelism::Custom {
                            bail!(
                                "Cannot change upstream table {} from {:?} to {:?}",
                                upstream_table_id,
                                upstream_table_parallelism,
                                TableParallelism::Custom
                            )
                        }
                    } else {
                        table_parallelisms.insert(*upstream_table_id, TableParallelism::Custom);
                    }
                }

                fragment_ids.insert(*upstream_fragment_id);
                queue.push_back(*upstream_fragment_id);
            }
        }

        let downstream_fragment_ids = fragment_ids
            .iter()
            .filter(|fragment_id| no_shuffle_target_fragment_ids.contains(fragment_id));

        let downstream_table_ids = downstream_fragment_ids
            .map(|fragment_id| fragment_to_table.get(fragment_id).unwrap())
            .collect::<HashSet<_>>();

        table_parallelisms.retain(|table_id, _| !downstream_table_ids.contains(table_id));

        Ok(())
    }

    pub fn resolve_no_shuffle_upstream_fragments<T>(
        reschedule: &mut HashMap<FragmentId, T>,
        fragment_map: &HashMap<FragmentId, CustomFragmentInfo>,
        no_shuffle_source_fragment_ids: &HashSet<FragmentId>,
        no_shuffle_target_fragment_ids: &HashSet<FragmentId>,
    ) -> MetaResult<()>
    where
        T: Clone + Eq,
    {
        let mut queue: VecDeque<FragmentId> = reschedule.keys().cloned().collect();

        // We trace the upstreams of each downstream under the hierarchy until we reach the top
        // for every no_shuffle relation.
        while let Some(fragment_id) = queue.pop_front() {
            if !no_shuffle_target_fragment_ids.contains(&fragment_id) {
                continue;
            }

            // for upstream
            for upstream_fragment_id in &fragment_map
                .get(&fragment_id)
                .unwrap()
                .upstream_fragment_ids
            {
                if !no_shuffle_source_fragment_ids.contains(upstream_fragment_id) {
                    continue;
                }

                let reschedule_plan = reschedule.get(&fragment_id).unwrap();

                if let Some(upstream_reschedule_plan) = reschedule.get(upstream_fragment_id) {
                    if upstream_reschedule_plan != reschedule_plan {
                        bail!("Inconsistent NO_SHUFFLE plan, check target worker ids of fragment {} and {}", fragment_id, upstream_fragment_id);
                    }

                    continue;
                }

                reschedule.insert(*upstream_fragment_id, reschedule_plan.clone());

                queue.push_back(*upstream_fragment_id);
            }
        }

        reschedule.retain(|fragment_id, _| !no_shuffle_target_fragment_ids.contains(fragment_id));

        Ok(())
    }
}

// At present, for table level scaling, we use the strategy TableResizePolicy.
// Currently, this is used as an internal interface, so it won’t be included in Protobuf for the time being.
pub struct TableResizePolicy {
    pub(crate) worker_ids: BTreeSet<WorkerId>,
    pub(crate) table_parallelisms: HashMap<u32, TableParallelism>,
}

impl GlobalStreamManager {
    pub async fn reschedule_lock_read_guard(&self) -> RwLockReadGuard<'_, ()> {
        self.scale_controller.reschedule_lock.read().await
    }

    pub async fn reschedule_lock_write_guard(&self) -> RwLockWriteGuard<'_, ()> {
        self.scale_controller.reschedule_lock.write().await
    }

    pub async fn reschedule_actors(
        &self,
        reschedules: HashMap<FragmentId, ParallelUnitReschedule>,
        options: RescheduleOptions,
        table_parallelism: Option<HashMap<TableId, TableParallelism>>,
    ) -> MetaResult<()> {
        let mut revert_funcs = vec![];
        if let Err(e) = self
            .reschedule_actors_impl(&mut revert_funcs, reschedules, options, table_parallelism)
            .await
        {
            for revert_func in revert_funcs.into_iter().rev() {
                revert_func.await;
            }
            return Err(e);
        }

        Ok(())
    }

    async fn reschedule_actors_impl(
        &self,
        revert_funcs: &mut Vec<BoxFuture<'_, ()>>,
        reschedules: HashMap<FragmentId, ParallelUnitReschedule>,
        options: RescheduleOptions,
        table_parallelism: Option<HashMap<TableId, TableParallelism>>,
    ) -> MetaResult<()> {
        let mut table_parallelism = table_parallelism;

        let (reschedule_fragment, applied_reschedules) = self
            .scale_controller
            .prepare_reschedule_command(reschedules, options, table_parallelism.as_mut())
            .await?;

        tracing::debug!("reschedule plan: {:?}", reschedule_fragment);

        let up_down_stream_fragment: HashSet<_> = reschedule_fragment
            .iter()
            .flat_map(|(_, reschedule)| {
                reschedule
                    .upstream_fragment_dispatcher_ids
                    .iter()
                    .map(|(fragment_id, _)| *fragment_id)
                    .chain(reschedule.downstream_fragment_ids.iter().cloned())
            })
            .collect();

        let fragment_actors =
            try_join_all(up_down_stream_fragment.iter().map(|fragment_id| async {
                let actor_ids = self
                    .metadata_manager
                    .get_running_actors_of_fragment(*fragment_id)
                    .await?;
                Result::<_, MetaError>::Ok((*fragment_id, actor_ids))
            }))
            .await?
            .into_iter()
            .collect();

        let command = Command::RescheduleFragment {
            reschedules: reschedule_fragment,
            table_parallelism: table_parallelism.unwrap_or_default(),
            fragment_actors,
        };

        match &self.metadata_manager {
            MetadataManager::V1(mgr) => {
                let fragment_manager_ref = mgr.fragment_manager.clone();

                revert_funcs.push(Box::pin(async move {
                    fragment_manager_ref
                        .cancel_apply_reschedules(applied_reschedules)
                        .await;
                }));
            }
            MetadataManager::V2(_) => {
                // meta model v2 does not need to revert
            }
        }

        tracing::debug!("pausing tick lock in source manager");
        let _source_pause_guard = self.source_manager.paused.lock().await;

        self.barrier_scheduler
            .run_config_change_command_with_pause(command)
            .await?;

        tracing::info!("reschedule done");

        Ok(())
    }

    async fn trigger_parallelism_control(&self) -> MetaResult<bool> {
        let background_streaming_jobs = self
            .metadata_manager
            .list_background_creating_jobs()
            .await?;

        if !background_streaming_jobs.is_empty() {
            tracing::debug!(
                "skipping parallelism control due to background jobs {:?}",
                background_streaming_jobs
            );
            // skip if there are background creating jobs
            return Ok(true);
        }

        tracing::info!("trigger parallelism control");

        let _reschedule_job_lock = self.reschedule_lock_write_guard().await;

        let (schedulable_worker_ids, table_parallelisms) = match &self.metadata_manager {
            MetadataManager::V1(mgr) => {
                let table_parallelisms: HashMap<u32, TableParallelism> = {
                    let guard = mgr.fragment_manager.get_fragment_read_guard().await;

                    guard
                        .table_fragments()
                        .iter()
                        .filter(|&(_, table)| matches!(table.state(), State::Created))
                        .map(|(table_id, table)| (table_id.table_id, table.assigned_parallelism))
                        .collect()
                };

                let workers = mgr
                    .cluster_manager
                    .list_active_streaming_compute_nodes()
                    .await;

                let schedulable_worker_ids: BTreeSet<_> = workers
                    .iter()
                    .filter(|worker| {
                        !worker
                            .property
                            .as_ref()
                            .map(|p| p.is_unschedulable)
                            .unwrap_or(false)
                    })
                    .map(|worker| worker.id)
                    .collect();

                (schedulable_worker_ids, table_parallelisms)
            }
            MetadataManager::V2(mgr) => {
                let table_parallelisms: HashMap<_, _> = {
                    let streaming_parallelisms = mgr
                        .catalog_controller
                        .get_all_created_streaming_parallelisms()
                        .await?;

                    streaming_parallelisms
                        .into_iter()
                        .map(|(table_id, parallelism)| {
                            let table_parallelism = match parallelism {
                                StreamingParallelism::Adaptive => TableParallelism::Adaptive,
                                StreamingParallelism::Fixed(n) => TableParallelism::Fixed(n),
                                StreamingParallelism::Custom => TableParallelism::Custom,
                            };

                            (table_id as u32, table_parallelism)
                        })
                        .collect()
                };

                let workers = mgr
                    .cluster_controller
                    .list_active_streaming_workers()
                    .await?;

                let schedulable_worker_ids = workers
                    .iter()
                    .filter(|worker| {
                        !worker
                            .property
                            .as_ref()
                            .map(|p| p.is_unschedulable)
                            .unwrap_or(false)
                    })
                    .map(|worker| worker.id)
                    .collect();

                (schedulable_worker_ids, table_parallelisms)
            }
        };

        if table_parallelisms.is_empty() {
            tracing::info!("no streaming jobs for scaling, maybe an empty cluster");
            return Ok(false);
        }

        let batch_size = match self.env.opts.parallelism_control_batch_size {
            0 => table_parallelisms.len(),
            n => n,
        };

        tracing::info!(
            "total {} streaming jobs, batch size {}, schedulable worker ids: {:?}",
            table_parallelisms.len(),
            batch_size,
            schedulable_worker_ids
        );

        let batches: Vec<_> = table_parallelisms
            .into_iter()
            .chunks(batch_size)
            .into_iter()
            .map(|chunk| chunk.collect_vec())
            .collect();

        let mut reschedules = None;

        for batch in batches {
            let parallelisms: HashMap<_, _> = batch.into_iter().collect();

            let plan = self
                .scale_controller
                .generate_table_resize_plan(TableResizePolicy {
                    worker_ids: schedulable_worker_ids.clone(),
                    table_parallelisms: parallelisms.clone(),
                })
                .await?;

            if !plan.is_empty() {
                tracing::info!(
                    "reschedule plan generated for streaming jobs {:?}",
                    parallelisms
                );
                reschedules = Some(plan);
                break;
            }
        }

        let Some(reschedules) = reschedules else {
            tracing::info!("no reschedule plan generated");
            return Ok(false);
        };

        self.reschedule_actors(
            reschedules,
            RescheduleOptions {
                resolve_no_shuffle_upstream: false,
                skip_create_new_actors: false,
            },
            None,
        )
        .await?;

        Ok(true)
    }

    async fn run(&self, mut shutdown_rx: Receiver<()>) {
        tracing::info!("starting automatic parallelism control monitor");

        let check_period =
            Duration::from_secs(self.env.opts.parallelism_control_trigger_period_sec);

        let mut ticker = tokio::time::interval_at(
            Instant::now()
                + Duration::from_secs(self.env.opts.parallelism_control_trigger_first_delay_sec),
            check_period,
        );
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // waiting for first tick
        ticker.tick().await;

        let (local_notification_tx, mut local_notification_rx) =
            tokio::sync::mpsc::unbounded_channel();

        self.env
            .notification_manager()
            .insert_local_sender(local_notification_tx)
            .await;

        let worker_nodes = self
            .metadata_manager
            .list_active_streaming_compute_nodes()
            .await
            .expect("list active streaming compute nodes");

        let mut worker_cache: BTreeMap<_, _> = worker_nodes
            .into_iter()
            .map(|worker| (worker.id, worker))
            .collect();

        let mut should_trigger = false;

        loop {
            tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    tracing::info!("Stream manager is stopped");
                    break;
                }

                _ = ticker.tick(), if should_trigger => {
                    let include_workers = worker_cache.keys().copied().collect_vec();

                    if include_workers.is_empty() {
                        tracing::debug!("no available worker nodes");
                        should_trigger = false;
                        continue;
                    }

                    match self.trigger_parallelism_control().await {
                        Ok(cont) => {
                            should_trigger = cont;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e.as_report(), "Failed to trigger scale out, waiting for next tick to retry after {}s", ticker.period().as_secs());
                            ticker.reset();
                        }
                    }
                }

                notification = local_notification_rx.recv() => {
                    let notification = notification.expect("local notification channel closed in loop of stream manager");

                    match notification {
                        LocalNotification::WorkerNodeActivated(worker) => {
                            match (worker.get_type(), worker.property.as_ref()) {
                                (Ok(WorkerType::ComputeNode), Some(prop)) if prop.is_streaming => {
                                    tracing::info!("worker {} activated notification received", worker.id);
                                }
                                _ => continue
                            }

                            let prev_worker = worker_cache.insert(worker.id, worker.clone());

                            match prev_worker {
                                Some(prev_worker) if prev_worker.parallel_units != worker.parallel_units  => {
                                    tracing::info!(worker = worker.id, "worker parallelism changed");
                                    should_trigger = true;
                                }
                                None => {
                                    tracing::info!(worker = worker.id, "new worker joined");
                                    should_trigger = true;
                                }
                                _ => {}
                            }
                        }

                        // Since our logic for handling passive scale-in is within the barrier manager,
                        // there’s not much we can do here. All we can do is proactively remove the entries from our cache.
                        LocalNotification::WorkerNodeDeleted(worker) => {
                            match worker_cache.remove(&worker.id) {
                                Some(prev_worker) => {
                                    tracing::info!(worker = prev_worker.id, "worker removed from stream manager cache");
                                }

                                None => {
                                    tracing::warn!(worker = worker.id, "worker not found in stream manager cache, but it was removed");
                                }
                            }
                        }

                        _ => {}
                    }
                }
            }
        }
    }

    pub fn start_auto_parallelism_monitor(
        self: Arc<Self>,
    ) -> (JoinHandle<()>, oneshot::Sender<()>) {
        tracing::info!("Automatic parallelism scale-out is enabled for streaming jobs");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let join_handle = tokio::spawn(async move {
            self.run(shutdown_rx).await;
        });

        (join_handle, shutdown_tx)
    }
}

// We redistribute parallel units (which will be ensembles in the future) through a simple consistent hashing ring.
// Note that we have added some simple logic here to ensure the consistency of the ratio between each slot,
// especially when equal division is needed.
pub fn schedule_units_for_slots(
    slots: &BTreeMap<WorkerId, BTreeSet<ParallelUnitId>>,
    total_unit_size: usize,
    salt: u32,
) -> MetaResult<BTreeMap<WorkerId, BTreeSet<ParallelUnitId>>> {
    let mut ch = ConsistentHashRing::new(salt);

    for (worker_id, parallel_unit_ids) in slots {
        ch.add_worker(*worker_id, parallel_unit_ids.len() as u32);
    }

    let target_distribution = ch.distribute_tasks(total_unit_size as u32)?;

    Ok(slots
        .iter()
        .map(|(worker_id, parallel_unit_ids)| {
            (
                *worker_id,
                parallel_unit_ids
                    .iter()
                    .take(
                        target_distribution
                            .get(worker_id)
                            .cloned()
                            .unwrap_or_default() as usize,
                    )
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .collect())
}

pub struct ConsistentHashRing {
    ring: BTreeMap<u64, u32>,
    capacities: BTreeMap<u32, u32>,
    virtual_nodes: u32,
    salt: u32,
}

impl ConsistentHashRing {
    fn new(salt: u32) -> Self {
        ConsistentHashRing {
            ring: BTreeMap::new(),
            capacities: BTreeMap::new(),
            virtual_nodes: 1024,
            salt,
        }
    }

    fn hash<T: Hash, S: Hash>(key: T, salt: S) -> u64 {
        let mut hasher = DefaultHasher::new();
        salt.hash(&mut hasher);
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn add_worker(&mut self, id: u32, capacity: u32) {
        let virtual_nodes_count = self.virtual_nodes;

        for i in 0..virtual_nodes_count {
            let virtual_node_key = (id, i);
            let hash = Self::hash(virtual_node_key, self.salt);
            self.ring.insert(hash, id);
        }

        self.capacities.insert(id, capacity);
    }

    fn distribute_tasks(&self, total_tasks: u32) -> MetaResult<BTreeMap<u32, u32>> {
        let total_capacity = self.capacities.values().sum::<u32>();

        if total_capacity < total_tasks {
            bail!("Total tasks exceed the total weight of all workers.");
        }

        let mut soft_limits = HashMap::new();
        for (worker_id, worker_capacity) in &self.capacities {
            soft_limits.insert(
                *worker_id,
                (total_tasks as f64 * (*worker_capacity as f64 / total_capacity as f64)).ceil()
                    as u32,
            );
        }

        let mut task_distribution: BTreeMap<u32, u32> = BTreeMap::new();
        let mut task_hashes = (0..total_tasks)
            .map(|task_idx| Self::hash(task_idx, self.salt))
            .collect_vec();

        // Sort task hashes to disperse them around the hash ring
        task_hashes.sort();

        for task_hash in task_hashes {
            let mut assigned = false;

            // Iterator that starts from the current task_hash or the next node in the ring
            let ring_range = self.ring.range(task_hash..).chain(self.ring.iter());

            for (_, &worker_id) in ring_range {
                let worker_capacity = self.capacities.get(&worker_id).unwrap();
                let worker_soft_limit = soft_limits.get(&worker_id).unwrap();

                let task_limit = min(*worker_capacity, *worker_soft_limit);

                let worker_task_count = task_distribution.entry(worker_id).or_insert(0);

                if *worker_task_count < task_limit {
                    *worker_task_count += 1;
                    assigned = true;
                    break;
                }
            }

            if !assigned {
                bail!("Could not distribute tasks due to capacity constraints.");
            }
        }

        Ok(task_distribution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_SALT: u32 = 42;

    #[test]
    fn test_single_worker_capacity() {
        let mut ch = ConsistentHashRing::new(DEFAULT_SALT);
        ch.add_worker(1, 10);

        let total_tasks = 5;
        let task_distribution = ch.distribute_tasks(total_tasks).unwrap();

        assert_eq!(task_distribution.get(&1).cloned().unwrap_or(0), 5);
    }

    #[test]
    fn test_multiple_workers_even_distribution() {
        let mut ch = ConsistentHashRing::new(DEFAULT_SALT);

        ch.add_worker(1, 1);
        ch.add_worker(2, 1);
        ch.add_worker(3, 1);

        let total_tasks = 3;
        let task_distribution = ch.distribute_tasks(total_tasks).unwrap();

        for id in 1..=3 {
            assert_eq!(task_distribution.get(&id).cloned().unwrap_or(0), 1);
        }
    }

    #[test]
    fn test_weighted_distribution() {
        let mut ch = ConsistentHashRing::new(DEFAULT_SALT);

        ch.add_worker(1, 2);
        ch.add_worker(2, 3);
        ch.add_worker(3, 5);

        let total_tasks = 10;
        let task_distribution = ch.distribute_tasks(total_tasks).unwrap();

        assert_eq!(task_distribution.get(&1).cloned().unwrap_or(0), 2);
        assert_eq!(task_distribution.get(&2).cloned().unwrap_or(0), 3);
        assert_eq!(task_distribution.get(&3).cloned().unwrap_or(0), 5);
    }

    #[test]
    fn test_over_capacity() {
        let mut ch = ConsistentHashRing::new(DEFAULT_SALT);

        ch.add_worker(1, 1);
        ch.add_worker(2, 2);
        ch.add_worker(3, 3);

        let total_tasks = 10; // More tasks than the total weight
        let task_distribution = ch.distribute_tasks(total_tasks);

        assert!(task_distribution.is_err());
    }

    #[test]
    fn test_balance_distribution() {
        for mut worker_capacity in 1..10 {
            for workers in 3..10 {
                let mut ring = ConsistentHashRing::new(DEFAULT_SALT);

                for worker_id in 0..workers {
                    ring.add_worker(worker_id, worker_capacity);
                }

                // Here we simulate a real situation where the actual parallelism cannot fill all the capacity.
                // This is to ensure an average distribution, for example, when three workers with 6 parallelism are assigned 9 tasks,
                // they should ideally get an exact distribution of 3, 3, 3 respectively.
                if worker_capacity % 2 == 0 {
                    worker_capacity /= 2;
                }

                let total_tasks = worker_capacity * workers;

                let task_distribution = ring.distribute_tasks(total_tasks).unwrap();

                for (_, v) in task_distribution {
                    assert_eq!(v, worker_capacity);
                }
            }
        }
    }
}
