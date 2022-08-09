// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::ops::DerefMut;

use function_name::named;
use itertools::Itertools;
use risingwave_hummock_sdk::HummockContextId;

use crate::hummock::error::{Error, Result};
use crate::hummock::manager::{
    commit_multi_var, read_lock, start_measure_real_process_timer, write_lock,
};
use crate::hummock::HummockManager;
use crate::model::{BTreeMapTransaction, ValTransaction};
use crate::storage::{MetaStore, Transaction};

impl<S> HummockManager<S>
where
    S: MetaStore,
{
    /// Release resources pinned by these contexts, including:
    /// - Version
    /// - Snapshot
    /// - Compaction task
    #[named]
    pub async fn release_contexts(
        &self,
        context_ids: impl AsRef<[HummockContextId]>,
    ) -> Result<()> {
        let mut compaction_guard = write_lock!(self, compaction).await;
        let compaction = compaction_guard.deref_mut();
        let mut compact_statuses = BTreeMapTransaction::new(&mut compaction.compaction_statuses);
        let mut compact_task_assignment =
            BTreeMapTransaction::new(&mut compaction.compact_task_assignment);
        let mut versioning_guard = write_lock!(self, versioning).await;
        let versioning = versioning_guard.deref_mut();
        let mut pinned_versions = BTreeMapTransaction::new(&mut versioning.pinned_versions);
        let mut pinned_snapshots = BTreeMapTransaction::new(&mut versioning.pinned_snapshots);
        for context_id in context_ids.as_ref() {
            tracing::debug!("Release context {}", *context_id);
            for assignment in compact_task_assignment.tree_ref().values() {
                if assignment.context_id != *context_id {
                    continue;
                }
                let task = assignment
                    .compact_task
                    .as_ref()
                    .expect("compact_task shouldn't be None");
                let mut compact_status = compact_statuses
                    .get_mut(task.compaction_group_id)
                    .ok_or(Error::InvalidCompactionGroup(task.compaction_group_id))?;
                compact_status.report_compact_task(
                    assignment
                        .compact_task
                        .as_ref()
                        .expect("compact_task shouldn't be None"),
                );
            }
            let task_ids_to_remove = compact_task_assignment
                .tree_ref()
                .iter()
                .filter_map(|(task_id, v)| {
                    if v.context_id == *context_id {
                        Some(*task_id)
                    } else {
                        None
                    }
                })
                .collect_vec();
            for task_id in task_ids_to_remove {
                compact_task_assignment.remove(task_id);
            }
            pinned_versions.remove(*context_id);
            pinned_snapshots.remove(*context_id);
        }
        commit_multi_var!(
            self,
            None,
            compact_statuses,
            compact_task_assignment,
            pinned_versions,
            pinned_snapshots
        )?;

        #[cfg(test)]
        {
            drop(versioning_guard);
            drop(compaction_guard);
            self.check_state_consistency().await;
        }

        Ok(())
    }

    /// Checks whether `context_id` is valid.
    pub async fn check_context(&self, context_id: HummockContextId) -> bool {
        self.cluster_manager
            .get_worker_by_id(context_id)
            .await
            .is_some()
    }

    /// Release invalid contexts, aka worker node ids which are no longer valid in `ClusterManager`.
    #[named]
    pub(super) async fn release_invalid_contexts(&self) -> Result<Vec<HummockContextId>> {
        let active_context_ids = {
            let compaction_guard = read_lock!(self, compaction).await;
            let versioning_guard = read_lock!(self, versioning).await;
            let _timer = start_measure_real_process_timer!(self);
            let mut active_context_ids = HashSet::new();
            active_context_ids.extend(
                compaction_guard
                    .compact_task_assignment
                    .values()
                    .map(|c| c.context_id),
            );
            active_context_ids.extend(versioning_guard.pinned_versions.keys());
            active_context_ids.extend(versioning_guard.pinned_snapshots.keys());
            active_context_ids
        };

        let mut invalid_context_ids = vec![];
        for active_context_id in &active_context_ids {
            if !self.check_context(*active_context_id).await {
                invalid_context_ids.push(*active_context_id);
            }
        }

        self.release_contexts(&invalid_context_ids).await?;

        Ok(invalid_context_ids)
    }
}