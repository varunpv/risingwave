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

// for derived code of `Message`
#![expect(clippy::all)]
#![expect(clippy::doc_markdown)]
#![feature(lint_reasons)]

use std::str::FromStr;

use risingwave_error::tonic::ToTonicStatus;
use thiserror::Error;

#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/catalog.rs")]
pub mod catalog;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/common.rs")]
pub mod common;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/compute.rs")]
pub mod compute;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/cloud_service.rs")]
pub mod cloud_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/data.rs")]
pub mod data;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/ddl_service.rs")]
pub mod ddl_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/expr.rs")]
pub mod expr;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/meta.rs")]
pub mod meta;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/plan_common.rs")]
pub mod plan_common;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/batch_plan.rs")]
pub mod batch_plan;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/task_service.rs")]
pub mod task_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path="sim/connector_service.rs")]
pub mod connector_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/stream_plan.rs")]
pub mod stream_plan;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/stream_service.rs")]
pub mod stream_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/hummock.rs")]
pub mod hummock;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/compactor.rs")]
pub mod compactor;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/user.rs")]
pub mod user;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/source.rs")]
pub mod source;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/monitor_service.rs")]
pub mod monitor_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/backup_service.rs")]
pub mod backup_service;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/java_binding.rs")]
pub mod java_binding;
#[rustfmt::skip]
#[cfg_attr(madsim, path = "sim/health.rs")]
pub mod health;
#[rustfmt::skip]
#[path = "sim/telemetry.rs"]
pub mod telemetry;
#[rustfmt::skip]
#[path = "connector_service.serde.rs"]
pub mod connector_service_serde;
#[rustfmt::skip]
#[path = "catalog.serde.rs"]
pub mod catalog_serde;
#[rustfmt::skip]
#[path = "common.serde.rs"]
pub mod common_serde;
#[rustfmt::skip]
#[path = "compute.serde.rs"]
pub mod compute_serde;
#[rustfmt::skip]
#[path = "cloud_service.serde.rs"]
pub mod cloud_service_serde;
#[rustfmt::skip]
#[path = "data.serde.rs"]
pub mod data_serde;
#[rustfmt::skip]
#[path = "ddl_service.serde.rs"]
pub mod ddl_service_serde;
#[rustfmt::skip]
#[path = "expr.serde.rs"]
pub mod expr_serde;
#[rustfmt::skip]
#[path = "meta.serde.rs"]
pub mod meta_serde;
#[rustfmt::skip]
#[path = "plan_common.serde.rs"]
pub mod plan_common_serde;
#[rustfmt::skip]
#[path = "batch_plan.serde.rs"]
pub mod batch_plan_serde;
#[rustfmt::skip]
#[path = "task_service.serde.rs"]
pub mod task_service_serde;
#[rustfmt::skip]
#[path = "stream_plan.serde.rs"]
pub mod stream_plan_serde;
#[rustfmt::skip]
#[path = "stream_service.serde.rs"]
pub mod stream_service_serde;
#[rustfmt::skip]
#[path = "hummock.serde.rs"]
pub mod hummock_serde;
#[rustfmt::skip]
#[path = "compactor.serde.rs"]
pub mod compactor_serde;
#[rustfmt::skip]
#[path = "user.serde.rs"]
pub mod user_serde;
#[rustfmt::skip]
#[path = "source.serde.rs"]
pub mod source_serde;
#[rustfmt::skip]
#[path = "monitor_service.serde.rs"]
pub mod monitor_service_serde;
#[rustfmt::skip]
#[path = "backup_service.serde.rs"]
pub mod backup_service_serde;
#[rustfmt::skip]
#[path = "java_binding.serde.rs"]
pub mod java_binding_serde;
#[rustfmt::skip]
#[path = "telemetry.serde.rs"]
pub mod telemetry_serde;

#[derive(Clone, PartialEq, Eq, Debug, Error)]
#[error("field `{0}` not found")]
pub struct PbFieldNotFound(pub &'static str);

impl From<PbFieldNotFound> for tonic::Status {
    fn from(e: PbFieldNotFound) -> Self {
        e.to_status_unnamed(tonic::Code::Internal)
    }
}

impl FromStr for crate::expr::table_function::PbType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str_name(&s.to_uppercase()).ok_or(())
    }
}

impl stream_plan::MaterializeNode {
    pub fn dist_key_indices(&self) -> Vec<u32> {
        self.get_table()
            .unwrap()
            .distribution_key
            .iter()
            .map(|i| *i as u32)
            .collect()
    }

    pub fn column_ids(&self) -> Vec<i32> {
        self.get_table()
            .unwrap()
            .columns
            .iter()
            .map(|c| c.get_column_desc().unwrap().column_id)
            .collect()
    }
}

impl stream_plan::SourceNode {
    pub fn column_ids(&self) -> Option<Vec<i32>> {
        Some(
            self.source_inner
                .as_ref()?
                .columns
                .iter()
                .map(|c| c.get_column_desc().unwrap().column_id)
                .collect(),
        )
    }
}

impl stream_plan::StreamNode {
    /// Find the external stream source info inside the stream node, if any.
    ///
    /// Returns `source_id`.
    pub fn find_stream_source(&self) -> Option<u32> {
        if let Some(crate::stream_plan::stream_node::NodeBody::Source(source)) =
            self.node_body.as_ref()
        {
            if let Some(inner) = &source.source_inner {
                return Some(inner.source_id);
            }
        }

        for child in &self.input {
            if let Some(source) = child.find_stream_source() {
                return Some(source);
            }
        }

        None
    }

    /// Find the external stream source info inside the stream node, if any.
    ///
    /// Returns `source_id`.
    pub fn find_source_backfill(&self) -> Option<u32> {
        if let Some(crate::stream_plan::stream_node::NodeBody::SourceBackfill(source)) =
            self.node_body.as_ref()
        {
            return Some(source.upstream_source_id);
        }

        for child in &self.input {
            if let Some(source) = child.find_source_backfill() {
                return Some(source);
            }
        }

        None
    }
}

impl catalog::StreamSourceInfo {
    /// Refer to [`Self::cdc_source_job`] for details.
    pub fn is_shared(&self) -> bool {
        self.cdc_source_job
    }
}

#[cfg(test)]
mod tests {
    use crate::data::{data_type, DataType};
    use crate::plan_common::Field;

    #[test]
    fn test_getter() {
        let mut data_type: DataType = DataType::default();
        data_type.is_nullable = true;
        let field = Field {
            data_type: Some(data_type),
            name: "".to_string(),
        };
        assert!(field.get_data_type().unwrap().is_nullable);
    }

    #[test]
    fn test_enum_getter() {
        let mut data_type: DataType = DataType::default();
        data_type.type_name = data_type::TypeName::Double as i32;
        assert_eq!(
            data_type::TypeName::Double,
            data_type.get_type_name().unwrap()
        );
    }

    #[test]
    fn test_enum_unspecified() {
        let mut data_type: DataType = DataType::default();
        data_type.type_name = data_type::TypeName::TypeUnspecified as i32;
        assert!(data_type.get_type_name().is_err());
    }

    #[test]
    fn test_primitive_getter() {
        let data_type: DataType = DataType::default();
        let new_data_type = DataType {
            is_nullable: data_type.get_is_nullable(),
            ..Default::default()
        };
        assert!(!new_data_type.is_nullable);
    }
}
