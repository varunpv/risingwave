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

use opendal::layers::LoggingLayer;
use opendal::services::Obs;
use opendal::Operator;
use risingwave_common::config::ObjectStoreConfig;

use super::{EngineType, OpendalObjectStore};
use crate::object::ObjectResult;

impl OpendalObjectStore {
    /// create opendal obs engine.
    pub fn new_obs_engine(
        bucket: String,
        root: String,
        config: Arc<ObjectStoreConfig>,
    ) -> ObjectResult<Self> {
        // Create obs backend builder.
        let mut builder = Obs::default();

        builder.bucket(&bucket);

        builder.root(&root);

        let endpoint = std::env::var("OBS_ENDPOINT")
            .unwrap_or_else(|_| panic!("OBS_ENDPOINT not found from environment variables"));
        let access_key_id = std::env::var("OBS_ACCESS_KEY_ID")
            .unwrap_or_else(|_| panic!("OBS_ACCESS_KEY_ID not found from environment variables"));
        let secret_access_key = std::env::var("OBS_SECRET_ACCESS_KEY").unwrap_or_else(|_| {
            panic!("OBS_SECRET_ACCESS_KEY not found from environment variables")
        });

        builder.endpoint(&endpoint);
        builder.access_key_id(&access_key_id);
        builder.secret_access_key(&secret_access_key);

        let op: Operator = Operator::new(builder)?
            .layer(LoggingLayer::default())
            .finish();
        Ok(Self {
            op,
            engine_type: EngineType::Obs,
            config,
        })
    }
}
