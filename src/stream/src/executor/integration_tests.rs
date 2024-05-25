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

use std::sync::Mutex;

use futures_async_stream::try_stream;
use multimap::MultiMap;
use risingwave_common::array::*;
use risingwave_common::catalog::Field;
use risingwave_common::config;
use risingwave_common::types::*;
use risingwave_common::util::epoch::{test_epoch, EpochExt};
use risingwave_expr::aggregate::AggCall;
use risingwave_expr::expr::*;
use risingwave_pb::plan_common::ExprContext;
use risingwave_storage::memory::MemoryStateStore;

use super::exchange::permit::channel_for_test;
use super::*;
use crate::executor::dispatch::*;
use crate::executor::exchange::output::{BoxedOutput, LocalOutput};
use crate::executor::monitor::StreamingMetrics;
use crate::executor::test_utils::agg_executor::{
    generate_agg_schema, new_boxed_simple_agg_executor,
};
use crate::task::{LocalBarrierManager, SharedContext};

/// This test creates a merger-dispatcher pair, and run a sum. Each chunk
/// has 0~9 elements. We first insert the 10 chunks, then delete them,
/// and do this again and again.
#[tokio::test]
async fn test_merger_sum_aggr() {
    let expr_context = ExprContext {
        time_zone: String::from("UTC"),
    };

    let actor_ctx = ActorContext::for_test(0);
    // `make_actor` build an actor to do local aggregation
    let make_actor = |input_rx| {
        let input_schema = Schema {
            fields: vec![Field::unnamed(DataType::Int64)],
        };
        let input = Executor::new(
            ExecutorInfo {
                schema: input_schema,
                pk_indices: PkIndices::new(),
                identity: "ReceiverExecutor".to_string(),
            },
            ReceiverExecutor::for_test(input_rx).boxed(),
        );
        let agg_calls = vec![
            AggCall::from_pretty("(count:int8)"),
            AggCall::from_pretty("(sum:int8 $0:int8)"),
        ];
        let schema = generate_agg_schema(&input, &agg_calls, None);
        // for the local aggregator, we need two states: row count and sum
        let aggregator =
            StatelessSimpleAggExecutor::new(actor_ctx.clone(), input, schema, agg_calls).unwrap();
        let (tx, rx) = channel_for_test();
        let consumer = SenderConsumer {
            input: aggregator.boxed(),
            channel: Box::new(LocalOutput::new(233, tx)),
        };
        let actor = Actor::new(
            consumer,
            vec![],
            StreamingMetrics::unused().into(),
            actor_ctx.clone(),
            expr_context.clone(),
            LocalBarrierManager::for_test(),
        );
        (actor, rx)
    };

    // join handles of all actors
    let mut handles = vec![];

    // input and output channels of the local aggregation actors
    let mut inputs = vec![];
    let mut outputs = vec![];

    let ctx = Arc::new(SharedContext::for_test());
    let metrics = Arc::new(StreamingMetrics::unused());

    // create 17 local aggregation actors
    for _ in 0..17 {
        let (tx, rx) = channel_for_test();
        let (actor, channel) = make_actor(rx);
        outputs.push(channel);
        handles.push(tokio::spawn(actor.run()));
        inputs.push(Box::new(LocalOutput::new(233, tx)) as BoxedOutput);
    }

    // create a round robin dispatcher, which dispatches messages to the actors
    let (input, rx) = channel_for_test();
    let receiver_op = Executor::new(
        ExecutorInfo {
            // input schema of local simple agg
            schema: Schema::new(vec![Field::unnamed(DataType::Int64)]),
            pk_indices: PkIndices::new(),
            identity: "ReceiverExecutor".to_string(),
        },
        ReceiverExecutor::for_test(rx).boxed(),
    );
    let dispatcher = DispatchExecutor::new(
        receiver_op,
        vec![DispatcherImpl::RoundRobin(RoundRobinDataDispatcher::new(
            inputs,
            vec![0],
            0,
        ))],
        0,
        0,
        ctx,
        metrics,
        config::default::developer::stream_chunk_size(),
    );
    let actor = Actor::new(
        dispatcher,
        vec![],
        StreamingMetrics::unused().into(),
        actor_ctx.clone(),
        expr_context.clone(),
        LocalBarrierManager::for_test(),
    );
    handles.push(tokio::spawn(actor.run()));

    // use a merge operator to collect data from dispatchers before sending them to aggregator
    let merger = Executor::new(
        ExecutorInfo {
            // output schema of local simple agg
            schema: Schema::new(vec![
                Field::unnamed(DataType::Int64),
                Field::unnamed(DataType::Int64),
            ]),
            pk_indices: PkIndices::new(),
            identity: "MergeExecutor".to_string(),
        },
        MergeExecutor::for_test(outputs).boxed(),
    );

    // for global aggregator, we need to sum data and sum row count
    let is_append_only = false;
    let aggregator = new_boxed_simple_agg_executor(
        actor_ctx.clone(),
        MemoryStateStore::new(),
        merger,
        is_append_only,
        vec![
            AggCall::from_pretty("(sum0:int8 $0:int8)"),
            AggCall::from_pretty("(sum:int8 $1:int8)"),
            AggCall::from_pretty("(count:int8)"),
        ],
        2, // row_count_index
        vec![],
        2,
    )
    .await;

    let projection = ProjectExecutor::new(
        actor_ctx.clone(),
        aggregator,
        vec![
            // TODO: use the new streaming_if_null expression here, and add `None` tests
            NonStrictExpression::for_test(InputRefExpression::new(DataType::Int64, 1)),
        ],
        MultiMap::new(),
        vec![],
        0.0,
    );

    let items = Arc::new(Mutex::new(vec![]));
    let consumer = MockConsumer {
        input: projection.boxed(),
        data: items.clone(),
    };
    let actor = Actor::new(
        consumer,
        vec![],
        StreamingMetrics::unused().into(),
        actor_ctx.clone(),
        expr_context.clone(),
        LocalBarrierManager::for_test(),
    );
    handles.push(tokio::spawn(actor.run()));

    let mut epoch = test_epoch(1);
    input
        .send(Message::Barrier(Barrier::new_test_barrier(epoch)))
        .await
        .unwrap();
    epoch.inc_epoch();
    for j in 0..11 {
        let op = if j % 2 == 0 { Op::Insert } else { Op::Delete };
        for i in 0..10 {
            let chunk = StreamChunk::new(
                vec![op; i],
                vec![I64Array::from_iter(vec![1; i]).into_ref()],
            );
            input.send(Message::Chunk(chunk)).await.unwrap();
        }
        input
            .send(Message::Barrier(Barrier::new_test_barrier(epoch)))
            .await
            .unwrap();
        epoch.inc_epoch();
    }
    input
        .send(Message::Barrier(
            Barrier::new_test_barrier(epoch)
                .with_mutation(Mutation::Stop([0].into_iter().collect())),
        ))
        .await
        .unwrap();

    // wait for all actors
    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    let data = items.lock().unwrap();
    let array = data.last().unwrap().column_at(0).as_int64();
    assert_eq!(array.value_at(array.len() - 1), Some((0..10).sum()));
}

struct MockConsumer {
    input: Box<dyn Execute>,
    data: Arc<Mutex<Vec<StreamChunk>>>,
}

impl StreamConsumer for MockConsumer {
    type BarrierStream = impl Stream<Item = StreamResult<Barrier>> + Send;

    fn execute(self: Box<Self>) -> Self::BarrierStream {
        let mut input = self.input.execute();
        let data = self.data;
        #[try_stream]
        async move {
            while let Some(item) = input.next().await {
                match item? {
                    Message::Watermark(_) => {
                        // TODO: https://github.com/risingwavelabs/risingwave/issues/6042
                    }
                    Message::Chunk(chunk) => data.lock().unwrap().push(chunk),
                    Message::Barrier(barrier) => yield barrier,
                }
            }
        }
    }
}

/// `SenderConsumer` consumes data from input executor and send it into a channel.
pub struct SenderConsumer {
    input: Box<dyn Execute>,
    channel: BoxedOutput,
}

impl StreamConsumer for SenderConsumer {
    type BarrierStream = impl Stream<Item = StreamResult<Barrier>> + Send;

    fn execute(self: Box<Self>) -> Self::BarrierStream {
        let mut input = self.input.execute();
        let mut channel = self.channel;
        #[try_stream]
        async move {
            while let Some(item) = input.next().await {
                let msg = item?;
                let barrier = msg.as_barrier().cloned();

                channel.send(msg).await.expect("failed to send message");

                if let Some(barrier) = barrier {
                    yield barrier;
                }
            }
        }
    }
}
