use std::collections::HashMap;

use risingwave_pb::common::WorkerNode;

use crate::cluster::NodeId;
use crate::model::ActorId;
use crate::stream::ActorInfos;

/// [`BarrierActorInfo`] resolves the actor info read from meta store for [`BarrierManager`].
pub struct BarrierActorInfo {
    /// node_id => node
    pub node_map: HashMap<NodeId, WorkerNode>,

    /// node_id => actors
    pub actor_map: HashMap<NodeId, Vec<ActorId>>,

    /// node_id => source actors
    pub actor_map_to_send: HashMap<NodeId, Vec<ActorId>>,
}

impl BarrierActorInfo {
    // TODO: we may resolve this info as graph updating, instead of doing it every time we want to
    //  send a barrier
    pub fn resolve(all_nodes: &[WorkerNode], actor_infos: ActorInfos) -> Self {
        let node_map = all_nodes
            .iter()
            .map(|node| (node.id, node.clone()))
            .collect::<HashMap<_, _>>();

        Self {
            node_map,
            actor_map: actor_infos.actor_maps,
            actor_map_to_send: actor_infos.source_actor_maps,
        }
    }

    // TODO: should only collect from reachable actors, for mv on mv
    pub fn actor_ids_to_collect(&self, node_id: &NodeId) -> Option<impl Iterator<Item = ActorId>> {
        self.actor_map
            .get(node_id)
            .map(|actor_ids| actor_ids.clone().into_iter())
    }

    pub fn actor_ids_to_send(&self, node_id: &NodeId) -> Option<impl Iterator<Item = ActorId>> {
        self.actor_map_to_send
            .get(node_id)
            .map(|actor_ids| actor_ids.clone().into_iter())
    }
}
