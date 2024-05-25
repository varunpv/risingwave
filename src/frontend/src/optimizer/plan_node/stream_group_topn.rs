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

use fixedbitset::FixedBitSet;
use pretty_xmlish::XmlNode;
use risingwave_pb::stream_plan::stream_node::PbNodeBody;

use super::generic::{DistillUnit, TopNLimit};
use super::stream::prelude::*;
use super::utils::{plan_node_name, watermark_pretty, Distill};
use super::{generic, ExprRewritable, PlanBase, PlanTreeNodeUnary, StreamNode};
use crate::optimizer::plan_node::expr_visitable::ExprVisitable;
use crate::optimizer::plan_node::generic::GenericPlanNode;
use crate::optimizer::property::Order;
use crate::stream_fragmenter::BuildFragmentGraphState;
use crate::PlanRef;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamGroupTopN {
    pub base: PlanBase<Stream>,
    core: generic::TopN<PlanRef>,
    /// an optional column index which is the vnode of each row computed by the input's consistent
    /// hash distribution
    vnode_col_idx: Option<usize>,
}

impl StreamGroupTopN {
    pub fn new(core: generic::TopN<PlanRef>, vnode_col_idx: Option<usize>) -> Self {
        assert!(!core.group_key.is_empty());
        assert!(core.limit_attr.limit() > 0);
        let input = &core.input;
        let schema = input.schema().clone();

        let watermark_columns = if input.append_only() {
            input.watermark_columns().clone()
        } else {
            let mut watermark_columns = FixedBitSet::with_capacity(schema.len());
            for &idx in &core.group_key {
                if input.watermark_columns().contains(idx) {
                    watermark_columns.insert(idx);
                }
            }
            watermark_columns
        };

        let mut stream_key = core
            .stream_key()
            .expect("logical node should have stream key here");
        if let Some(vnode_col_idx) = vnode_col_idx
            && stream_key.len() > 1
        {
            // The output stream key of `GroupTopN` is a union of group key and input stream key,
            // while vnode is calculated from a subset of input stream key. So we can safely remove
            // the vnode column from output stream key. While at meanwhile we cannot leave the stream key
            // as empty, so we only remove it when stream key length is > 1.
            stream_key.remove(stream_key.iter().position(|i| *i == vnode_col_idx).unwrap());
        }

        let base = PlanBase::new_stream(
            core.ctx(),
            core.schema(),
            Some(stream_key),
            core.functional_dependency(),
            input.distribution().clone(),
            false,
            // TODO: https://github.com/risingwavelabs/risingwave/issues/8348
            false,
            watermark_columns,
        );
        StreamGroupTopN {
            base,
            core,
            vnode_col_idx,
        }
    }

    pub fn limit_attr(&self) -> TopNLimit {
        self.core.limit_attr
    }

    pub fn offset(&self) -> u64 {
        self.core.offset
    }

    pub fn topn_order(&self) -> &Order {
        &self.core.order
    }

    pub fn group_key(&self) -> &[usize] {
        &self.core.group_key
    }
}

impl StreamNode for StreamGroupTopN {
    fn to_stream_prost_body(&self, state: &mut BuildFragmentGraphState) -> PbNodeBody {
        use risingwave_pb::stream_plan::*;

        let input = self.input();
        let table = self
            .core
            .infer_internal_table_catalog(
                input.schema(),
                input.ctx(),
                input.expect_stream_key(),
                self.vnode_col_idx,
            )
            .with_id(state.gen_table_id_wrapped());
        assert!(!self.group_key().is_empty());
        let group_topn_node = GroupTopNNode {
            limit: self.limit_attr().limit(),
            offset: self.offset(),
            with_ties: self.limit_attr().with_ties(),
            group_key: self.group_key().iter().map(|idx| *idx as u32).collect(),
            table: Some(table.to_internal_table_prost()),
            order_by: self.topn_order().to_protobuf(),
        };
        if self.input().append_only() {
            PbNodeBody::AppendOnlyGroupTopN(group_topn_node)
        } else {
            PbNodeBody::GroupTopN(group_topn_node)
        }
    }
}

impl Distill for StreamGroupTopN {
    fn distill<'a>(&self) -> XmlNode<'a> {
        let name = plan_node_name!("StreamGroupTopN",
            { "append_only", self.input().append_only() },
        );
        let mut node = self.core.distill_with_name(name);
        if let Some(ow) = watermark_pretty(self.base.watermark_columns(), self.schema()) {
            node.fields.push(("output_watermarks".into(), ow));
        }
        node
    }
}

impl_plan_tree_node_for_unary! { StreamGroupTopN }

impl PlanTreeNodeUnary for StreamGroupTopN {
    fn input(&self) -> PlanRef {
        self.core.input.clone()
    }

    fn clone_with_input(&self, input: PlanRef) -> Self {
        let mut core = self.core.clone();
        core.input = input;
        Self::new(core, self.vnode_col_idx)
    }
}

impl ExprRewritable for StreamGroupTopN {}

impl ExprVisitable for StreamGroupTopN {}
