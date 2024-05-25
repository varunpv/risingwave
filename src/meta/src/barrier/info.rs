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

use std::collections::{HashMap, HashSet};

use risingwave_common::catalog::TableId;
use risingwave_pb::common::PbWorkerNode;
use tracing::warn;

use crate::barrier::Command;
use crate::manager::{ActiveStreamingWorkerNodes, ActorInfos, WorkerId};
use crate::model::ActorId;

#[derive(Debug, Clone)]
pub struct ActorDesc {
    pub id: ActorId,
    pub node_id: WorkerId,
    pub is_injectable: bool,
}

#[derive(Debug, Clone)]
pub struct CommandActorChanges {
    pub(crate) to_add: Vec<ActorDesc>,
    pub(crate) to_remove: HashSet<ActorId>,
}

/// [`InflightActorInfo`] resolves the actor info read from meta store for
/// [`crate::barrier::GlobalBarrierManager`].
#[derive(Default, Clone)]
pub struct InflightActorInfo {
    /// `node_id` => node
    pub node_map: HashMap<WorkerId, PbWorkerNode>,

    /// `node_id` => actors
    pub actor_map: HashMap<WorkerId, HashSet<ActorId>>,

    /// `node_id` => barrier inject actors
    pub actor_map_to_send: HashMap<WorkerId, HashSet<ActorId>>,

    /// `actor_id` => `WorkerId`
    pub actor_location_map: HashMap<ActorId, WorkerId>,

    /// `mv_table_id` => `subscription_id` => retention seconds
    pub mv_depended_subscriptions: HashMap<TableId, HashMap<u32, u64>>,
}

impl InflightActorInfo {
    /// Resolve inflight actor info from given nodes and actors that are loaded from meta store. It will be used during recovery to rebuild all streaming actors.
    pub fn resolve(
        active_nodes: &ActiveStreamingWorkerNodes,
        actor_infos: ActorInfos,
        mv_depended_subscriptions: HashMap<TableId, HashMap<u32, u64>>,
    ) -> Self {
        let node_map = active_nodes.current().clone();

        let actor_map = actor_infos
            .actor_maps
            .into_iter()
            .map(|(node_id, actor_ids)| (node_id, actor_ids.into_iter().collect::<HashSet<_>>()))
            .collect::<HashMap<_, _>>();

        let actor_map_to_send = actor_infos
            .barrier_inject_actor_maps
            .into_iter()
            .map(|(node_id, actor_ids)| (node_id, actor_ids.into_iter().collect::<HashSet<_>>()))
            .collect::<HashMap<_, _>>();

        let actor_location_map = actor_map
            .iter()
            .flat_map(|(node_id, actor_ids)| actor_ids.iter().map(|actor_id| (*actor_id, *node_id)))
            .collect::<HashMap<_, _>>();

        Self {
            node_map,
            actor_map,
            actor_map_to_send,
            actor_location_map,
            mv_depended_subscriptions,
        }
    }

    /// Update worker nodes snapshot. We need to support incremental updates for it in the future.
    pub fn resolve_worker_nodes(&mut self, all_nodes: impl IntoIterator<Item = PbWorkerNode>) {
        let new_node_map = all_nodes
            .into_iter()
            .map(|node| (node.id, node))
            .collect::<HashMap<_, _>>();
        for (actor_id, location) in &self.actor_location_map {
            if !new_node_map.contains_key(location) {
                warn!(actor_id, location, node = ?self.node_map.get(location), "node with running actors is deleted");
            }
        }
        self.node_map = new_node_map;
    }

    /// Apply some actor changes before issuing a barrier command, if the command contains any new added actors, we should update
    /// the info correspondingly.
    pub fn pre_apply(&mut self, command: &Command) {
        if let Some(CommandActorChanges { to_add, .. }) = command.actor_changes() {
            for actor_desc in to_add {
                assert!(self.node_map.contains_key(&actor_desc.node_id));
                assert!(
                    self.actor_map
                        .entry(actor_desc.node_id)
                        .or_default()
                        .insert(actor_desc.id),
                    "duplicate actor in command changes"
                );
                if actor_desc.is_injectable {
                    assert!(
                        self.actor_map_to_send
                            .entry(actor_desc.node_id)
                            .or_default()
                            .insert(actor_desc.id),
                        "duplicate actor in command changes"
                    );
                }
                assert!(
                    self.actor_location_map
                        .insert(actor_desc.id, actor_desc.node_id)
                        .is_none(),
                    "duplicate actor in command changes"
                );
            }
        };
        if let Command::CreateSubscription {
            subscription_id,
            upstream_mv_table_id,
            retention_second,
        } = command
        {
            if let Some(prev_retiontion) = self
                .mv_depended_subscriptions
                .entry(*upstream_mv_table_id)
                .or_default()
                .insert(*subscription_id, *retention_second)
            {
                warn!(subscription_id, ?upstream_mv_table_id, mv_depended_subscriptions = ?self.mv_depended_subscriptions, prev_retiontion, "add an existing subscription id");
            }
        }
    }

    /// Apply some actor changes after the barrier command is collected, if the command contains any actors that are dropped, we should
    /// remove that from the snapshot correspondingly.
    pub fn post_apply(&mut self, command: &Command) {
        if let Some(CommandActorChanges { to_remove, .. }) = command.actor_changes() {
            for actor_id in to_remove {
                let node_id = self
                    .actor_location_map
                    .remove(&actor_id)
                    .expect("actor not found");
                let actor_ids = self.actor_map.get_mut(&node_id).expect("node not found");
                assert!(actor_ids.remove(&actor_id), "actor not found");
                self.actor_map_to_send
                    .get_mut(&node_id)
                    .map(|actor_ids| actor_ids.remove(&actor_id));
            }
            self.actor_map.retain(|_, actor_ids| !actor_ids.is_empty());
            self.actor_map_to_send
                .retain(|_, actor_ids| !actor_ids.is_empty());
        }
        if let Command::DropSubscription {
            subscription_id,
            upstream_mv_table_id,
        } = command
        {
            let removed = match self.mv_depended_subscriptions.get_mut(upstream_mv_table_id) {
                Some(subscriptions) => {
                    let removed = subscriptions.remove(subscription_id).is_some();
                    if removed && subscriptions.is_empty() {
                        self.mv_depended_subscriptions.remove(upstream_mv_table_id);
                    }
                    removed
                }
                None => false,
            };
            if !removed {
                warn!(subscription_id, ?upstream_mv_table_id, mv_depended_subscriptions = ?self.mv_depended_subscriptions, "remove a non-existing subscription id");
            }
        }
    }

    /// Returns actor list to collect in the target worker node.
    pub fn actor_ids_to_collect(&self, node_id: &WorkerId) -> impl Iterator<Item = ActorId> {
        self.actor_map
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
    }

    /// Returns actor list to send in the target worker node.
    pub fn actor_ids_to_send(&self, node_id: &WorkerId) -> impl Iterator<Item = ActorId> {
        self.actor_map_to_send
            .get(node_id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
    }
}
