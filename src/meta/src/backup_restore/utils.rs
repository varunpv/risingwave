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

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use etcd_client::ConnectOptions;
use risingwave_backup::error::{BackupError, BackupResult};
use risingwave_backup::storage::{MetaSnapshotStorageRef, ObjectStoreMetaSnapshotStorage};
use risingwave_common::config::{MetaBackend, ObjectStoreConfig};
use risingwave_object_store::object::build_remote_object_store;
use risingwave_object_store::object::object_metrics::ObjectStoreMetrics;
use sea_orm::DbBackend;

use crate::backup_restore::RestoreOpts;
use crate::controller::SqlMetaStore;
use crate::storage::{EtcdMetaStore, MemStore, WrappedEtcdClient as EtcdClient};
use crate::MetaStoreBackend;

#[derive(Clone)]
pub enum MetaStoreBackendImpl {
    Etcd(EtcdMetaStore),
    Mem(MemStore),
    Sql(SqlMetaStore),
}

#[macro_export]
macro_rules! dispatch_meta_store {
    ($impl:expr, $store:ident, $body:tt) => {{
        match $impl {
            MetaStoreBackendImpl::Etcd($store) => $body,
            MetaStoreBackendImpl::Mem($store) => $body,
            MetaStoreBackendImpl::Sql(_) => panic!("not supported"),
        }
    }};
}

// Code is copied from src/meta/src/rpc/server.rs. TODO #6482: extract method.
pub async fn get_meta_store(opts: RestoreOpts) -> BackupResult<MetaStoreBackendImpl> {
    let meta_store_backend = match opts.meta_store_type {
        MetaBackend::Etcd => MetaStoreBackend::Etcd {
            endpoints: opts
                .etcd_endpoints
                .split(',')
                .map(|x| x.to_string())
                .collect(),
            credentials: match opts.etcd_auth {
                true => Some((opts.etcd_username, opts.etcd_password)),
                false => None,
            },
        },
        MetaBackend::Mem => MetaStoreBackend::Mem,
        MetaBackend::Sql => MetaStoreBackend::Sql {
            endpoint: opts.sql_endpoint,
        },
    };
    match meta_store_backend {
        MetaStoreBackend::Etcd {
            endpoints,
            credentials,
        } => {
            let mut options = ConnectOptions::default()
                .with_keep_alive(Duration::from_secs(3), Duration::from_secs(5));
            if let Some((username, password)) = &credentials {
                options = options.with_user(username, password)
            }
            let client = EtcdClient::connect(endpoints, Some(options), credentials.is_some())
                .await
                .context("failed to connect etcd")?;
            Ok(MetaStoreBackendImpl::Etcd(EtcdMetaStore::new(client)))
        }
        MetaStoreBackend::Mem => Ok(MetaStoreBackendImpl::Mem(MemStore::new())),
        MetaStoreBackend::Sql { endpoint } => {
            let max_connection = if DbBackend::Sqlite.is_prefix_of(&endpoint) {
                // Due to the fact that Sqlite is prone to the error "(code: 5) database is locked" under concurrent access,
                // here we forcibly specify the number of connections as 1.
                1
            } else {
                10
            };
            let mut options = sea_orm::ConnectOptions::new(endpoint);
            options
                .max_connections(max_connection)
                .connect_timeout(Duration::from_secs(10))
                .idle_timeout(Duration::from_secs(30));
            let conn = sea_orm::Database::connect(options)
                .await
                .map_err(|e| BackupError::MetaStorage(e.into()))?;
            let meta_store_sql = SqlMetaStore::new(conn);
            Ok(MetaStoreBackendImpl::Sql(meta_store_sql))
        }
    }
}

pub async fn get_backup_store(opts: RestoreOpts) -> BackupResult<MetaSnapshotStorageRef> {
    let object_store = build_remote_object_store(
        &opts.backup_storage_url,
        Arc::new(ObjectStoreMetrics::unused()),
        "Meta Backup",
        Arc::new(ObjectStoreConfig::default()),
    )
    .await;
    let backup_store =
        ObjectStoreMetaSnapshotStorage::new(&opts.backup_storage_directory, Arc::new(object_store))
            .await?;
    Ok(Arc::new(backup_store))
}
