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

#![feature(async_closure)]
#![feature(extract_if)]
#![feature(hash_extract_if)]
#![feature(lint_reasons)]
#![feature(map_many_mut)]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]
#![feature(is_sorted)]
#![feature(let_chains)]
#![feature(btree_cursors)]

mod key_cmp;
use std::cmp::Ordering;
use std::collections::HashMap;

pub use key_cmp::*;
use risingwave_common::util::epoch::EPOCH_SPILL_TIME_MASK;
use risingwave_pb::common::{batch_query_epoch, BatchQueryEpoch};
use risingwave_pb::hummock::SstableInfo;

use crate::compaction_group::StaticCompactionGroupId;
use crate::key_range::KeyRangeCommon;
use crate::table_stats::{to_prost_table_stats_map, PbTableStatsMap, TableStatsMap};

pub mod change_log;
pub mod compact;
pub mod compaction_group;
pub mod key;
pub mod key_range;
pub mod prost_key_range;
pub mod table_stats;
pub mod table_watermark;
pub mod version;

pub use compact::*;
use risingwave_common::catalog::TableId;

use crate::table_watermark::TableWatermarks;

pub type HummockSstableObjectId = u64;
pub type HummockSstableId = u64;
pub type HummockRefCount = u64;
pub type HummockVersionId = u64;
pub type HummockContextId = u32;
pub type HummockEpoch = u64;
pub type HummockCompactionTaskId = u64;
pub type CompactionGroupId = u64;
pub const INVALID_VERSION_ID: HummockVersionId = 0;
pub const FIRST_VERSION_ID: HummockVersionId = 1;
pub const SPLIT_TABLE_COMPACTION_GROUP_ID_HEAD: u64 = 1u64 << 56;
pub const SINGLE_TABLE_COMPACTION_GROUP_ID_HEAD: u64 = 2u64 << 56;
pub const OBJECT_SUFFIX: &str = "data";

#[macro_export]
/// This is wrapper for `info` log.
///
/// In our CI tests, we frequently create and drop tables, and checkpoint in all barriers, which may
/// cause many events. However, these events are not expected to be frequent in production usage, so
/// we print an info log for every these events. But these events are frequent in CI, and produce
/// many logs in CI, and we may want to downgrade the log level of these event log to debug.
/// Therefore, we provide this macro to wrap the `info` log, which will produce `info` log when
/// `debug_assertions` is not enabled, and `debug` log when `debug_assertions` is enabled.
macro_rules! info_in_release {
    ($($arg:tt)*) => {
        {
            #[cfg(debug_assertions)]
            {
                use tracing::debug;
                debug!($($arg)*);
            }
            #[cfg(not(debug_assertions))]
            {
                use tracing::info;
                info!($($arg)*);
            }
        }
    }
}

#[derive(Default, Debug)]
pub struct SyncResult {
    /// The size of all synced shared buffers.
    pub sync_size: usize,
    /// The `sst_info` of sync.
    pub uncommitted_ssts: Vec<LocalSstableInfo>,
    /// The collected table watermarks written by state tables.
    pub table_watermarks: HashMap<TableId, TableWatermarks>,
    /// Sstable that holds the uncommitted old value
    pub old_value_ssts: Vec<LocalSstableInfo>,
}

#[derive(Debug, Clone)]
pub struct LocalSstableInfo {
    pub compaction_group_id: CompactionGroupId,
    pub sst_info: SstableInfo,
    pub table_stats: TableStatsMap,
}

impl LocalSstableInfo {
    pub fn new(
        compaction_group_id: CompactionGroupId,
        sst_info: SstableInfo,
        table_stats: TableStatsMap,
    ) -> Self {
        Self {
            compaction_group_id,
            sst_info,
            table_stats,
        }
    }

    pub fn with_compaction_group(
        compaction_group_id: CompactionGroupId,
        sst_info: SstableInfo,
    ) -> Self {
        Self::new(compaction_group_id, sst_info, TableStatsMap::default())
    }

    pub fn with_stats(sst_info: SstableInfo, table_stats: TableStatsMap) -> Self {
        Self::new(
            StaticCompactionGroupId::StateDefault as CompactionGroupId,
            sst_info,
            table_stats,
        )
    }

    pub fn for_test(sst_info: SstableInfo) -> Self {
        Self {
            compaction_group_id: StaticCompactionGroupId::StateDefault as CompactionGroupId,
            sst_info,
            table_stats: Default::default(),
        }
    }

    pub fn file_size(&self) -> u64 {
        self.sst_info.file_size
    }
}

#[derive(Debug, Clone)]
pub struct ExtendedSstableInfo {
    pub compaction_group_id: CompactionGroupId,
    pub sst_info: SstableInfo,
    pub table_stats: PbTableStatsMap,
}

impl ExtendedSstableInfo {
    pub fn new(
        compaction_group_id: CompactionGroupId,
        sst_info: SstableInfo,
        table_stats: PbTableStatsMap,
    ) -> Self {
        Self {
            compaction_group_id,
            sst_info,
            table_stats,
        }
    }

    pub fn with_compaction_group(
        compaction_group_id: CompactionGroupId,
        sst_info: SstableInfo,
    ) -> Self {
        Self::new(compaction_group_id, sst_info, PbTableStatsMap::default())
    }
}

impl From<LocalSstableInfo> for ExtendedSstableInfo {
    fn from(value: LocalSstableInfo) -> Self {
        Self {
            compaction_group_id: value.compaction_group_id,
            sst_info: value.sst_info,
            table_stats: to_prost_table_stats_map(value.table_stats),
        }
    }
}

impl PartialEq for LocalSstableInfo {
    fn eq(&self, other: &Self) -> bool {
        self.compaction_group_id == other.compaction_group_id && self.sst_info == other.sst_info
    }
}

/// Package read epoch of hummock, it be used for `wait_epoch`
#[derive(Debug, Clone, Copy)]
pub enum HummockReadEpoch {
    /// We need to wait the `max_committed_epoch`
    Committed(HummockEpoch),
    /// We need to wait the `max_current_epoch`
    Current(HummockEpoch),
    /// We don't need to wait epoch, we usually do stream reading with it.
    NoWait(HummockEpoch),
    /// We don't need to wait epoch.
    Backup(HummockEpoch),
}

impl From<BatchQueryEpoch> for HummockReadEpoch {
    fn from(e: BatchQueryEpoch) -> Self {
        match e.epoch.unwrap() {
            batch_query_epoch::Epoch::Committed(epoch) => HummockReadEpoch::Committed(epoch),
            batch_query_epoch::Epoch::Current(epoch) => HummockReadEpoch::Current(epoch),
            batch_query_epoch::Epoch::Backup(epoch) => HummockReadEpoch::Backup(epoch),
        }
    }
}

pub fn to_committed_batch_query_epoch(epoch: u64) -> BatchQueryEpoch {
    BatchQueryEpoch {
        epoch: Some(batch_query_epoch::Epoch::Committed(epoch)),
    }
}

impl HummockReadEpoch {
    pub fn get_epoch(&self) -> HummockEpoch {
        *match self {
            HummockReadEpoch::Committed(epoch) => epoch,
            HummockReadEpoch::Current(epoch) => epoch,
            HummockReadEpoch::NoWait(epoch) => epoch,
            HummockReadEpoch::Backup(epoch) => epoch,
        }
    }
}
pub struct SstObjectIdRange {
    // inclusive
    pub start_id: HummockSstableObjectId,
    // exclusive
    pub end_id: HummockSstableObjectId,
}

impl SstObjectIdRange {
    pub fn new(start_id: HummockSstableObjectId, end_id: HummockSstableObjectId) -> Self {
        Self { start_id, end_id }
    }

    pub fn peek_next_sst_object_id(&self) -> Option<HummockSstableObjectId> {
        if self.start_id < self.end_id {
            return Some(self.start_id);
        }
        None
    }

    /// Pops and returns next SST id.
    pub fn get_next_sst_object_id(&mut self) -> Option<HummockSstableObjectId> {
        let next_id = self.peek_next_sst_object_id();
        self.start_id += 1;
        next_id
    }
}

pub fn can_concat(ssts: &[SstableInfo]) -> bool {
    let len = ssts.len();
    for i in 1..len {
        if ssts[i - 1]
            .key_range
            .as_ref()
            .unwrap()
            .compare_right_with(&ssts[i].key_range.as_ref().unwrap().left)
            != Ordering::Less
        {
            return false;
        }
    }
    true
}

const CHECKPOINT_DIR: &str = "checkpoint";
const CHECKPOINT_NAME: &str = "0";
const ARCHIVE_DIR: &str = "archive";

pub fn version_checkpoint_path(root_dir: &str) -> String {
    format!("{}/{}/{}", root_dir, CHECKPOINT_DIR, CHECKPOINT_NAME)
}

pub fn version_archive_dir(root_dir: &str) -> String {
    format!("{}/{}", root_dir, ARCHIVE_DIR)
}

pub fn version_checkpoint_dir(checkpoint_path: &str) -> String {
    checkpoint_path.trim_end_matches(|c| c != '/').to_string()
}

/// Represents an epoch with a gap.
///
/// When a spill of the mem table occurs between two epochs, `EpochWithGap` generates an offset.
/// This offset is encoded when performing full key encoding. When returning to the upper-level
/// interface, a pure epoch with the lower 16 bits set to 0 should be returned.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug, PartialOrd, Ord)]
pub struct EpochWithGap(u64);

impl EpochWithGap {
    #[allow(unused_variables)]
    pub fn new(epoch: u64, spill_offset: u16) -> Self {
        // We only use 48 high bit to store epoch and use 16 low bit to store spill offset. But for MAX epoch,
        // we still keep `u64::MAX` because we have use it in delete range and persist this value to sstable files.
        //  So for compatibility, we must skip checking it for u64::MAX. See bug description in https://github.com/risingwavelabs/risingwave/issues/13717
        if risingwave_common::util::epoch::is_max_epoch(epoch) {
            EpochWithGap::new_max_epoch()
        } else {
            debug_assert!((epoch & EPOCH_SPILL_TIME_MASK) == 0);
            EpochWithGap(epoch + spill_offset as u64)
        }
    }

    pub fn new_from_epoch(epoch: u64) -> Self {
        EpochWithGap::new(epoch, 0)
    }

    pub fn new_min_epoch() -> Self {
        EpochWithGap(0)
    }

    pub fn new_max_epoch() -> Self {
        EpochWithGap(HummockEpoch::MAX)
    }

    // return the epoch_with_gap(epoch + spill_offset)
    pub(crate) fn as_u64(&self) -> HummockEpoch {
        self.0
    }

    // return the epoch_with_gap(epoch + spill_offset)
    pub fn from_u64(epoch_with_gap: u64) -> Self {
        EpochWithGap(epoch_with_gap)
    }

    // return the pure epoch without spill offset
    pub fn pure_epoch(&self) -> HummockEpoch {
        self.0 & !EPOCH_SPILL_TIME_MASK
    }

    pub fn offset(&self) -> u64 {
        self.0 & EPOCH_SPILL_TIME_MASK
    }
}
