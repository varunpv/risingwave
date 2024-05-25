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

use std::iter;
use std::ops::Sub;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::stream;
use itertools::Itertools;
use risingwave_meta::hummock::test_utils::setup_compute_env;
use risingwave_meta::hummock::MockHummockMetaClient;
use risingwave_object_store::object::ObjectMetadata;
use risingwave_pb::hummock::{FullScanTask, VacuumTask};
use risingwave_storage::hummock::iterator::test_utils::mock_sstable_store;
use risingwave_storage::hummock::test_utils::{
    default_builder_opt_for_test, gen_default_test_sstable,
};
use risingwave_storage::hummock::vacuum::Vacuum;

#[tokio::test]
async fn test_vacuum() {
    let sstable_store = mock_sstable_store().await;
    // Put some SSTs to object store
    let object_ids = (1..10).collect_vec();
    let mut sstables = vec![];
    for sstable_object_id in &object_ids {
        let sstable = gen_default_test_sstable(
            default_builder_opt_for_test(),
            *sstable_object_id,
            sstable_store.clone(),
        )
        .await;
        sstables.push(sstable);
    }

    // Delete all existent SSTs and a nonexistent SSTs. Trying to delete a nonexistent SST is
    // OK.
    let nonexistent_id = 11u64;
    let vacuum_task = VacuumTask {
        sstable_object_ids: object_ids
            .into_iter()
            .chain(iter::once(nonexistent_id))
            .collect_vec(),
    };
    let (_env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let mock_hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref.clone(),
        worker_node.id,
    ));
    Vacuum::handle_vacuum_task(sstable_store, &vacuum_task.sstable_object_ids)
        .await
        .unwrap();
    assert!(Vacuum::report_vacuum_task(vacuum_task, mock_hummock_meta_client).await);
}

#[tokio::test]
async fn test_full_scan() {
    let (_env, hummock_manager_ref, _cluster_manager_ref, worker_node) =
        setup_compute_env(8080).await;
    let sstable_store = mock_sstable_store().await;
    let _mock_hummock_meta_client = Arc::new(MockHummockMetaClient::new(
        hummock_manager_ref,
        worker_node.id,
    ));
    let now_ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let object_store_list_result = vec![
        ObjectMetadata {
            key: sstable_store.get_sst_data_path(1),
            last_modified: now_ts.sub(Duration::from_secs(7200)).as_secs_f64(),
            total_size: 128,
        },
        ObjectMetadata {
            key: sstable_store.get_sst_data_path(2),
            last_modified: now_ts.sub(Duration::from_secs(3600)).as_secs_f64(),
            total_size: 128,
        },
    ];
    let object_metadata_iter = Box::pin(stream::iter(object_store_list_result.into_iter().map(Ok)));

    let task = FullScanTask {
        sst_retention_time_sec: 10000,
    };
    let (scan_result, _, _) = Vacuum::full_scan_inner(task, object_metadata_iter.clone())
        .await
        .unwrap();
    assert!(scan_result.is_empty());

    let task = FullScanTask {
        sst_retention_time_sec: 6000,
    };
    let (scan_result, _, _) = Vacuum::full_scan_inner(task, object_metadata_iter.clone())
        .await
        .unwrap();
    assert_eq!(scan_result.into_iter().sorted().collect_vec(), vec![1]);

    let task = FullScanTask {
        sst_retention_time_sec: 2000,
    };
    let (scan_result, _, _) = Vacuum::full_scan_inner(task, object_metadata_iter)
        .await
        .unwrap();
    assert_eq!(scan_result.into_iter().sorted().collect_vec(), vec![1, 2]);
}
