// Copyright 2023 RisingWave Labs
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

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use itertools::Itertools;
use risingwave_hummock_sdk::can_concat;
use risingwave_hummock_sdk::key::FullKey;
use risingwave_hummock_sdk::key_range::{KeyRange, KeyRangeCommon};
use risingwave_pb::hummock::{CompactTask, LevelType};

use super::compaction_utils::estimate_task_memory_capacity;
use super::task_progress::TaskProgress;
use super::TaskConfig;
use crate::filter_key_extractor::FilterKeyExtractorImpl;
use crate::hummock::compactor::iterator::ConcatSstableIterator;
use crate::hummock::compactor::{CompactOutput, CompactionFilter, Compactor, CompactorContext};
use crate::hummock::iterator::{Forward, HummockIterator, UnorderedMergeIteratorInner};
use crate::hummock::sstable::CompactionDeleteRangesBuilder;
use crate::hummock::{
    create_tombstones_to_represent_monotonic_deletes, CachePolicy, CompactionDeleteRanges,
    CompressionAlgorithm, HummockResult, SstableBuilderOptions, SstableStoreRef,
};
use crate::monitor::StoreLocalStatistic;

pub struct CompactorRunner {
    compact_task: CompactTask,
    compactor: Compactor,
    sstable_store: SstableStoreRef,
    key_range: KeyRange,
    split_index: usize,
}

impl CompactorRunner {
    pub fn new(split_index: usize, context: Arc<CompactorContext>, task: CompactTask) -> Self {
        let mut options: SstableBuilderOptions = context.storage_opts.as_ref().into();
        options.compression_algorithm = match task.compression_algorithm {
            0 => CompressionAlgorithm::None,
            1 => CompressionAlgorithm::Lz4,
            _ => CompressionAlgorithm::Zstd,
        };
        options.capacity = estimate_task_memory_capacity(context.clone(), &task);

        let key_range = KeyRange {
            left: Bytes::copy_from_slice(task.splits[split_index].get_left()),
            right: Bytes::copy_from_slice(task.splits[split_index].get_right()),
            right_exclusive: true,
        };

        let compactor = Compactor::new(
            context.clone(),
            options,
            TaskConfig {
                key_range: key_range.clone(),
                cache_policy: CachePolicy::NotFill,
                gc_delete_keys: task.gc_delete_keys,
                watermark: task.watermark,
                stats_target_table_ids: Some(HashSet::from_iter(task.existing_table_ids.clone())),
                task_type: task.task_type(),
                split_by_table: task.split_by_state_table,
            },
        );

        Self {
            compactor,
            compact_task: task,
            sstable_store: context.sstable_store.clone(),
            key_range,
            split_index,
        }
    }

    pub async fn run(
        &self,
        compaction_filter: impl CompactionFilter,
        filter_key_extractor: Arc<FilterKeyExtractorImpl>,
        del_agg: Arc<CompactionDeleteRanges>,
        task_progress: Arc<TaskProgress>,
    ) -> HummockResult<CompactOutput> {
        let iter = self.build_sst_iter()?;
        let (ssts, compaction_stat) = self
            .compactor
            .compact_key_range(
                iter,
                compaction_filter,
                del_agg,
                filter_key_extractor,
                Some(task_progress),
            )
            .await?;
        Ok((self.split_index, ssts, compaction_stat))
    }

    pub async fn build_delete_range_iter<F: CompactionFilter>(
        compact_task: &CompactTask,
        sstable_store: &SstableStoreRef,
        filter: &mut F,
    ) -> HummockResult<Arc<CompactionDeleteRanges>> {
        let mut builder = CompactionDeleteRangesBuilder::default();
        let mut local_stats = StoreLocalStatistic::default();
        for level in &compact_task.input_ssts {
            if level.table_infos.is_empty() {
                continue;
            }

            for table_info in &level.table_infos {
                let table = sstable_store.sstable(table_info, &mut local_stats).await?;
                let mut range_tombstone_list = create_tombstones_to_represent_monotonic_deletes(
                    &table.value().meta.monotonic_tombstone_events,
                );
                range_tombstone_list.retain(|tombstone| {
                    !filter.should_delete(FullKey::from_user_key(
                        tombstone.start_user_key.as_ref(),
                        tombstone.sequence,
                    ))
                });
                builder.add_tombstone(range_tombstone_list);
            }
        }
        let aggregator = builder.build_for_compaction(compact_task.gc_delete_keys);
        Ok(aggregator)
    }

    /// Build the merge iterator based on the given input ssts.
    fn build_sst_iter(&self) -> HummockResult<impl HummockIterator<Direction = Forward>> {
        let mut table_iters = Vec::new();

        for level in &self.compact_task.input_ssts {
            if level.table_infos.is_empty() {
                continue;
            }

            // Do not need to filter the table because manager has done it.
            if level.level_type == LevelType::Nonoverlapping as i32 {
                debug_assert!(can_concat(&level.table_infos));
                let tables = level
                    .table_infos
                    .iter()
                    .filter(|info| {
                        let key_range = KeyRange::from(info.key_range.as_ref().unwrap());
                        self.key_range.full_key_overlap(&key_range)
                    })
                    .cloned()
                    .collect_vec();
                table_iters.push(ConcatSstableIterator::new(
                    self.compact_task.existing_table_ids.clone(),
                    tables,
                    self.compactor.task_config.key_range.clone(),
                    self.sstable_store.clone(),
                ));
            } else {
                for table_info in &level.table_infos {
                    let key_range = KeyRange::from(table_info.key_range.as_ref().unwrap());
                    if !self.key_range.full_key_overlap(&key_range) {
                        continue;
                    }
                    table_iters.push(ConcatSstableIterator::new(
                        self.compact_task.existing_table_ids.clone(),
                        vec![table_info.clone()],
                        self.compactor.task_config.key_range.clone(),
                        self.sstable_store.clone(),
                    ));
                }
            }
        }
        Ok(UnorderedMergeIteratorInner::for_compactor(table_iters))
    }
}
#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use risingwave_common::catalog::TableId;
    use risingwave_hummock_sdk::key::UserKey;
    use risingwave_pb::hummock::InputLevel;

    use super::*;
    use crate::hummock::compactor::StateCleanUpCompactionFilter;
    use crate::hummock::iterator::test_utils::mock_sstable_store;
    use crate::hummock::test_utils::{
        default_builder_opt_for_test, gen_test_sstable_with_range_tombstone,
    };
    use crate::hummock::{create_monotonic_events, DeleteRangeTombstone};

    #[tokio::test]
    async fn test_delete_range_aggregator_with_filter() {
        let sstable_store = mock_sstable_store();
        let kv_pairs = vec![];
        let range_tombstones = vec![
            DeleteRangeTombstone::new(TableId::new(1), b"abc".to_vec(), b"cde".to_vec(), 1),
            DeleteRangeTombstone::new(TableId::new(2), b"abc".to_vec(), b"def".to_vec(), 1),
        ];
        let sstable_info = gen_test_sstable_with_range_tombstone(
            default_builder_opt_for_test(),
            1,
            kv_pairs.into_iter(),
            range_tombstones.clone(),
            sstable_store.clone(),
        )
        .await
        .get_sstable_info();
        let compact_task = CompactTask {
            input_ssts: vec![InputLevel {
                level_idx: 0,
                level_type: 0,
                table_infos: vec![sstable_info],
            }],
            existing_table_ids: vec![2],
            ..Default::default()
        };
        let mut state_clean_up_filter = StateCleanUpCompactionFilter::new(HashSet::from_iter(
            compact_task.existing_table_ids.clone(),
        ));
        let collector = CompactorRunner::build_delete_range_iter(
            &compact_task,
            &sstable_store,
            &mut state_clean_up_filter,
        )
        .await
        .unwrap();
        let ret = collector.get_tombstone_between(
            UserKey::<Bytes>::default().as_ref(),
            UserKey::<Bytes>::default().as_ref(),
        );
        assert_eq!(
            ret,
            create_monotonic_events(&vec![range_tombstones[1].clone()])
        );
    }
}
