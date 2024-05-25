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

use super::{
    AggCall, CorrelatedInputRef, ExprImpl, FunctionCall, FunctionCallWithLambda, InputRef, Literal,
    Parameter, Subquery, TableFunction, UserDefinedFunction, WindowFunction,
};
use crate::expr::Now;

/// By default, `ExprRewriter` simply traverses the expression tree and leaves nodes unchanged.
/// Implementations can override a subset of methods and perform transformation on some particular
/// types of expression.
pub trait ExprRewriter {
    fn rewrite_expr(&mut self, expr: ExprImpl) -> ExprImpl {
        match expr {
            ExprImpl::InputRef(inner) => self.rewrite_input_ref(*inner),
            ExprImpl::Literal(inner) => self.rewrite_literal(*inner),
            ExprImpl::FunctionCall(inner) => self.rewrite_function_call(*inner),
            ExprImpl::FunctionCallWithLambda(inner) => {
                self.rewrite_function_call_with_lambda(*inner)
            }
            ExprImpl::AggCall(inner) => self.rewrite_agg_call(*inner),
            ExprImpl::Subquery(inner) => self.rewrite_subquery(*inner),
            ExprImpl::CorrelatedInputRef(inner) => self.rewrite_correlated_input_ref(*inner),
            ExprImpl::TableFunction(inner) => self.rewrite_table_function(*inner),
            ExprImpl::WindowFunction(inner) => self.rewrite_window_function(*inner),
            ExprImpl::UserDefinedFunction(inner) => self.rewrite_user_defined_function(*inner),
            ExprImpl::Parameter(inner) => self.rewrite_parameter(*inner),
            ExprImpl::Now(inner) => self.rewrite_now(*inner),
        }
    }

    fn rewrite_function_call(&mut self, func_call: FunctionCall) -> ExprImpl {
        let (func_type, inputs, ret) = func_call.decompose();
        let inputs = inputs
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        FunctionCall::new_unchecked(func_type, inputs, ret).into()
    }

    fn rewrite_function_call_with_lambda(&mut self, func_call: FunctionCallWithLambda) -> ExprImpl {
        let (func_type, inputs, lambda_arg, ret) = func_call.into_parts();
        let inputs = inputs
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        FunctionCallWithLambda::new_unchecked(func_type, inputs, lambda_arg, ret).into()
    }

    fn rewrite_agg_call(&mut self, agg_call: AggCall) -> ExprImpl {
        let AggCall {
            agg_kind,
            return_type,
            args,
            distinct,
            order_by,
            filter,
            direct_args,
            user_defined,
        } = agg_call;
        let args = args
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        let order_by = order_by.rewrite_expr(self);
        let filter = filter.rewrite_expr(self);
        AggCall {
            agg_kind,
            return_type,
            args,
            distinct,
            order_by,
            filter,
            direct_args,
            user_defined,
        }
        .into()
    }

    fn rewrite_parameter(&mut self, parameter: Parameter) -> ExprImpl {
        parameter.into()
    }

    fn rewrite_literal(&mut self, literal: Literal) -> ExprImpl {
        literal.into()
    }

    fn rewrite_input_ref(&mut self, input_ref: InputRef) -> ExprImpl {
        input_ref.into()
    }

    fn rewrite_subquery(&mut self, subquery: Subquery) -> ExprImpl {
        subquery.into()
    }

    fn rewrite_correlated_input_ref(&mut self, input_ref: CorrelatedInputRef) -> ExprImpl {
        input_ref.into()
    }

    fn rewrite_table_function(&mut self, table_func: TableFunction) -> ExprImpl {
        let TableFunction {
            args,
            return_type,
            function_type,
            user_defined: udtf_catalog,
        } = table_func;
        let args = args
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        TableFunction {
            args,
            return_type,
            function_type,
            user_defined: udtf_catalog,
        }
        .into()
    }

    fn rewrite_window_function(&mut self, window_func: WindowFunction) -> ExprImpl {
        let WindowFunction {
            args,
            return_type,
            kind,
            partition_by,
            order_by,
            frame,
        } = window_func;
        let args = args
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        WindowFunction {
            kind,
            args,
            return_type,
            partition_by,
            order_by,
            frame,
        }
        .into()
    }

    fn rewrite_user_defined_function(&mut self, udf: UserDefinedFunction) -> ExprImpl {
        let UserDefinedFunction { args, catalog } = udf;
        let args = args
            .into_iter()
            .map(|expr| self.rewrite_expr(expr))
            .collect();
        UserDefinedFunction { args, catalog }.into()
    }

    fn rewrite_now(&mut self, now: Now) -> ExprImpl {
        now.into()
    }
}
