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

use std::collections::BTreeSet;

use futures_util::FutureExt;
use risingwave_common::array::{DataChunk, Op, StreamChunk};
use risingwave_common::types::{DataType, Datum};
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::{bail, must_match};
use risingwave_common_estimate_size::{EstimateSize, KvSize};
use smallvec::SmallVec;

use super::buffer::{RangeWindow, RowsWindow, WindowBuffer, WindowImpl};
use super::{BoxedWindowState, StateEvictHint, StateKey, StatePos, WindowState};
use crate::aggregate::{
    AggCall, AggregateFunction, AggregateState as AggImplState, BoxedAggregateFunction,
};
use crate::sig::FUNCTION_REGISTRY;
use crate::window_function::{FrameBounds, WindowFuncCall, WindowFuncKind};
use crate::Result;

type StateValue = SmallVec<[Datum; 2]>;

struct AggregateState<W>
where
    W: WindowImpl<Key = StateKey, Value = StateValue>,
{
    agg_func: BoxedAggregateFunction,
    agg_impl: AggImpl,
    arg_data_types: Vec<DataType>,
    buffer: WindowBuffer<W>,
    buffer_heap_size: KvSize,
}

pub(super) fn new(call: &WindowFuncCall) -> Result<BoxedWindowState> {
    if call.frame.bounds.validate().is_err() {
        bail!("the window frame must be valid");
    }
    let agg_kind = must_match!(call.kind, WindowFuncKind::Aggregate(agg_kind) => agg_kind);
    let arg_data_types = call.args.arg_types().to_vec();
    let agg_call = AggCall {
        kind: agg_kind,
        args: call.args.clone(),
        return_type: call.return_type.clone(),
        column_orders: Vec::new(), // the input is already sorted
        // TODO(rc): support filter on window function call
        filter: None,
        // TODO(rc): support distinct on window function call? PG doesn't support it either.
        distinct: false,
        direct_args: vec![],
        user_defined: None,
    };
    let agg_func_sig = FUNCTION_REGISTRY
        .get(agg_kind, &arg_data_types, &call.return_type)
        .expect("the agg func must exist");
    let agg_func = agg_func_sig.build_aggregate(&agg_call)?;
    let (agg_impl, enable_delta) =
        if agg_func_sig.is_retractable() && call.frame.exclusion.is_no_others() {
            let init_state = agg_func.create_state()?;
            (AggImpl::Incremental(init_state), true)
        } else {
            (AggImpl::Full, false)
        };

    let this = match &call.frame.bounds {
        FrameBounds::Rows(frame_bounds) => Box::new(AggregateState {
            agg_func,
            agg_impl,
            arg_data_types,
            buffer: WindowBuffer::<RowsWindow<StateKey, StateValue>>::new(
                RowsWindow::new(frame_bounds.clone()),
                call.frame.exclusion,
                enable_delta,
            ),
            buffer_heap_size: KvSize::new(),
        }) as BoxedWindowState,
        FrameBounds::Range(frame_bounds) => Box::new(AggregateState {
            agg_func,
            agg_impl,
            arg_data_types,
            buffer: WindowBuffer::<RangeWindow<StateValue>>::new(
                RangeWindow::new(frame_bounds.clone()),
                call.frame.exclusion,
                enable_delta,
            ),
            buffer_heap_size: KvSize::new(),
        }) as BoxedWindowState,
    };
    Ok(this)
}

impl<W> AggregateState<W>
where
    W: WindowImpl<Key = StateKey, Value = StateValue>,
{
    fn slide_inner(&mut self) -> StateEvictHint {
        let removed_keys: BTreeSet<_> = self
            .buffer
            .slide()
            .map(|(k, v)| {
                v.iter().for_each(|arg| {
                    self.buffer_heap_size.sub_val(arg);
                });
                self.buffer_heap_size.sub_val(&k);
                k
            })
            .collect();
        if removed_keys.is_empty() {
            StateEvictHint::CannotEvict(
                self.buffer
                    .smallest_key()
                    .expect("sliding without removing, must have some entry in the buffer")
                    .clone(),
            )
        } else {
            StateEvictHint::CanEvict(removed_keys)
        }
    }
}

impl<W> WindowState for AggregateState<W>
where
    W: WindowImpl<Key = StateKey, Value = StateValue>,
{
    fn append(&mut self, key: StateKey, args: SmallVec<[Datum; 2]>) {
        args.iter().for_each(|arg| {
            self.buffer_heap_size.add_val(arg);
        });
        self.buffer_heap_size.add_val(&key);
        self.buffer.append(key, args);
    }

    fn curr_window(&self) -> StatePos<'_> {
        let window = self.buffer.curr_window();
        StatePos {
            key: window.key,
            is_ready: window.following_saturated,
        }
    }

    fn slide(&mut self) -> Result<(Datum, StateEvictHint)> {
        let wrapper = AggregatorWrapper {
            agg_func: self.agg_func.as_ref(),
            arg_data_types: &self.arg_data_types,
        };
        let output = match self.agg_impl {
            AggImpl::Full => wrapper.aggregate(self.buffer.curr_window_values()),
            AggImpl::Incremental(ref mut state) => {
                wrapper.update(state, self.buffer.consume_curr_window_values_delta())
            }
        }?;
        let evict_hint = self.slide_inner();
        Ok((output, evict_hint))
    }

    fn slide_no_output(&mut self) -> Result<StateEvictHint> {
        match self.agg_impl {
            AggImpl::Full => {}
            AggImpl::Incremental(ref mut state) => {
                // for incremental agg, we need to update the state even if the caller doesn't need
                // the output
                let wrapper = AggregatorWrapper {
                    agg_func: self.agg_func.as_ref(),
                    arg_data_types: &self.arg_data_types,
                };
                wrapper.update(state, self.buffer.consume_curr_window_values_delta())?;
            }
        };
        Ok(self.slide_inner())
    }
}

impl<W> EstimateSize for AggregateState<W>
where
    W: WindowImpl<Key = StateKey, Value = StateValue>,
{
    fn estimated_heap_size(&self) -> usize {
        // estimate `VecDeque` of `StreamWindowBuffer` internal size
        // https://github.com/risingwavelabs/risingwave/issues/9713
        self.arg_data_types.estimated_heap_size() + self.buffer_heap_size.size()
    }
}

enum AggImpl {
    Incremental(AggImplState),
    Full,
}

struct AggregatorWrapper<'a> {
    agg_func: &'a dyn AggregateFunction,
    arg_data_types: &'a [DataType],
}

impl AggregatorWrapper<'_> {
    fn aggregate<V>(&self, values: impl IntoIterator<Item = V>) -> Result<Datum>
    where
        V: AsRef<[Datum]>,
    {
        let mut state = self.agg_func.create_state()?;
        self.update(
            &mut state,
            values.into_iter().map(|args| (Op::Insert, args)),
        )
    }

    fn update<V>(
        &self,
        state: &mut AggImplState,
        delta: impl IntoIterator<Item = (Op, V)>,
    ) -> Result<Datum>
    where
        V: AsRef<[Datum]>,
    {
        let mut args_builders = self
            .arg_data_types
            .iter()
            .map(|data_type| data_type.create_array_builder(0 /* bad! */))
            .collect::<Vec<_>>();
        let mut ops = Vec::new();
        let mut n_rows = 0;
        for (op, value) in delta {
            n_rows += 1;
            ops.push(op);
            for (builder, datum) in args_builders.iter_mut().zip_eq_fast(value.as_ref()) {
                builder.append(datum);
            }
        }
        let columns = args_builders
            .into_iter()
            .map(|builder| builder.finish().into())
            .collect::<Vec<_>>();
        let chunk = StreamChunk::from_parts(ops, DataChunk::new(columns, n_rows));

        self.agg_func
            .update(state, &chunk)
            .now_or_never()
            .expect("we don't support UDAF currently, so the function should return immediately")?;
        self.agg_func
            .get_result(state)
            .now_or_never()
            .expect("we don't support UDAF currently, so the function should return immediately")
    }
}

#[cfg(test)]
mod tests {
    // TODO(rc): need to add some unit tests
}
