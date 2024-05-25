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

use std::sync::atomic::{self, AtomicI64};

use async_trait::async_trait;
use etcd_client::{Compare, CompareOp, Error as EtcdError, GetOptions, Txn, TxnOp};
use futures::Future;
use itertools::Itertools;
use risingwave_common::config::MetaBackend;
use tokio::sync::Mutex;

use super::{Key, MetaStore, MetaStoreError, MetaStoreResult, Snapshot, Transaction, Value};
use crate::storage::etcd_retry_client::EtcdRetryClient as KvClient;
use crate::storage::WrappedEtcdClient;

impl From<EtcdError> for MetaStoreError {
    fn from(err: EtcdError) -> Self {
        MetaStoreError::Internal(anyhow::Error::new(err))
    }
}

const REVISION_UNINITIALIZED: i64 = -1;

#[derive(Clone)]
pub struct EtcdMetaStore {
    client: KvClient,
}
pub struct EtcdSnapshot {
    client: KvClient,
    revision: AtomicI64,
    init_lock: Mutex<()>,
}

// TODO: we can refine the key encoding before release.
fn encode_etcd_key(cf: &str, key: &[u8]) -> Vec<u8> {
    let mut encoded_key = Vec::with_capacity(key.len() + cf.len() + 1);
    encoded_key.extend_from_slice(cf.as_bytes());
    encoded_key.push(b'/');
    encoded_key.extend_from_slice(key);
    encoded_key
}

impl EtcdSnapshot {
    async fn view_inner<V: SnapshotViewer>(&self, view: V) -> MetaStoreResult<V::Output> {
        loop {
            let revision = self.revision.load(atomic::Ordering::Relaxed);
            if revision != REVISION_UNINITIALIZED {
                // Fast and likely path.
                let (_, output) = view.view(self.client.clone(), revision).await?;
                return Ok(output);
            } else {
                // Slow path
                let _g = self.init_lock.lock().await;
                let revision = self.revision.load(atomic::Ordering::Acquire);
                if revision != REVISION_UNINITIALIZED {
                    // Double check failed, release the lock.
                    continue;
                }
                let (new_revision, output) = view.view(self.client.clone(), revision).await?;
                self.revision.store(new_revision, atomic::Ordering::Release);
                return Ok(output);
            }
        }
    }
}

trait SnapshotViewer {
    type Output;
    type OutputFuture<'a>: Future<Output = MetaStoreResult<(i64, Self::Output)>> + 'a
    where
        Self: 'a;

    fn view(&self, client: KvClient, revision: i64) -> Self::OutputFuture<'_>;
}

struct GetViewer {
    key: Vec<u8>,
}

impl SnapshotViewer for GetViewer {
    type Output = Vec<u8>;

    type OutputFuture<'a> = impl Future<Output = MetaStoreResult<(i64, Self::Output)>> + 'a;

    fn view(&self, client: KvClient, revision: i64) -> Self::OutputFuture<'_> {
        async move {
            let res = client
                .get(
                    self.key.clone(),
                    Some(GetOptions::default().with_revision(revision)),
                )
                .await?;
            let new_revision = if let Some(header) = res.header() {
                header.revision()
            } else {
                return Err(MetaStoreError::Internal(anyhow::anyhow!(
                    "Etcd response missing header"
                )));
            };
            let value = res
                .kvs()
                .first()
                .map(|kv| kv.value().to_vec())
                .ok_or_else(|| MetaStoreError::ItemNotFound(hex::encode(self.key.clone())))?;
            Ok((new_revision, value))
        }
    }
}

struct ListViewer {
    key: Vec<u8>,
}

impl SnapshotViewer for ListViewer {
    type Output = Vec<(Vec<u8>, Vec<u8>)>;

    type OutputFuture<'a> = impl Future<Output = MetaStoreResult<(i64, Self::Output)>> + 'a;

    fn view(&self, client: KvClient, revision: i64) -> Self::OutputFuture<'_> {
        async move {
            let res = client
                .get(
                    self.key.clone(),
                    Some(GetOptions::default().with_revision(revision).with_prefix()),
                )
                .await?;
            let new_revision = if let Some(header) = res.header() {
                header.revision()
            } else {
                return Err(MetaStoreError::Internal(anyhow::anyhow!(
                    "Etcd response missing header"
                )));
            };
            let kv = res
                .kvs()
                .iter()
                .map(|kv| (kv.key().to_vec(), kv.value().to_vec()))
                .collect();
            Ok((new_revision, kv))
        }
    }
}

#[async_trait]
impl Snapshot for EtcdSnapshot {
    async fn list_cf(&self, cf: &str) -> MetaStoreResult<Vec<(Vec<u8>, Vec<u8>)>> {
        let view = ListViewer {
            key: encode_etcd_key(cf, &[]),
        };
        let cf_bytes = cf.as_bytes().len() + 1;
        self.view_inner(view).await.map(|kvs| {
            kvs.into_iter()
                .map(|(mut k, v)| (k.drain(cf_bytes..).collect_vec(), v))
                .collect()
        })
    }

    async fn get_cf(&self, cf: &str, key: &[u8]) -> MetaStoreResult<Vec<u8>> {
        let view = GetViewer {
            key: encode_etcd_key(cf, key),
        };
        self.view_inner(view).await
    }
}

impl EtcdMetaStore {
    pub fn new(client: WrappedEtcdClient) -> Self {
        Self {
            client: KvClient::new(client),
        }
    }
}

#[async_trait]
impl MetaStore for EtcdMetaStore {
    type Snapshot = EtcdSnapshot;

    fn meta_store_type(&self) -> MetaBackend {
        MetaBackend::Etcd
    }

    async fn snapshot(&self) -> Self::Snapshot {
        EtcdSnapshot {
            client: self.client.clone(),
            revision: AtomicI64::new(REVISION_UNINITIALIZED),
            init_lock: Default::default(),
        }
    }

    async fn put_cf(&self, cf: &str, key: Key, value: Value) -> MetaStoreResult<()> {
        self.client
            .put(encode_etcd_key(cf, &key), value, None)
            .await?;
        Ok(())
    }

    async fn delete_cf(&self, cf: &str, key: &[u8]) -> MetaStoreResult<()> {
        self.client.delete(encode_etcd_key(cf, key), None).await?;
        Ok(())
    }

    async fn txn(&self, trx: Transaction) -> MetaStoreResult<()> {
        let (preconditions, operations) = trx.into_parts();
        let when = preconditions
            .into_iter()
            .map(|cond| match cond {
                super::Precondition::KeyExists { cf, key } => {
                    Compare::value(encode_etcd_key(&cf, &key), CompareOp::NotEqual, vec![])
                }
                super::Precondition::KeyEqual { cf, key, value } => {
                    Compare::value(encode_etcd_key(&cf, &key), CompareOp::Equal, value)
                }
            })
            .collect::<Vec<_>>();

        let then = operations
            .into_iter()
            .map(|op| match op {
                super::Operation::Put { cf, key, value } => {
                    let key = encode_etcd_key(&cf, &key);
                    let value = value.to_vec();
                    TxnOp::put(key, value, None)
                }
                super::Operation::Delete { cf, key } => {
                    let key = encode_etcd_key(&cf, &key);
                    TxnOp::delete(key, None)
                }
            })
            .collect::<Vec<_>>();

        let etcd_txn = Txn::new().when(when).and_then(then);
        if !self.client.txn(etcd_txn).await?.succeeded() {
            Err(MetaStoreError::TransactionAbort())
        } else {
            Ok(())
        }
    }
}
