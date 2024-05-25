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

use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::TryStreamExt;
use risingwave_common::config::{MAX_CONNECTION_WINDOW_SIZE, STREAM_WINDOW_SIZE};
use risingwave_common::monitor::connection::{EndpointExt, TcpConfig};
use risingwave_pb::connector_service::connector_service_client::ConnectorServiceClient;
use risingwave_pb::connector_service::sink_coordinator_stream_request::{
    CommitMetadata, StartCoordinator,
};
use risingwave_pb::connector_service::sink_writer_stream_request::write_batch::Payload;
use risingwave_pb::connector_service::sink_writer_stream_request::{
    Barrier, Request as SinkRequest, StartSink, WriteBatch,
};
use risingwave_pb::connector_service::sink_writer_stream_response::CommitResponse;
use risingwave_pb::connector_service::*;
use risingwave_pb::plan_common::column_desc::GeneratedOrDefaultColumn;
use thiserror_ext::AsReport;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};
use tonic::Streaming;
use tracing::error;

use crate::error::{Result, RpcError};
use crate::{BidiStreamHandle, BidiStreamReceiver, BidiStreamSender};

#[derive(Clone, Debug)]
pub struct ConnectorClient {
    rpc_client: ConnectorServiceClient<Channel>,
    endpoint: String,
}

pub type SinkWriterRequestSender<REQ = SinkWriterStreamRequest> = BidiStreamSender<REQ>;
pub type SinkWriterResponseReceiver = BidiStreamReceiver<SinkWriterStreamResponse>;

pub type SinkWriterStreamHandle<REQ = SinkWriterStreamRequest> =
    BidiStreamHandle<REQ, SinkWriterStreamResponse>;

impl<REQ: From<SinkWriterStreamRequest>> SinkWriterRequestSender<REQ> {
    pub async fn write_batch(&mut self, epoch: u64, batch_id: u64, payload: Payload) -> Result<()> {
        self.send_request(SinkWriterStreamRequest {
            request: Some(SinkRequest::WriteBatch(WriteBatch {
                epoch,
                batch_id,
                payload: Some(payload),
            })),
        })
        .await
    }

    pub async fn barrier(&mut self, epoch: u64, is_checkpoint: bool) -> Result<()> {
        self.send_request(SinkWriterStreamRequest {
            request: Some(SinkRequest::Barrier(Barrier {
                epoch,
                is_checkpoint,
            })),
        })
        .await
    }
}

impl SinkWriterResponseReceiver {
    pub async fn next_commit_response(&mut self) -> Result<CommitResponse> {
        match self.next_response().await? {
            SinkWriterStreamResponse {
                response: Some(sink_writer_stream_response::Response::Commit(rsp)),
            } => Ok(rsp),
            msg => Err(RpcError::Internal(anyhow!(
                "should get Sync response but get {:?}",
                msg
            ))),
        }
    }
}

impl<REQ: From<SinkWriterStreamRequest>> SinkWriterStreamHandle<REQ> {
    pub async fn write_batch(&mut self, epoch: u64, batch_id: u64, payload: Payload) -> Result<()> {
        self.request_sender
            .write_batch(epoch, batch_id, payload)
            .await
    }

    pub async fn barrier(&mut self, epoch: u64) -> Result<()> {
        self.request_sender.barrier(epoch, false).await
    }

    pub async fn commit(&mut self, epoch: u64) -> Result<CommitResponse> {
        self.request_sender.barrier(epoch, true).await?;
        self.response_stream.next_commit_response().await
    }
}

pub type SinkCoordinatorStreamHandle =
    BidiStreamHandle<SinkCoordinatorStreamRequest, SinkCoordinatorStreamResponse>;

impl SinkCoordinatorStreamHandle {
    pub async fn commit(&mut self, epoch: u64, metadata: Vec<SinkMetadata>) -> Result<()> {
        self.send_request(SinkCoordinatorStreamRequest {
            request: Some(sink_coordinator_stream_request::Request::Commit(
                CommitMetadata { epoch, metadata },
            )),
        })
        .await?;
        match self.next_response().await? {
            SinkCoordinatorStreamResponse {
                response:
                    Some(sink_coordinator_stream_response::Response::Commit(
                        sink_coordinator_stream_response::CommitResponse {
                            epoch: response_epoch,
                        },
                    )),
            } => {
                if epoch == response_epoch {
                    Ok(())
                } else {
                    Err(RpcError::Internal(anyhow!(
                        "get different response epoch to commit epoch: {} {}",
                        epoch,
                        response_epoch
                    )))
                }
            }
            msg => Err(RpcError::Internal(anyhow!(
                "should get Commit response but get {:?}",
                msg
            ))),
        }
    }
}

impl ConnectorClient {
    pub async fn try_new(connector_endpoint: Option<&String>) -> Option<Self> {
        match connector_endpoint {
            None => None,
            Some(connector_endpoint) => match ConnectorClient::new(connector_endpoint).await {
                Ok(client) => Some(client),
                Err(e) => {
                    error!(
                        endpoint = connector_endpoint,
                        error = %e.as_report(),
                        "invalid connector endpoint",
                    );
                    None
                }
            },
        }
    }

    #[allow(clippy::unused_async)]
    pub async fn new(connector_endpoint: &String) -> Result<Self> {
        let endpoint = Endpoint::from_shared(format!("http://{}", connector_endpoint))
            .with_context(|| format!("invalid connector endpoint `{}`", connector_endpoint))?
            .initial_connection_window_size(MAX_CONNECTION_WINDOW_SIZE)
            .initial_stream_window_size(STREAM_WINDOW_SIZE)
            .connect_timeout(Duration::from_secs(5));

        let channel = {
            #[cfg(madsim)]
            {
                endpoint.connect().await?
            }
            #[cfg(not(madsim))]
            {
                endpoint.monitored_connect_lazy(
                    "grpc-connector-client",
                    TcpConfig {
                        tcp_nodelay: true,
                        keepalive_duration: None,
                    },
                )
            }
        };
        Ok(Self {
            rpc_client: ConnectorServiceClient::new(channel).max_decoding_message_size(usize::MAX),
            endpoint: connector_endpoint.to_string(),
        })
    }

    pub fn endpoint(&self) -> &String {
        &self.endpoint
    }

    /// Get source event stream
    pub async fn start_source_stream(
        &self,
        source_id: u64,
        source_type: SourceType,
        start_offset: Option<String>,
        properties: HashMap<String, String>,
        snapshot_done: bool,
        is_source_job: bool,
    ) -> Result<Streaming<GetEventStreamResponse>> {
        Ok(self
            .rpc_client
            .clone()
            .get_event_stream(GetEventStreamRequest {
                source_id,
                source_type: source_type as _,
                start_offset: start_offset.unwrap_or_default(),
                properties,
                snapshot_done,
                is_source_job,
            })
            .await
            .inspect_err(|err| {
                tracing::error!(
                    "failed to start stream for CDC source {}: {}",
                    source_id,
                    err.message()
                )
            })
            .map_err(RpcError::from_connector_status)?
            .into_inner())
    }

    /// Validate source properties
    pub async fn validate_source_properties(
        &self,
        source_id: u64,
        source_type: SourceType,
        properties: HashMap<String, String>,
        table_schema: Option<TableSchema>,
        is_source_job: bool,
        is_backfill_table: bool,
    ) -> Result<()> {
        let table_schema = table_schema.map(|mut table_schema| {
            table_schema.columns.retain(|c| {
                !matches!(
                    c.generated_or_default_column,
                    Some(GeneratedOrDefaultColumn::GeneratedColumn(_))
                )
            });
            table_schema
        });
        let response = self
            .rpc_client
            .clone()
            .validate_source(ValidateSourceRequest {
                source_id,
                source_type: source_type as _,
                properties,
                table_schema,
                is_source_job,
                is_backfill_table,
            })
            .await
            .inspect_err(|err| {
                tracing::error!("failed to validate source#{}: {}", source_id, err.message())
            })
            .map_err(RpcError::from_connector_status)?
            .into_inner();

        response.error.map_or(Ok(()), |err| {
            Err(RpcError::Internal(anyhow!(format!(
                "source cannot pass validation: {}",
                err.error_message
            ))))
        })
    }

    pub async fn start_sink_writer_stream(
        &self,
        payload_schema: Option<TableSchema>,
        sink_proto: PbSinkParam,
    ) -> Result<SinkWriterStreamHandle> {
        let mut rpc_client = self.rpc_client.clone();
        let (handle, first_rsp) = SinkWriterStreamHandle::initialize(
            SinkWriterStreamRequest {
                request: Some(SinkRequest::Start(StartSink {
                    payload_schema,
                    sink_param: Some(sink_proto),
                })),
            },
            |rx| async move {
                rpc_client
                    .sink_writer_stream(ReceiverStream::new(rx))
                    .await
                    .map(|response| {
                        response
                            .into_inner()
                            .map_err(RpcError::from_connector_status)
                    })
                    .map_err(RpcError::from_connector_status)
            },
        )
        .await?;

        match first_rsp {
            SinkWriterStreamResponse {
                response: Some(sink_writer_stream_response::Response::Start(_)),
            } => Ok(handle),
            msg => Err(RpcError::Internal(anyhow!(
                "should get start response but get {:?}",
                msg
            ))),
        }
    }

    pub async fn start_sink_coordinator_stream(
        &self,
        param: SinkParam,
    ) -> Result<SinkCoordinatorStreamHandle> {
        let mut rpc_client = self.rpc_client.clone();
        let (handle, first_rsp) = SinkCoordinatorStreamHandle::initialize(
            SinkCoordinatorStreamRequest {
                request: Some(sink_coordinator_stream_request::Request::Start(
                    StartCoordinator { param: Some(param) },
                )),
            },
            |rx| async move {
                rpc_client
                    .sink_coordinator_stream(ReceiverStream::new(rx))
                    .await
                    .map(|response| {
                        response
                            .into_inner()
                            .map_err(RpcError::from_connector_status)
                    })
                    .map_err(RpcError::from_connector_status)
            },
        )
        .await?;

        match first_rsp {
            SinkCoordinatorStreamResponse {
                response: Some(sink_coordinator_stream_response::Response::Start(_)),
            } => Ok(handle),
            msg => Err(RpcError::Internal(anyhow!(
                "should get start response but get {:?}",
                msg
            ))),
        }
    }

    pub async fn validate_sink_properties(&self, sink_param: SinkParam) -> Result<()> {
        let response = self
            .rpc_client
            .clone()
            .validate_sink(ValidateSinkRequest {
                sink_param: Some(sink_param),
            })
            .await
            .inspect_err(|err| {
                tracing::error!("failed to validate sink properties: {}", err.message())
            })
            .map_err(RpcError::from_connector_status)?
            .into_inner();
        response.error.map_or_else(
            || Ok(()), // If there is no error message, return Ok here.
            |err| {
                Err(RpcError::Internal(anyhow!(format!(
                    "sink cannot pass validation: {}",
                    err.error_message
                ))))
            },
        )
    }
}
