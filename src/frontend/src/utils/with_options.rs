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

use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroU32;

use risingwave_connector::source::kafka::private_link::{
    insert_privatelink_broker_rewrite_map, CONNECTION_NAME_KEY, PRIVATELINK_ENDPOINT_KEY,
};
use risingwave_connector::WithPropertiesExt;
use risingwave_sqlparser::ast::{
    CreateConnectionStatement, CreateSinkStatement, CreateSourceStatement,
    CreateSubscriptionStatement, SqlOption, Statement, Value,
};

use super::OverwriteOptions;
use crate::catalog::connection_catalog::resolve_private_link_connection;
use crate::catalog::ConnectionId;
use crate::error::{ErrorCode, Result as RwResult, RwError};
use crate::session::SessionImpl;

mod options {

    pub const RETENTION_SECONDS: &str = "retention_seconds";
}

/// Options or properties extracted from the `WITH` clause of DDLs.
#[derive(Default, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WithOptions {
    inner: BTreeMap<String, String>,
}

impl std::ops::Deref for WithOptions {
    type Target = BTreeMap<String, String>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for WithOptions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl WithOptions {
    /// Create a new [`WithOptions`] from a [`HashMap`].
    pub fn new(inner: HashMap<String, String>) -> Self {
        Self {
            inner: inner.into_iter().collect(),
        }
    }

    pub fn from_inner(inner: BTreeMap<String, String>) -> Self {
        Self { inner }
    }

    /// Get the reference of the inner map.
    pub fn inner(&self) -> &BTreeMap<String, String> {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut BTreeMap<String, String> {
        &mut self.inner
    }

    /// Take the value of the inner map.
    pub fn into_inner(self) -> BTreeMap<String, String> {
        self.inner
    }

    /// Convert to connector props, remove the key-value pairs used in the top-level.
    pub fn into_connector_props(self) -> HashMap<String, String> {
        self.inner
            .into_iter()
            .filter(|(key, _)| key != OverwriteOptions::STREAMING_RATE_LIMIT_KEY)
            .collect()
    }

    /// Parse the retention seconds from the options.
    pub fn retention_seconds(&self) -> Option<NonZeroU32> {
        self.inner
            .get(options::RETENTION_SECONDS)
            .and_then(|s| s.parse().ok())
    }

    /// Get a subset of the options from the given keys.
    pub fn subset(&self, keys: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let inner = keys
            .into_iter()
            .filter_map(|k| {
                self.inner
                    .get_key_value(k.as_ref())
                    .map(|(k, v)| (k.clone(), v.clone()))
            })
            .collect();

        Self { inner }
    }

    pub fn value_eq_ignore_case(&self, key: &str, val: &str) -> bool {
        if let Some(inner_val) = self.inner.get(key) {
            if inner_val.eq_ignore_ascii_case(val) {
                return true;
            }
        }
        false
    }
}

pub(crate) fn resolve_privatelink_in_with_option(
    with_options: &mut WithOptions,
    schema_name: &Option<String>,
    session: &SessionImpl,
) -> RwResult<Option<ConnectionId>> {
    let is_kafka = with_options.is_kafka_connector();
    let privatelink_endpoint = with_options.remove(PRIVATELINK_ENDPOINT_KEY);

    // if `privatelink.endpoint` is provided in WITH, use it to rewrite broker address directly
    if let Some(endpoint) = privatelink_endpoint {
        if !is_kafka {
            return Err(RwError::from(ErrorCode::ProtocolError(
                "Privatelink is only supported in kafka connector".to_string(),
            )));
        }
        insert_privatelink_broker_rewrite_map(with_options.inner_mut(), None, Some(endpoint))
            .map_err(RwError::from)?;
        return Ok(None);
    }

    let connection_name = with_options
        .remove(CONNECTION_NAME_KEY)
        .map(|s| s.to_lowercase());
    let connection_id = match connection_name {
        Some(connection_name) => {
            let connection = session
                .get_connection_by_name(schema_name.clone(), &connection_name)
                .map_err(|_| ErrorCode::ItemNotFound(connection_name))?;
            if !is_kafka {
                return Err(RwError::from(ErrorCode::ProtocolError(
                    "Connection is only supported in kafka connector".to_string(),
                )));
            }
            resolve_private_link_connection(&connection, with_options.inner_mut())?;
            Some(connection.id)
        }
        None => None,
    };
    Ok(connection_id)
}

impl TryFrom<&[SqlOption]> for WithOptions {
    type Error = RwError;

    fn try_from(options: &[SqlOption]) -> Result<Self, Self::Error> {
        let mut inner: BTreeMap<String, String> = BTreeMap::new();
        for option in options {
            let key = option.name.real_value();
            let value: String = match option.value.clone() {
                Value::CstyleEscapedString(s) => s.value,
                Value::SingleQuotedString(s) => s,
                Value::Number(n) => n,
                Value::Boolean(b) => b.to_string(),
                _ => {
                    return Err(RwError::from(ErrorCode::InvalidParameterValue(
                        "`with options` or `with properties` only support single quoted string value and C style escaped string"
                            .to_owned(),
                    )))
                }
            };
            if inner.insert(key.clone(), value).is_some() {
                return Err(RwError::from(ErrorCode::InvalidParameterValue(format!(
                    "Duplicated option: {}",
                    key
                ))));
            }
        }

        Ok(Self { inner })
    }
}

impl TryFrom<&Statement> for WithOptions {
    type Error = RwError;

    /// Extract options from the `WITH` clause from the given statement.
    fn try_from(statement: &Statement) -> Result<Self, Self::Error> {
        match statement {
            // Explain: forward to the inner statement.
            Statement::Explain { statement, .. } => Self::try_from(statement.as_ref()),

            // View
            Statement::CreateView { with_options, .. } => Self::try_from(with_options.as_slice()),

            // Sink
            Statement::CreateSink {
                stmt:
                    CreateSinkStatement {
                        with_properties, ..
                    },
            }
            | Statement::CreateConnection {
                stmt:
                    CreateConnectionStatement {
                        with_properties, ..
                    },
            } => Self::try_from(with_properties.0.as_slice()),
            Statement::CreateSource {
                stmt:
                    CreateSourceStatement {
                        with_properties, ..
                    },
                ..
            } => Self::try_from(with_properties.0.as_slice()),
            Statement::CreateSubscription {
                stmt:
                    CreateSubscriptionStatement {
                        with_properties, ..
                    },
                ..
            } => Self::try_from(with_properties.0.as_slice()),
            Statement::CreateTable { with_options, .. } => Self::try_from(with_options.as_slice()),

            _ => Ok(Default::default()),
        }
    }
}
