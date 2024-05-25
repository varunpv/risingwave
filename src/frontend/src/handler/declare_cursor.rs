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

use pgwire::pg_field_descriptor::PgFieldDescriptor;
use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::util::epoch::Epoch;
use risingwave_sqlparser::ast::{DeclareCursorStatement, ObjectName, Query, Since, Statement};

use super::query::{gen_batch_plan_by_statement, gen_batch_plan_fragmenter};
use super::util::convert_unix_millis_to_logstore_u64;
use super::RwPgResponse;
use crate::error::{ErrorCode, Result};
use crate::handler::query::create_stream;
use crate::handler::HandlerArgs;
use crate::{Binder, OptimizerContext, PgResponseStream};

pub async fn handle_declare_cursor(
    handle_args: HandlerArgs,
    stmt: DeclareCursorStatement,
) -> Result<RwPgResponse> {
    match stmt.declare_cursor {
        risingwave_sqlparser::ast::DeclareCursor::Query(query) => {
            handle_declare_query_cursor(handle_args, stmt.cursor_name, query).await
        }
        risingwave_sqlparser::ast::DeclareCursor::Subscription(sub_name, rw_timestamp) => {
            handle_declare_subscription_cursor(
                handle_args,
                sub_name,
                stmt.cursor_name,
                rw_timestamp,
            )
            .await
        }
    }
}
async fn handle_declare_subscription_cursor(
    handle_args: HandlerArgs,
    sub_name: ObjectName,
    cursor_name: ObjectName,
    rw_timestamp: Option<Since>,
) -> Result<RwPgResponse> {
    let session = handle_args.session.clone();
    let db_name = session.database();
    let (schema_name, cursor_name) =
        Binder::resolve_schema_qualified_name(db_name, cursor_name.clone())?;

    let cursor_from_subscription_name = sub_name.0.last().unwrap().real_value().clone();
    let subscription =
        session.get_subscription_by_name(schema_name, &cursor_from_subscription_name)?;
    let table = session.get_table_by_id(&subscription.dependent_table_id)?;
    // Start the first query of cursor, which includes querying the table and querying the subscription's logstore
    let start_rw_timestamp = match rw_timestamp {
        Some(risingwave_sqlparser::ast::Since::TimestampMsNum(start_rw_timestamp)) => {
            check_cursor_unix_millis(start_rw_timestamp, subscription.retention_seconds)?;
            Some(convert_unix_millis_to_logstore_u64(start_rw_timestamp))
        }
        Some(risingwave_sqlparser::ast::Since::ProcessTime) => Some(Epoch::now().0),
        Some(risingwave_sqlparser::ast::Since::Begin) => {
            let min_unix_millis =
                Epoch::now().as_unix_millis() - subscription.retention_seconds * 1000;
            let subscription_build_millis = subscription.created_at_epoch.unwrap().as_unix_millis();
            let min_unix_millis = std::cmp::max(min_unix_millis, subscription_build_millis);
            Some(convert_unix_millis_to_logstore_u64(min_unix_millis))
        }
        None => None,
    };
    // Create cursor based on the response
    session
        .get_cursor_manager()
        .add_subscription_cursor(
            cursor_name.clone(),
            start_rw_timestamp,
            subscription,
            table,
            &handle_args,
        )
        .await?;

    Ok(PgResponse::empty_result(StatementType::DECLARE_CURSOR))
}

fn check_cursor_unix_millis(unix_millis: u64, retention_seconds: u64) -> Result<()> {
    let now = Epoch::now().as_unix_millis();
    let min_unix_millis = now - retention_seconds * 1000;
    if unix_millis > now {
        return Err(ErrorCode::CatalogError(
            "rw_timestamp is too large, need to be less than the current unix_millis"
                .to_string()
                .into(),
        )
        .into());
    }
    if unix_millis < min_unix_millis {
        return Err(ErrorCode::CatalogError("rw_timestamp is too small, need to be large than the current unix_millis - subscription's retention time".to_string().into()).into());
    }
    Ok(())
}

async fn handle_declare_query_cursor(
    handle_args: HandlerArgs,
    cursor_name: ObjectName,
    query: Box<Query>,
) -> Result<RwPgResponse> {
    let (row_stream, pg_descs) =
        create_stream_for_cursor_stmt(handle_args.clone(), Statement::Query(query)).await?;
    handle_args
        .session
        .get_cursor_manager()
        .add_query_cursor(cursor_name, row_stream, pg_descs)
        .await?;
    Ok(PgResponse::empty_result(StatementType::DECLARE_CURSOR))
}

pub async fn create_stream_for_cursor_stmt(
    handle_args: HandlerArgs,
    stmt: Statement,
) -> Result<(PgResponseStream, Vec<PgFieldDescriptor>)> {
    let session = handle_args.session.clone();
    let plan_fragmenter_result = {
        let context = OptimizerContext::from_handler_args(handle_args);
        let plan_result = gen_batch_plan_by_statement(&session, context.into(), stmt)?;
        gen_batch_plan_fragmenter(&session, plan_result)?
    };
    create_stream(session, plan_fragmenter_result, vec![]).await
}
