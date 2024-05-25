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
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::iter;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use itertools::Itertools;
use risingwave_common::hash::ParallelUnitId;
use risingwave_common::util::addr::HostAddr;
use risingwave_common::util::resource_util::cpu::total_cpu_available;
use risingwave_common::util::resource_util::memory::system_memory_available_bytes;
use risingwave_common::RW_VERSION;
use risingwave_pb::common::worker_node::{Property, State};
use risingwave_pb::common::{HostAddress, ParallelUnit, WorkerNode, WorkerType};
use risingwave_pb::meta::add_worker_node_request::Property as AddNodeProperty;
use risingwave_pb::meta::heartbeat_request;
use risingwave_pb::meta::subscribe_response::{Info, Operation};
use risingwave_pb::meta::update_worker_node_schedulability_request::Schedulability;
use thiserror_ext::AsReport;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::oneshot::Sender;
use tokio::sync::{RwLock, RwLockReadGuard};
use tokio::task::JoinHandle;

use crate::manager::{IdCategory, LocalNotification, MetaSrvEnv};
use crate::model::{
    InMemValTransaction, MetadataModel, ValTransaction, VarTransaction, Worker, INVALID_EXPIRE_AT,
};
use crate::storage::{MetaStore, Transaction};
use crate::{MetaError, MetaResult};

pub type WorkerId = u32;
pub type WorkerLocations = HashMap<WorkerId, WorkerNode>;
pub type ClusterManagerRef = Arc<ClusterManager>;

#[derive(Clone, Debug)]
pub struct WorkerKey(pub HostAddress);

impl PartialEq<Self> for WorkerKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl Eq for WorkerKey {}

impl Hash for WorkerKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.host.hash(state);
        self.0.port.hash(state);
    }
}

/// The id preserved for the meta node. Note that there's no such entry in cluster manager.
pub const META_NODE_ID: u32 = 0;

/// [`ClusterManager`] manager cluster/worker meta data in [`MetaStore`].
pub struct ClusterManager {
    env: MetaSrvEnv,

    max_heartbeat_interval: Duration,

    core: RwLock<ClusterManagerCore>,
}

impl ClusterManager {
    pub async fn new(env: MetaSrvEnv, max_heartbeat_interval: Duration) -> MetaResult<Self> {
        let core = ClusterManagerCore::new(env.clone()).await?;

        Ok(Self {
            env,
            max_heartbeat_interval,
            core: RwLock::new(core),
        })
    }

    /// Used in `NotificationService::subscribe`.
    /// Need to pay attention to the order of acquiring locks to prevent deadlock problems.
    pub async fn get_cluster_core_guard(&self) -> RwLockReadGuard<'_, ClusterManagerCore> {
        self.core.read().await
    }

    pub async fn count_worker_node(&self) -> HashMap<WorkerType, u64> {
        self.core.read().await.count_worker_node()
    }

    /// A worker node will immediately register itself to meta when it bootstraps.
    /// The meta will assign it with a unique ID and set its state as `Starting`.
    /// When the worker node is fully ready to serve, it will request meta again
    /// (via `activate_worker_node`) to set its state to `Running`.
    pub async fn add_worker_node(
        &self,
        r#type: WorkerType,
        host_address: HostAddress,
        property: AddNodeProperty,
        resource: risingwave_pb::common::worker_node::Resource,
    ) -> MetaResult<WorkerNode> {
        let new_worker_parallelism = property.worker_node_parallelism as usize;
        let mut property = self.parse_property(r#type, property);
        let mut core = self.core.write().await;

        if let Some(worker) = core.get_worker_by_host_mut(host_address.clone()) {
            tracing::info!("worker {} re-joined the cluster", worker.worker_id());
            worker.update_resource(Some(resource));
            worker.update_started_at(timestamp_now_sec());
            if let Some(property) = &mut property {
                property.is_unschedulable = worker
                    .worker_node
                    .property
                    .as_ref()
                    .map(|p| p.is_unschedulable)
                    .unwrap_or_default();
            }

            let old_worker_parallelism = worker.worker_node.parallel_units.len();
            if old_worker_parallelism == new_worker_parallelism
                && worker.worker_node.property == property
            {
                worker.update_expire_at(self.max_heartbeat_interval);
                return Ok(worker.to_protobuf());
            }

            let mut new_worker = worker.clone();
            match old_worker_parallelism.cmp(&new_worker_parallelism) {
                Ordering::Less => {
                    tracing::info!(
                        "worker {} parallelism updated from {} to {}",
                        new_worker.worker_node.id,
                        old_worker_parallelism,
                        new_worker_parallelism
                    );
                    let parallel_units = self
                        .generate_cn_parallel_units(
                            new_worker_parallelism - old_worker_parallelism,
                            new_worker.worker_id(),
                        )
                        .await?;
                    new_worker.worker_node.parallel_units.extend(parallel_units);
                }
                Ordering::Greater => {
                    if !self.env.opts.disable_automatic_parallelism_control {
                        // Handing over to the subsequent recovery loop for a forced reschedule.
                        tracing::info!(
                            "worker {} parallelism reduced from {} to {}",
                            new_worker.worker_node.id,
                            old_worker_parallelism,
                            new_worker_parallelism
                        );
                        new_worker
                            .worker_node
                            .parallel_units
                            .truncate(new_worker_parallelism)
                    } else {
                        // Warn and keep the original parallelism if the worker registered with a
                        // smaller parallelism, entering compatibility mode.
                        tracing::warn!(
                            "worker {} parallelism is less than current, current is {}, but received {}",
                            new_worker.worker_id(),
                            new_worker_parallelism,
                            old_worker_parallelism,
                        );
                    }
                }
                Ordering::Equal => {}
            }
            if property != new_worker.worker_node.property {
                tracing::info!(
                    "worker {} property updated from {:?} to {:?}",
                    new_worker.worker_node.id,
                    new_worker.worker_node.property,
                    property
                );

                new_worker.worker_node.property = property;
            }

            new_worker.update_expire_at(self.max_heartbeat_interval);
            new_worker.insert(self.env.meta_store().as_kv()).await?;
            *worker = new_worker;
            return Ok(worker.to_protobuf());
        }

        // Generate worker id.
        let worker_id = self
            .env
            .id_gen_manager()
            .as_kv()
            .generate::<{ IdCategory::Worker }>()
            .await? as WorkerId;

        let transactional_id = match (core.available_transactional_ids.front(), r#type) {
            (None, _) => return Err(MetaError::unavailable("no available reusable machine id")),
            // We only assign transactional id to compute node and frontend.
            (Some(id), WorkerType::ComputeNode | WorkerType::Frontend) => Some(*id),
            _ => None,
        };

        // Generate parallel units.
        let parallel_units = if r#type == WorkerType::ComputeNode {
            self.generate_cn_parallel_units(new_worker_parallelism, worker_id)
                .await?
        } else {
            vec![]
        };
        // Construct worker.
        let worker_node = WorkerNode {
            id: worker_id,
            r#type: r#type as i32,
            host: Some(host_address.clone()),
            state: State::Starting as i32,
            parallel_units,
            property,
            transactional_id,
            // resource doesn't need persist
            resource: None,
            started_at: None,
        };

        let mut worker = Worker::from_protobuf(worker_node.clone());
        worker.update_started_at(timestamp_now_sec());
        worker.update_resource(Some(resource));
        // Persist worker node.
        worker.insert(self.env.meta_store().as_kv()).await?;
        // Update core.
        core.add_worker_node(worker);

        tracing::info!(
            "new worker {} from {}:{} joined cluster",
            worker_id,
            host_address.get_host(),
            host_address.get_port()
        );

        Ok(worker_node)
    }

    pub async fn activate_worker_node(&self, host_address: HostAddress) -> MetaResult<()> {
        let mut core = self.core.write().await;
        let mut worker = core.get_worker_by_host_checked(host_address.clone())?;

        let worker_id = worker.worker_id();

        tracing::info!("worker {} activating", worker_id);

        if worker.worker_node.state != State::Running as i32 {
            worker.worker_node.state = State::Running as i32;
            worker.insert(self.env.meta_store().as_kv()).await?;
            core.update_worker_node(worker.clone());
        }

        // Notify frontends of new compute node.
        // Always notify because a running worker's property may have been changed.
        if worker.worker_type() == WorkerType::ComputeNode {
            self.env
                .notification_manager()
                .notify_frontend(Operation::Add, Info::Node(worker.worker_node.clone()))
                .await;
        }
        self.env
            .notification_manager()
            .notify_local_subscribers(LocalNotification::WorkerNodeActivated(worker.worker_node))
            .await;

        tracing::info!("worker {} activated", worker_id);

        Ok(())
    }

    pub async fn update_schedulability(
        &self,
        worker_ids: Vec<u32>,
        schedulability: Schedulability,
    ) -> MetaResult<()> {
        let worker_ids: HashSet<_> = worker_ids.into_iter().collect();

        let mut core = self.core.write().await;
        let mut txn = Transaction::default();
        let mut var_txns = vec![];

        for worker in core.workers.values_mut() {
            if worker_ids.contains(&worker.worker_node.id) {
                if let Some(property) = worker.worker_node.property.as_mut() {
                    let target = schedulability == Schedulability::Unschedulable;
                    if property.is_unschedulable != target {
                        let mut var_txn = VarTransaction::new(worker);
                        var_txn
                            .worker_node
                            .property
                            .as_mut()
                            .unwrap()
                            .is_unschedulable = target;

                        var_txn.apply_to_txn(&mut txn).await?;
                        var_txns.push(var_txn);
                    }
                }
            }
        }

        self.env.meta_store().as_kv().txn(txn).await?;

        for var_txn in var_txns {
            var_txn.commit();
        }

        Ok(())
    }

    pub async fn delete_worker_node(&self, host_address: HostAddress) -> MetaResult<WorkerType> {
        let mut core = self.core.write().await;
        let worker = core.get_worker_by_host_checked(host_address.clone())?;
        let worker_type = worker.worker_type();
        let worker_node = worker.to_protobuf();

        // Persist deletion.
        Worker::delete(self.env.meta_store().as_kv(), &host_address).await?;

        // Update core.
        core.delete_worker_node(worker);

        // Notify frontends to delete compute node.
        if worker_type == WorkerType::ComputeNode {
            self.env
                .notification_manager()
                .notify_frontend(Operation::Delete, Info::Node(worker_node.clone()))
                .await;
        }

        // Notify local subscribers.
        // Note: Any type of workers may pin some hummock resource. So `HummockManager` expect this
        // local notification.
        self.env
            .notification_manager()
            .notify_local_subscribers(LocalNotification::WorkerNodeDeleted(worker_node))
            .await;

        Ok(worker_type)
    }

    /// Invoked when it receives a heartbeat from a worker node.
    pub async fn heartbeat(
        &self,
        worker_id: WorkerId,
        info: Vec<heartbeat_request::extra_info::Info>,
    ) -> MetaResult<()> {
        tracing::debug!(target: "events::meta::server_heartbeat", worker_id, "receive heartbeat");
        let mut core = self.core.write().await;
        for worker in core.workers.values_mut() {
            if worker.worker_id() == worker_id {
                worker.update_expire_at(self.max_heartbeat_interval);
                worker.update_info(info);
                return Ok(());
            }
        }
        Err(MetaError::invalid_worker(worker_id, "worker not found"))
    }

    pub fn start_heartbeat_checker(
        cluster_manager: ClusterManagerRef,
        check_interval: Duration,
    ) -> (JoinHandle<()>, Sender<()>) {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let join_handle = tokio::spawn(async move {
            let mut min_interval = tokio::time::interval(check_interval);
            min_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    // Wait for interval
                    _ = min_interval.tick() => {},
                    // Shutdown
                    _ = &mut shutdown_rx => {
                        tracing::info!("Heartbeat checker is stopped");
                        return;
                    }
                }
                let (workers_to_delete, now) = {
                    let mut core = cluster_manager.core.write().await;
                    let workers = &mut core.workers;
                    // 1. Initialize new workers' TTL.
                    for worker in workers
                        .values_mut()
                        .filter(|worker| worker.expire_at == INVALID_EXPIRE_AT)
                    {
                        worker.update_expire_at(cluster_manager.max_heartbeat_interval);
                    }
                    // 2. Collect expired workers.
                    let now = timestamp_now_sec();
                    (
                        workers
                            .values()
                            .filter(|worker| worker.expire_at < now)
                            .map(|worker| (worker.worker_id(), worker.key().unwrap()))
                            .collect_vec(),
                        now,
                    )
                };
                // 3. Delete expired workers.
                for (worker_id, key) in workers_to_delete {
                    match cluster_manager.delete_worker_node(key.clone()).await {
                        Ok(worker_type) => {
                            match worker_type {
                                WorkerType::Frontend
                                | WorkerType::ComputeNode
                                | WorkerType::Compactor
                                | WorkerType::RiseCtl => {
                                    cluster_manager
                                        .env
                                        .notification_manager()
                                        .delete_sender(worker_type, WorkerKey(key.clone()))
                                        .await
                                }
                                _ => {}
                            };
                            tracing::warn!(
                                "Deleted expired worker {} {:#?}, current timestamp {}",
                                worker_id,
                                key,
                                now,
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err.as_report(),
                                "Failed to delete expired worker {} {:#?}, current timestamp {}",
                                worker_id,
                                key,
                                now,
                            );
                        }
                    }
                }
            }
        });
        (join_handle, shutdown_tx)
    }

    /// Get live nodes with the specified type and state.
    /// # Arguments
    /// * `worker_type` `WorkerType` of the nodes if it is not None.
    /// * `worker_state` Filter by this state if it is not None.
    pub async fn list_worker_node(
        &self,
        worker_type: Option<WorkerType>,
        worker_state: Option<State>,
    ) -> Vec<WorkerNode> {
        let core = self.core.read().await;
        core.list_worker_node(worker_type, worker_state)
    }

    pub async fn subscribe_active_streaming_compute_nodes(
        &self,
    ) -> (Vec<WorkerNode>, UnboundedReceiver<LocalNotification>) {
        let core = self.core.read().await;
        let worker_nodes = core.list_streaming_worker_node(Some(State::Running));
        let (tx, rx) = unbounded_channel();

        // insert before release the read lock to ensure that we don't lose any update in between
        self.env
            .notification_manager()
            .insert_local_sender(tx)
            .await;
        drop(core);
        (worker_nodes, rx)
    }

    /// A convenient method to get all running compute nodes that may have running actors on them
    /// i.e. CNs which are running
    pub async fn list_active_streaming_compute_nodes(&self) -> Vec<WorkerNode> {
        let core = self.core.read().await;
        core.list_streaming_worker_node(Some(State::Running))
    }

    /// Get the cluster info used for scheduling a streaming job, containing all nodes that are
    /// running and schedulable
    pub async fn list_active_serving_compute_nodes(&self) -> Vec<WorkerNode> {
        let core = self.core.read().await;
        core.list_serving_worker_node(Some(State::Running))
    }

    /// Get the cluster info used for scheduling a streaming job.
    pub async fn get_streaming_cluster_info(&self) -> StreamingClusterInfo {
        let core = self.core.read().await;
        core.get_streaming_cluster_info()
    }

    fn parse_property(
        &self,
        worker_type: WorkerType,
        worker_property: AddNodeProperty,
    ) -> Option<Property> {
        if worker_type == WorkerType::ComputeNode {
            Some(Property {
                is_streaming: worker_property.is_streaming,
                is_serving: worker_property.is_serving,
                is_unschedulable: worker_property.is_unschedulable,
            })
        } else {
            None
        }
    }

    /// Generate `parallel_degree` parallel units.
    async fn generate_cn_parallel_units(
        &self,
        parallel_degree: usize,
        worker_id: WorkerId,
    ) -> MetaResult<Vec<ParallelUnit>> {
        let start_id = self
            .env
            .id_gen_manager()
            .as_kv()
            .generate_interval::<{ IdCategory::ParallelUnit }>(parallel_degree as u64)
            .await? as ParallelUnitId;
        let parallel_units = (start_id..start_id + parallel_degree as ParallelUnitId)
            .map(|id| ParallelUnit {
                id,
                worker_node_id: worker_id,
            })
            .collect();
        Ok(parallel_units)
    }

    pub async fn get_worker_by_id(&self, worker_id: WorkerId) -> Option<Worker> {
        self.core.read().await.get_worker_by_id(worker_id)
    }
}

/// The cluster info used for scheduling a streaming job.
#[derive(Debug, Clone)]
pub struct StreamingClusterInfo {
    /// All **active** compute nodes in the cluster.
    pub worker_nodes: HashMap<u32, WorkerNode>,

    /// All parallel units of the **active** compute nodes in the cluster.
    pub parallel_units: HashMap<ParallelUnitId, ParallelUnit>,

    /// All unschedulable parallel units of compute nodes in the cluster.
    pub unschedulable_parallel_units: HashMap<ParallelUnitId, ParallelUnit>,
}

pub struct ClusterManagerCore {
    env: MetaSrvEnv,
    /// Record for workers in the cluster.
    workers: HashMap<WorkerKey, Worker>,
    /// Record for tracking available machine ids, one is available.
    available_transactional_ids: VecDeque<u32>,
    /// Used as timestamp when meta node starts in sec.
    started_at: u64,
}

impl ClusterManagerCore {
    pub const MAX_WORKER_REUSABLE_ID_BITS: usize = 10;
    pub const MAX_WORKER_REUSABLE_ID_COUNT: usize = 1 << Self::MAX_WORKER_REUSABLE_ID_BITS;

    async fn new(env: MetaSrvEnv) -> MetaResult<Self> {
        let meta_store = env.meta_store().as_kv();
        let mut workers = Worker::list(meta_store).await?;

        let used_transactional_ids: HashSet<_> = workers
            .iter()
            .flat_map(|w| w.worker_node.transactional_id)
            .collect();

        let mut available_transactional_ids: VecDeque<_> = (0..Self::MAX_WORKER_REUSABLE_ID_COUNT
            as u32)
            .filter(|id| !used_transactional_ids.contains(id))
            .collect();

        let mut txn = Transaction::default();
        let mut var_txns = vec![];

        for worker in &mut workers {
            let worker_type = worker.worker_node.get_type().unwrap();

            if worker.worker_node.transactional_id.is_none()
                && (worker_type == WorkerType::ComputeNode || worker_type == WorkerType::Frontend)
            {
                let worker_id = worker.worker_node.id;

                let transactional_id = match available_transactional_ids.pop_front() {
                    None => {
                        return Err(MetaError::unavailable(
                            "no available transactional id for worker",
                        ));
                    }
                    Some(id) => id,
                };

                let mut var_txn = VarTransaction::new(worker);
                var_txn.worker_node.transactional_id = Some(transactional_id);

                tracing::info!(
                    "assigning transactional id {} to worker node {}",
                    transactional_id,
                    worker_id
                );

                var_txn.apply_to_txn(&mut txn).await?;
                var_txns.push(var_txn);
            }
        }

        meta_store.txn(txn).await?;

        for var_txn in var_txns {
            var_txn.commit();
        }

        Ok(Self {
            env,
            workers: workers
                .into_iter()
                .map(|w| (WorkerKey(w.key().unwrap()), w))
                .collect(),
            available_transactional_ids,
            started_at: timestamp_now_sec(),
        })
    }

    /// If no worker exists, return an error.
    fn get_worker_by_host_checked(&self, host_address: HostAddress) -> MetaResult<Worker> {
        self.get_worker_by_host(host_address)
            .ok_or_else(|| anyhow::anyhow!("Worker node does not exist!").into())
    }

    pub fn get_worker_by_host(&self, host_address: HostAddress) -> Option<Worker> {
        self.workers.get(&WorkerKey(host_address)).cloned()
    }

    pub fn get_worker_by_host_mut(&mut self, host_address: HostAddress) -> Option<&mut Worker> {
        self.workers.get_mut(&WorkerKey(host_address))
    }

    fn get_worker_by_id(&self, id: WorkerId) -> Option<Worker> {
        self.workers
            .iter()
            .find(|(_, worker)| worker.worker_id() == id)
            .map(|(_, worker)| worker.clone())
    }

    fn add_worker_node(&mut self, worker: Worker) {
        if let Some(transactional_id) = worker.worker_node.transactional_id {
            self.available_transactional_ids
                .retain(|id| *id != transactional_id);
        }

        self.workers
            .insert(WorkerKey(worker.key().unwrap()), worker);
    }

    fn update_worker_node(&mut self, worker: Worker) {
        self.workers
            .insert(WorkerKey(worker.key().unwrap()), worker);
    }

    fn delete_worker_node(&mut self, worker: Worker) {
        self.workers.remove(&WorkerKey(worker.key().unwrap()));

        if let Some(transactional_id) = worker.worker_node.transactional_id {
            self.available_transactional_ids.push_back(transactional_id);
        }
    }

    pub fn list_worker_node(
        &self,
        worker_type: Option<WorkerType>,
        worker_state: Option<State>,
    ) -> Vec<WorkerNode> {
        let worker_state = worker_state.map(|worker_state| worker_state as i32);
        self.workers
            .values()
            .map(|worker| WorkerNode {
                resource: worker.resource.to_owned(),
                started_at: worker.started_at,
                ..worker.to_protobuf()
            })
            .chain(iter::once(meta_node_info(
                &self.env.opts.advertise_addr,
                Some(self.started_at),
            )))
            .filter(|w| match worker_type {
                None => true,
                Some(worker_type) => w.r#type == worker_type as i32,
            })
            .filter(|w| match worker_state {
                None => true,
                Some(state) => state == w.state,
            })
            .collect()
    }

    pub fn list_streaming_worker_node(&self, worker_state: Option<State>) -> Vec<WorkerNode> {
        self.list_worker_node(Some(WorkerType::ComputeNode), worker_state)
            .into_iter()
            .filter(|w| w.property.as_ref().map_or(false, |p| p.is_streaming))
            .collect()
    }

    // List all parallel units on running nodes
    pub fn list_serving_worker_node(&self, worker_state: Option<State>) -> Vec<WorkerNode> {
        self.list_worker_node(Some(WorkerType::ComputeNode), worker_state)
            .into_iter()
            .filter(|w| w.property.as_ref().map_or(false, |p| p.is_serving))
            .collect()
    }

    // Lists active worker nodes
    fn get_streaming_cluster_info(&self) -> StreamingClusterInfo {
        let mut streaming_worker_node = self.list_streaming_worker_node(Some(State::Running));

        let unschedulable_worker_node = streaming_worker_node
            .extract_if(|worker| {
                worker
                    .property
                    .as_ref()
                    .map_or(false, |p| p.is_unschedulable)
            })
            .collect_vec();

        let active_workers: HashMap<_, _> = streaming_worker_node
            .into_iter()
            .map(|w| (w.id, w))
            .collect();

        let active_parallel_units = active_workers
            .values()
            .flat_map(|worker| worker.parallel_units.iter().map(|p| (p.id, p.clone())))
            .collect();

        let unschedulable_parallel_units = unschedulable_worker_node
            .iter()
            .flat_map(|worker| worker.parallel_units.iter().map(|p| (p.id, p.clone())))
            .collect();

        StreamingClusterInfo {
            worker_nodes: active_workers,
            parallel_units: active_parallel_units,
            unschedulable_parallel_units,
        }
    }

    fn count_worker_node(&self) -> HashMap<WorkerType, u64> {
        const MONITORED_WORKER_TYPES: [WorkerType; 4] = [
            WorkerType::Compactor,
            WorkerType::ComputeNode,
            WorkerType::Frontend,
            WorkerType::Meta,
        ];
        let mut ret = HashMap::new();
        self.workers
            .values()
            .map(|worker| worker.worker_type())
            .filter(|worker_type| MONITORED_WORKER_TYPES.contains(worker_type))
            .for_each(|worker_type| {
                ret.entry(worker_type)
                    .and_modify(|worker_num| *worker_num += 1)
                    .or_insert(1);
            });
        // Make sure all the monitored worker types exist in the map.
        for wt in MONITORED_WORKER_TYPES {
            ret.entry(wt).or_insert(0);
        }
        ret
    }
}

fn timestamp_now_sec() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("Clock may have gone backwards")
        .as_secs()
}

fn meta_node_info(host: &str, started_at: Option<u64>) -> WorkerNode {
    WorkerNode {
        id: META_NODE_ID,
        r#type: WorkerType::Meta as _,
        host: HostAddr::try_from(host)
            .as_ref()
            .map(HostAddr::to_protobuf)
            .ok(),
        state: State::Running as _,
        parallel_units: vec![],
        property: None,
        transactional_id: None,
        resource: Some(risingwave_pb::common::worker_node::Resource {
            rw_version: RW_VERSION.to_string(),
            total_memory_bytes: system_memory_available_bytes() as _,
            total_cpu_cores: total_cpu_available() as _,
        }),
        started_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cluster_manager() -> MetaResult<()> {
        let env = MetaSrvEnv::for_test().await;

        let cluster_manager = Arc::new(
            ClusterManager::new(env.clone(), Duration::new(0, 0))
                .await
                .unwrap(),
        );

        let mut worker_nodes = Vec::new();
        let worker_count = 5usize;
        let fake_parallelism: usize = 4;
        for i in 0..worker_count {
            let fake_host_address = HostAddress {
                host: "localhost".to_string(),
                port: 5000 + i as i32,
            };
            let worker_node = cluster_manager
                .add_worker_node(
                    WorkerType::ComputeNode,
                    fake_host_address,
                    AddNodeProperty {
                        worker_node_parallelism: fake_parallelism as _,
                        is_streaming: true,
                        is_serving: true,
                        is_unschedulable: false,
                    },
                    Default::default(),
                )
                .await
                .unwrap();
            worker_nodes.push(worker_node);
        }

        // Since no worker is active, the parallel unit count should be 0.
        assert_cluster_manager(&cluster_manager, 0).await;

        for worker_node in worker_nodes {
            cluster_manager
                .activate_worker_node(worker_node.get_host().unwrap().clone())
                .await
                .unwrap();
        }

        let worker_count_map = cluster_manager.core.read().await.count_worker_node();
        assert_eq!(
            *worker_count_map.get(&WorkerType::ComputeNode).unwrap() as usize,
            worker_count
        );

        let parallel_count = fake_parallelism * worker_count;
        assert_cluster_manager(&cluster_manager, parallel_count).await;

        // re-register existing worker node with larger parallelism.
        let fake_host_address = HostAddress {
            host: "localhost".to_string(),
            port: 5000,
        };
        let worker_node = cluster_manager
            .add_worker_node(
                WorkerType::ComputeNode,
                fake_host_address,
                AddNodeProperty {
                    worker_node_parallelism: (fake_parallelism + 4) as u64,
                    is_streaming: true,
                    is_serving: true,
                    is_unschedulable: false,
                },
                Default::default(),
            )
            .await
            .unwrap();
        assert_eq!(worker_node.parallel_units.len(), fake_parallelism + 4);
        assert_cluster_manager(&cluster_manager, parallel_count + 4).await;

        // re-register existing worker node with smaller parallelism.
        let fake_host_address = HostAddress {
            host: "localhost".to_string(),
            port: 5000,
        };
        let worker_node = cluster_manager
            .add_worker_node(
                WorkerType::ComputeNode,
                fake_host_address,
                AddNodeProperty {
                    worker_node_parallelism: (fake_parallelism - 2) as u64,
                    is_streaming: true,
                    is_serving: true,
                    is_unschedulable: false,
                },
                Default::default(),
            )
            .await
            .unwrap();

        if !env.opts.disable_automatic_parallelism_control {
            assert_eq!(worker_node.parallel_units.len(), fake_parallelism - 2);
            assert_cluster_manager(&cluster_manager, parallel_count - 2).await;
        } else {
            // compatibility mode
            assert_eq!(worker_node.parallel_units.len(), fake_parallelism + 4);
            assert_cluster_manager(&cluster_manager, parallel_count + 4).await;
        }

        let worker_to_delete_count = 4usize;
        for i in 0..worker_to_delete_count {
            let fake_host_address = HostAddress {
                host: "localhost".to_string(),
                port: 5000 + i as i32,
            };
            cluster_manager
                .delete_worker_node(fake_host_address)
                .await
                .unwrap();
        }
        assert_cluster_manager(&cluster_manager, fake_parallelism).await;

        Ok(())
    }

    #[tokio::test]
    async fn test_cluster_manager_schedulability() -> MetaResult<()> {
        let env = MetaSrvEnv::for_test().await;

        let cluster_manager =
            Arc::new(ClusterManager::new(env, Duration::new(0, 0)).await.unwrap());
        let worker_node = cluster_manager
            .add_worker_node(
                WorkerType::ComputeNode,
                HostAddress {
                    host: "127.0.0.1".to_string(),
                    port: 1,
                },
                AddNodeProperty {
                    worker_node_parallelism: 1,
                    is_streaming: true,
                    is_serving: true,
                    is_unschedulable: false,
                },
                Default::default(),
            )
            .await
            .unwrap();

        assert!(!worker_node.property.as_ref().unwrap().is_unschedulable);

        cluster_manager
            .activate_worker_node(worker_node.get_host().unwrap().clone())
            .await
            .unwrap();

        cluster_manager
            .update_schedulability(vec![worker_node.id], Schedulability::Unschedulable)
            .await
            .unwrap();

        let worker_nodes = cluster_manager.list_active_streaming_compute_nodes().await;

        let worker_node = &worker_nodes[0];

        assert!(worker_node.property.as_ref().unwrap().is_unschedulable);

        Ok(())
    }

    async fn assert_cluster_manager(cluster_manager: &ClusterManager, parallel_count: usize) {
        let parallel_units = cluster_manager
            .list_active_serving_compute_nodes()
            .await
            .into_iter()
            .flat_map(|w| w.parallel_units)
            .collect_vec();
        assert_eq!(parallel_units.len(), parallel_count);
    }

    // This test takes seconds because the TTL is measured in seconds.
    #[cfg(madsim)]
    #[tokio::test]
    async fn test_heartbeat() {
        use crate::hummock::test_utils::setup_compute_env;
        let (_env, _hummock_manager, cluster_manager, worker_node) = setup_compute_env(1).await;
        let context_id_1 = worker_node.id;
        let fake_host_address_2 = HostAddress {
            host: "127.0.0.1".to_string(),
            port: 2,
        };
        let fake_parallelism = 4;
        let _worker_node_2 = cluster_manager
            .add_worker_node(
                WorkerType::ComputeNode,
                fake_host_address_2,
                AddNodeProperty {
                    worker_node_parallelism: fake_parallelism as _,
                    is_streaming: true,
                    is_serving: true,
                    is_unschedulable: false,
                },
                Default::default(),
            )
            .await
            .unwrap();
        // Two live nodes
        assert_eq!(
            cluster_manager
                .list_worker_node(Some(WorkerType::ComputeNode), None)
                .await
                .len(),
            2
        );

        let ttl = cluster_manager.max_heartbeat_interval;
        let check_interval = std::cmp::min(Duration::from_millis(100), ttl / 4);

        // Keep worker 1 alive
        let cluster_manager_ref = cluster_manager.clone();
        let keep_alive_join_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(cluster_manager_ref.max_heartbeat_interval / 3).await;
                cluster_manager_ref
                    .heartbeat(context_id_1, vec![])
                    .await
                    .unwrap();
            }
        });

        tokio::time::sleep(ttl * 2 + check_interval).await;

        // One node has actually expired but still got two, because heartbeat check is not
        // started.
        assert_eq!(
            cluster_manager
                .list_worker_node(Some(WorkerType::ComputeNode), None)
                .await
                .len(),
            2
        );

        let (join_handle, shutdown_sender) =
            ClusterManager::start_heartbeat_checker(cluster_manager.clone(), check_interval);
        tokio::time::sleep(ttl * 2 + check_interval).await;

        // One live node left.
        assert_eq!(
            cluster_manager
                .list_worker_node(Some(WorkerType::ComputeNode), None)
                .await
                .len(),
            1
        );

        shutdown_sender.send(()).unwrap();
        join_handle.await.unwrap();
        keep_alive_join_handle.abort();
    }
}
