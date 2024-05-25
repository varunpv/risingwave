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

use std::fmt::{Debug, Display};

use risingwave_common::array::{ArrayError, ArrayRef};
use risingwave_common::types::DataType;
use risingwave_pb::PbFieldNotFound;
use thiserror::Error;
use thiserror_ext::AsReport;

/// A specialized Result type for expression operations.
pub type Result<T, E = ExprError> = std::result::Result<T, E>;

pub struct ContextUnavailable(&'static str);

impl ContextUnavailable {
    pub fn new(field: &'static str) -> Self {
        Self(field)
    }
}

impl From<ContextUnavailable> for ExprError {
    fn from(e: ContextUnavailable) -> Self {
        ExprError::Context(e.0)
    }
}

/// The error type for expression operations.
#[derive(Error, Debug)]
pub enum ExprError {
    /// A collection of multiple errors in batch evaluation.
    #[error("multiple errors:\n{1}")]
    Multiple(ArrayRef, MultiExprError),

    // Ideally "Unsupported" errors are caught by frontend. But when the match arms between
    // frontend and backend are inconsistent, we do not panic with `unreachable!`.
    #[error("Unsupported function: {0}")]
    UnsupportedFunction(String),

    #[error("Unsupported cast: {0} to {1}")]
    UnsupportedCast(DataType, DataType),

    #[error("Casting to {0} out of range")]
    CastOutOfRange(&'static str),

    #[error("Numeric out of range")]
    NumericOutOfRange,

    #[error("Numeric out of range: underflow")]
    NumericUnderflow,

    #[error("Numeric out of range: overflow")]
    NumericOverflow,

    #[error("Division by zero")]
    DivisionByZero,

    #[error("Parse error: {0}")]
    // TODO(error-handling): should prefer use error types than strings.
    Parse(Box<str>),

    #[error("Invalid parameter {name}: {reason}")]
    // TODO(error-handling): should prefer use error types than strings.
    InvalidParam {
        name: &'static str,
        reason: Box<str>,
    },

    #[error("Array error: {0}")]
    Array(
        #[from]
        #[backtrace]
        ArrayError,
    ),

    #[error("More than one row returned by {0} used as an expression")]
    MaxOneRow(&'static str),

    #[error(transparent)]
    Internal(
        #[from]
        #[backtrace]
        anyhow::Error,
    ),

    #[error("not a constant")]
    NotConstant,

    #[error("Context {0} not found")]
    Context(&'static str),

    #[error("field name must not be null")]
    FieldNameNull,

    #[error("too few arguments for format()")]
    TooFewArguments,

    #[error("invalid state: {0}")]
    InvalidState(String),

    /// Function error message returned by UDF.
    #[error("{0}")]
    Custom(String),

    /// Error from a function call.
    #[error("{0}")]
    Function(#[source] Box<dyn std::error::Error + Send + Sync>),
}

static_assertions::const_assert_eq!(std::mem::size_of::<ExprError>(), 40);

impl From<chrono::ParseError> for ExprError {
    fn from(e: chrono::ParseError) -> Self {
        Self::Parse(e.to_report_string().into())
    }
}

impl From<PbFieldNotFound> for ExprError {
    fn from(err: PbFieldNotFound) -> Self {
        Self::Internal(anyhow::anyhow!(
            "Failed to decode prost: field not found `{}`",
            err.0
        ))
    }
}

/// A collection of multiple errors.
#[derive(Error, Debug)]
pub struct MultiExprError(Box<[ExprError]>);

impl MultiExprError {
    /// Returns the first error.
    pub fn into_first(self) -> ExprError {
        self.0.into_vec().into_iter().next().expect("first error")
    }
}

impl Display for MultiExprError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            writeln!(f, "{i}: {}", e.as_report())?;
        }
        Ok(())
    }
}

impl From<Vec<ExprError>> for MultiExprError {
    fn from(v: Vec<ExprError>) -> Self {
        Self(v.into_boxed_slice())
    }
}

impl FromIterator<ExprError> for MultiExprError {
    fn from_iter<T: IntoIterator<Item = ExprError>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for MultiExprError {
    type IntoIter = std::vec::IntoIter<ExprError>;
    type Item = ExprError;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_vec().into_iter()
    }
}
