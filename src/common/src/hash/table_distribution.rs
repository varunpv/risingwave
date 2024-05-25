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

use std::mem::replace;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use itertools::Itertools;
use risingwave_pb::plan_common::StorageTableDesc;
use tracing::warn;

use crate::array::{Array, DataChunk, PrimitiveArray};
use crate::buffer::{Bitmap, BitmapBuilder};
use crate::hash::VirtualNode;
use crate::row::Row;
use crate::util::iter_util::ZipEqFast;

/// For tables without distribution (singleton), the `DEFAULT_VNODE` is encoded.
pub const DEFAULT_VNODE: VirtualNode = VirtualNode::ZERO;

#[derive(Debug, Clone)]
enum ComputeVnode {
    Singleton,
    DistKeyIndices {
        /// Indices of distribution key for computing vnode, based on the pk columns of the table.
        dist_key_in_pk_indices: Vec<usize>,
    },
    VnodeColumnIndex {
        /// Index of vnode column.
        vnode_col_idx_in_pk: usize,
    },
}

#[derive(Debug, Clone)]
/// Represents the distribution for a specific table instance.
pub struct TableDistribution {
    /// The way to compute vnode provided primary key
    compute_vnode: ComputeVnode,

    /// Virtual nodes that the table is partitioned into.
    vnodes: Arc<Bitmap>,
}

pub const SINGLETON_VNODE: VirtualNode = DEFAULT_VNODE;

impl TableDistribution {
    pub fn new_from_storage_table_desc(
        vnodes: Option<Arc<Bitmap>>,
        table_desc: &StorageTableDesc,
    ) -> Self {
        let dist_key_in_pk_indices = table_desc
            .dist_key_in_pk_indices
            .iter()
            .map(|&k| k as usize)
            .collect_vec();
        let vnode_col_idx_in_pk = table_desc.vnode_col_idx_in_pk.map(|k| k as usize);
        Self::new(vnodes, dist_key_in_pk_indices, vnode_col_idx_in_pk)
    }

    pub fn new(
        vnodes: Option<Arc<Bitmap>>,
        dist_key_in_pk_indices: Vec<usize>,
        vnode_col_idx_in_pk: Option<usize>,
    ) -> Self {
        let compute_vnode = if let Some(vnode_col_idx_in_pk) = vnode_col_idx_in_pk {
            ComputeVnode::VnodeColumnIndex {
                vnode_col_idx_in_pk,
            }
        } else if !dist_key_in_pk_indices.is_empty() {
            ComputeVnode::DistKeyIndices {
                dist_key_in_pk_indices,
            }
        } else {
            ComputeVnode::Singleton
        };

        let vnodes = vnodes.unwrap_or_else(Self::singleton_vnode_bitmap);
        if let ComputeVnode::Singleton = &compute_vnode {
            if &vnodes != Self::singleton_vnode_bitmap_ref() && &vnodes != Self::all_vnodes_ref() {
                warn!(
                    ?vnodes,
                    "singleton distribution get non-singleton vnode bitmap"
                );
            }
        }

        Self {
            compute_vnode,
            vnodes,
        }
    }

    pub fn is_singleton(&self) -> bool {
        matches!(&self.compute_vnode, ComputeVnode::Singleton)
    }

    pub fn singleton_vnode_bitmap_ref() -> &'static Arc<Bitmap> {
        /// A bitmap that only the default vnode is set.
        static SINGLETON_VNODES: LazyLock<Arc<Bitmap>> = LazyLock::new(|| {
            let mut vnodes = BitmapBuilder::zeroed(VirtualNode::COUNT);
            vnodes.set(SINGLETON_VNODE.to_index(), true);
            vnodes.finish().into()
        });

        SINGLETON_VNODES.deref()
    }

    pub fn singleton_vnode_bitmap() -> Arc<Bitmap> {
        Self::singleton_vnode_bitmap_ref().clone()
    }

    pub fn all_vnodes_ref() -> &'static Arc<Bitmap> {
        /// A bitmap that all vnodes are set.
        static ALL_VNODES: LazyLock<Arc<Bitmap>> =
            LazyLock::new(|| Bitmap::ones(VirtualNode::COUNT).into());
        &ALL_VNODES
    }

    pub fn all_vnodes() -> Arc<Bitmap> {
        Self::all_vnodes_ref().clone()
    }

    /// Distribution that accesses all vnodes, mainly used for tests.
    pub fn all(dist_key_in_pk_indices: Vec<usize>) -> Self {
        Self {
            compute_vnode: ComputeVnode::DistKeyIndices {
                dist_key_in_pk_indices,
            },
            vnodes: Self::all_vnodes(),
        }
    }

    /// Fallback distribution for singleton or tests.
    pub fn singleton() -> Self {
        Self {
            compute_vnode: ComputeVnode::Singleton,
            vnodes: Self::singleton_vnode_bitmap(),
        }
    }

    pub fn update_vnode_bitmap(&mut self, new_vnodes: Arc<Bitmap>) -> Arc<Bitmap> {
        if self.is_singleton() && &new_vnodes != Self::singleton_vnode_bitmap_ref() {
            warn!(?new_vnodes, "update vnode on singleton distribution");
        }
        assert_eq!(self.vnodes.len(), new_vnodes.len());
        replace(&mut self.vnodes, new_vnodes)
    }

    pub fn vnodes(&self) -> &Arc<Bitmap> {
        &self.vnodes
    }

    /// Get vnode value with given primary key.
    pub fn compute_vnode_by_pk(&self, pk: impl Row) -> VirtualNode {
        match &self.compute_vnode {
            ComputeVnode::Singleton => SINGLETON_VNODE,
            ComputeVnode::DistKeyIndices {
                dist_key_in_pk_indices,
            } => compute_vnode(pk, dist_key_in_pk_indices, &self.vnodes),
            ComputeVnode::VnodeColumnIndex {
                vnode_col_idx_in_pk,
            } => get_vnode_from_row(pk, *vnode_col_idx_in_pk, &self.vnodes),
        }
    }

    pub fn try_compute_vnode_by_pk_prefix(&self, pk_prefix: impl Row) -> Option<VirtualNode> {
        match &self.compute_vnode {
            ComputeVnode::Singleton => Some(SINGLETON_VNODE),
            ComputeVnode::DistKeyIndices {
                dist_key_in_pk_indices,
            } => dist_key_in_pk_indices
                .iter()
                .all(|&d| d < pk_prefix.len())
                .then(|| compute_vnode(pk_prefix, dist_key_in_pk_indices, &self.vnodes)),
            ComputeVnode::VnodeColumnIndex {
                vnode_col_idx_in_pk,
            } => {
                if *vnode_col_idx_in_pk >= pk_prefix.len() {
                    None
                } else {
                    Some(get_vnode_from_row(
                        pk_prefix,
                        *vnode_col_idx_in_pk,
                        &self.vnodes,
                    ))
                }
            }
        }
    }
}

/// Get vnode value with `indices` on the given `row`.
pub fn compute_vnode(row: impl Row, indices: &[usize], vnodes: &Bitmap) -> VirtualNode {
    assert!(!indices.is_empty());
    let vnode = VirtualNode::compute_row(&row, indices);
    check_vnode_is_set(vnode, vnodes);

    tracing::debug!(target: "events::storage::storage_table", "compute vnode: {:?} key {:?} => {}", row, indices, vnode);

    vnode
}

pub fn get_vnode_from_row(row: impl Row, index: usize, vnodes: &Bitmap) -> VirtualNode {
    let vnode = VirtualNode::from_datum(row.datum_at(index));
    check_vnode_is_set(vnode, vnodes);

    tracing::debug!(target: "events::storage::storage_table", "get vnode from row: {:?} vnode column index {:?} => {}", row, index, vnode);

    vnode
}

impl TableDistribution {
    /// Get vnode values with `indices` on the given `chunk`.
    ///
    /// Vnode of invisible rows will be included. Only the vnode of visible row check if it's accessible
    pub fn compute_chunk_vnode(&self, chunk: &DataChunk, pk_indices: &[usize]) -> Vec<VirtualNode> {
        match &self.compute_vnode {
            ComputeVnode::Singleton => {
                vec![SINGLETON_VNODE; chunk.capacity()]
            }
            ComputeVnode::DistKeyIndices {
                dist_key_in_pk_indices,
            } => {
                let dist_key_indices = dist_key_in_pk_indices
                    .iter()
                    .map(|idx| pk_indices[*idx])
                    .collect_vec();

                VirtualNode::compute_chunk(chunk, &dist_key_indices)
                    .into_iter()
                    .zip_eq_fast(chunk.visibility().iter())
                    .map(|(vnode, vis)| {
                        // Ignore the invisible rows.
                        if vis {
                            check_vnode_is_set(vnode, &self.vnodes);
                        }
                        vnode
                    })
                    .collect()
            }
            ComputeVnode::VnodeColumnIndex {
                vnode_col_idx_in_pk,
            } => {
                let array: &PrimitiveArray<i16> =
                    chunk.columns()[pk_indices[*vnode_col_idx_in_pk]].as_int16();
                array
                    .raw_iter()
                    .zip_eq_fast(array.null_bitmap().iter())
                    .zip_eq_fast(chunk.visibility().iter())
                    .map(|((vnode, exist), vis)| {
                        let vnode = VirtualNode::from_scalar(vnode);
                        if vis {
                            assert!(exist);
                            check_vnode_is_set(vnode, &self.vnodes);
                        }
                        vnode
                    })
                    .collect_vec()
            }
        }
    }
}

/// Check whether the given `vnode` is set in the `vnodes` of this table.
fn check_vnode_is_set(vnode: VirtualNode, vnodes: &Bitmap) {
    let is_set = vnodes.is_set(vnode.to_index());
    assert!(
        is_set,
        "vnode {} should not be accessed by this table",
        vnode
    );
}
