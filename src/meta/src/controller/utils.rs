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

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::anyhow;
use itertools::Itertools;
use risingwave_meta_model_migration::WithQuery;
use risingwave_meta_model_v2::actor::ActorStatus;
use risingwave_meta_model_v2::fragment::DistributionType;
use risingwave_meta_model_v2::object::ObjectType;
use risingwave_meta_model_v2::prelude::*;
use risingwave_meta_model_v2::{
    actor, actor_dispatcher, connection, database, fragment, function, index, object,
    object_dependency, schema, sink, source, subscription, table, user, user_privilege, view,
    ActorId, DataTypeArray, DatabaseId, FragmentId, FragmentVnodeMapping, I32Array, ObjectId,
    PrivilegeId, SchemaId, SourceId, StreamNode, UserId,
};
use risingwave_pb::catalog::{PbConnection, PbFunction, PbSubscription};
use risingwave_pb::meta::PbFragmentParallelUnitMapping;
use risingwave_pb::stream_plan::stream_node::NodeBody;
use risingwave_pb::stream_plan::{PbFragmentTypeFlag, PbStreamNode, StreamSource};
use risingwave_pb::user::grant_privilege::{PbAction, PbActionWithGrantOption, PbObject};
use risingwave_pb::user::{PbGrantPrivilege, PbUserInfo};
use sea_orm::sea_query::{
    Alias, CommonTableExpression, Expr, Query, QueryStatementBuilder, SelectStatement, UnionType,
    WithClause,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DerivePartialModel, EntityTrait, FromQueryResult, JoinType,
    Order, PaginatorTrait, QueryFilter, QuerySelect, RelationTrait, Statement,
};

use crate::{MetaError, MetaResult};

/// This function will construct a query using recursive cte to find all objects[(id, `obj_type`)] that are used by the given object.
///
/// # Examples
///
/// ```
/// use risingwave_meta::controller::utils::construct_obj_dependency_query;
/// use sea_orm::sea_query::*;
/// use sea_orm::*;
///
/// let query = construct_obj_dependency_query(1);
///
/// assert_eq!(
///     query.to_string(MysqlQueryBuilder),
///     r#"WITH RECURSIVE `used_by_object_ids` (`used_by`) AS (SELECT `used_by` FROM `object_dependency` WHERE `object_dependency`.`oid` = 1 UNION ALL (SELECT `object_dependency`.`used_by` FROM `object_dependency` INNER JOIN `used_by_object_ids` ON `used_by_object_ids`.`used_by` = `oid`)) SELECT DISTINCT `oid`, `obj_type`, `schema_id`, `database_id` FROM `used_by_object_ids` INNER JOIN `object` ON `used_by_object_ids`.`used_by` = `oid` ORDER BY `oid` DESC"#
/// );
/// assert_eq!(
///     query.to_string(PostgresQueryBuilder),
///     r#"WITH RECURSIVE "used_by_object_ids" ("used_by") AS (SELECT "used_by" FROM "object_dependency" WHERE "object_dependency"."oid" = 1 UNION ALL (SELECT "object_dependency"."used_by" FROM "object_dependency" INNER JOIN "used_by_object_ids" ON "used_by_object_ids"."used_by" = "oid")) SELECT DISTINCT "oid", "obj_type", "schema_id", "database_id" FROM "used_by_object_ids" INNER JOIN "object" ON "used_by_object_ids"."used_by" = "oid" ORDER BY "oid" DESC"#
/// );
/// assert_eq!(
///     query.to_string(SqliteQueryBuilder),
///     r#"WITH RECURSIVE "used_by_object_ids" ("used_by") AS (SELECT "used_by" FROM "object_dependency" WHERE "object_dependency"."oid" = 1 UNION ALL SELECT "object_dependency"."used_by" FROM "object_dependency" INNER JOIN "used_by_object_ids" ON "used_by_object_ids"."used_by" = "oid") SELECT DISTINCT "oid", "obj_type", "schema_id", "database_id" FROM "used_by_object_ids" INNER JOIN "object" ON "used_by_object_ids"."used_by" = "oid" ORDER BY "oid" DESC"#
/// );
/// ```
pub fn construct_obj_dependency_query(obj_id: ObjectId) -> WithQuery {
    let cte_alias = Alias::new("used_by_object_ids");
    let cte_return_alias = Alias::new("used_by");

    let mut base_query = SelectStatement::new()
        .column(object_dependency::Column::UsedBy)
        .from(ObjectDependency)
        .and_where(object_dependency::Column::Oid.eq(obj_id))
        .to_owned();

    let cte_referencing = Query::select()
        .column((ObjectDependency, object_dependency::Column::UsedBy))
        .from(ObjectDependency)
        .inner_join(
            cte_alias.clone(),
            Expr::col((cte_alias.clone(), cte_return_alias.clone()))
                .equals(object_dependency::Column::Oid),
        )
        .to_owned();

    let common_table_expr = CommonTableExpression::new()
        .query(base_query.union(UnionType::All, cte_referencing).to_owned())
        .column(cte_return_alias.clone())
        .table_name(cte_alias.clone())
        .to_owned();

    SelectStatement::new()
        .distinct()
        .columns([
            object::Column::Oid,
            object::Column::ObjType,
            object::Column::SchemaId,
            object::Column::DatabaseId,
        ])
        .from(cte_alias.clone())
        .inner_join(
            Object,
            Expr::col((cte_alias, cte_return_alias.clone())).equals(object::Column::Oid),
        )
        .order_by(object::Column::Oid, Order::Desc)
        .to_owned()
        .with(
            WithClause::new()
                .recursive(true)
                .cte(common_table_expr)
                .to_owned(),
        )
        .to_owned()
}

/// This function will construct a query using recursive cte to find if dependent objects are already relying on the target table.
///
/// # Examples
///
/// ```
/// use risingwave_meta::controller::utils::construct_sink_cycle_check_query;
/// use sea_orm::sea_query::*;
/// use sea_orm::*;
///
/// let query = construct_sink_cycle_check_query(1, vec![2, 3]);
///
/// assert_eq!(
///     query.to_string(MysqlQueryBuilder),
///     r#"WITH RECURSIVE `used_by_object_ids_with_sink` (`oid`, `used_by`) AS (SELECT `oid`, `used_by` FROM `object_dependency` WHERE `object_dependency`.`oid` = 1 UNION ALL (SELECT `obj_dependency_with_sink`.`oid`, `obj_dependency_with_sink`.`used_by` FROM (SELECT `oid`, `used_by` FROM `object_dependency` UNION ALL (SELECT `sink_id`, `target_table` FROM `sink` WHERE `sink`.`target_table` IS NOT NULL)) AS `obj_dependency_with_sink` INNER JOIN `used_by_object_ids_with_sink` ON `used_by_object_ids_with_sink`.`used_by` = `obj_dependency_with_sink`.`oid` WHERE `used_by_object_ids_with_sink`.`used_by` <> `used_by_object_ids_with_sink`.`oid`)) SELECT COUNT(`used_by_object_ids_with_sink`.`used_by`) FROM `used_by_object_ids_with_sink` WHERE `used_by_object_ids_with_sink`.`used_by` IN (2, 3)"#
/// );
/// assert_eq!(
///     query.to_string(PostgresQueryBuilder),
///     r#"WITH RECURSIVE "used_by_object_ids_with_sink" ("oid", "used_by") AS (SELECT "oid", "used_by" FROM "object_dependency" WHERE "object_dependency"."oid" = 1 UNION ALL (SELECT "obj_dependency_with_sink"."oid", "obj_dependency_with_sink"."used_by" FROM (SELECT "oid", "used_by" FROM "object_dependency" UNION ALL (SELECT "sink_id", "target_table" FROM "sink" WHERE "sink"."target_table" IS NOT NULL)) AS "obj_dependency_with_sink" INNER JOIN "used_by_object_ids_with_sink" ON "used_by_object_ids_with_sink"."used_by" = "obj_dependency_with_sink"."oid" WHERE "used_by_object_ids_with_sink"."used_by" <> "used_by_object_ids_with_sink"."oid")) SELECT COUNT("used_by_object_ids_with_sink"."used_by") FROM "used_by_object_ids_with_sink" WHERE "used_by_object_ids_with_sink"."used_by" IN (2, 3)"#
/// );
/// assert_eq!(
///     query.to_string(SqliteQueryBuilder),
///     r#"WITH RECURSIVE "used_by_object_ids_with_sink" ("oid", "used_by") AS (SELECT "oid", "used_by" FROM "object_dependency" WHERE "object_dependency"."oid" = 1 UNION ALL SELECT "obj_dependency_with_sink"."oid", "obj_dependency_with_sink"."used_by" FROM (SELECT "oid", "used_by" FROM "object_dependency" UNION ALL SELECT "sink_id", "target_table" FROM "sink" WHERE "sink"."target_table" IS NOT NULL) AS "obj_dependency_with_sink" INNER JOIN "used_by_object_ids_with_sink" ON "used_by_object_ids_with_sink"."used_by" = "obj_dependency_with_sink"."oid" WHERE "used_by_object_ids_with_sink"."used_by" <> "used_by_object_ids_with_sink"."oid") SELECT COUNT("used_by_object_ids_with_sink"."used_by") FROM "used_by_object_ids_with_sink" WHERE "used_by_object_ids_with_sink"."used_by" IN (2, 3)"#
/// );
/// ```
pub fn construct_sink_cycle_check_query(
    target_table: ObjectId,
    dependent_objects: Vec<ObjectId>,
) -> WithQuery {
    let cte_alias = Alias::new("used_by_object_ids_with_sink");
    let depend_alias = Alias::new("obj_dependency_with_sink");

    let mut base_query = SelectStatement::new()
        .columns([
            object_dependency::Column::Oid,
            object_dependency::Column::UsedBy,
        ])
        .from(ObjectDependency)
        .and_where(object_dependency::Column::Oid.eq(target_table))
        .to_owned();

    let query_sink_deps = SelectStatement::new()
        .columns([sink::Column::SinkId, sink::Column::TargetTable])
        .from(Sink)
        .and_where(sink::Column::TargetTable.is_not_null())
        .to_owned();

    let cte_referencing = Query::select()
        .column((depend_alias.clone(), object_dependency::Column::Oid))
        .column((depend_alias.clone(), object_dependency::Column::UsedBy))
        .from_subquery(
            SelectStatement::new()
                .columns([
                    object_dependency::Column::Oid,
                    object_dependency::Column::UsedBy,
                ])
                .from(ObjectDependency)
                .union(UnionType::All, query_sink_deps)
                .to_owned(),
            depend_alias.clone(),
        )
        .inner_join(
            cte_alias.clone(),
            Expr::col((cte_alias.clone(), object_dependency::Column::UsedBy)).eq(Expr::col((
                depend_alias.clone(),
                object_dependency::Column::Oid,
            ))),
        )
        .and_where(
            Expr::col((cte_alias.clone(), object_dependency::Column::UsedBy)).ne(Expr::col((
                cte_alias.clone(),
                object_dependency::Column::Oid,
            ))),
        )
        .to_owned();

    let common_table_expr = CommonTableExpression::new()
        .query(base_query.union(UnionType::All, cte_referencing).to_owned())
        .columns([
            object_dependency::Column::Oid,
            object_dependency::Column::UsedBy,
        ])
        .table_name(cte_alias.clone())
        .to_owned();

    SelectStatement::new()
        .expr(Expr::col((cte_alias.clone(), object_dependency::Column::UsedBy)).count())
        .from(cte_alias.clone())
        .and_where(
            Expr::col((cte_alias.clone(), object_dependency::Column::UsedBy))
                .is_in(dependent_objects),
        )
        .to_owned()
        .with(
            WithClause::new()
                .recursive(true)
                .cte(common_table_expr)
                .to_owned(),
        )
        .to_owned()
}

#[derive(Clone, DerivePartialModel, FromQueryResult)]
#[sea_orm(entity = "Object")]
pub struct PartialObject {
    pub oid: ObjectId,
    pub obj_type: ObjectType,
    pub schema_id: Option<SchemaId>,
    pub database_id: Option<DatabaseId>,
}

#[derive(Clone, DerivePartialModel, FromQueryResult)]
#[sea_orm(entity = "Fragment")]
pub struct PartialFragmentStateTables {
    pub fragment_id: FragmentId,
    pub job_id: ObjectId,
    pub state_table_ids: I32Array,
}

#[derive(Clone, DerivePartialModel, FromQueryResult)]
#[sea_orm(entity = "Actor")]
pub struct PartialActorLocation {
    pub actor_id: ActorId,
    pub fragment_id: FragmentId,
    pub parallel_unit_id: i32,
    pub status: ActorStatus,
}

#[derive(FromQueryResult)]
pub struct FragmentDesc {
    pub fragment_id: FragmentId,
    pub job_id: ObjectId,
    pub fragment_type_mask: i32,
    pub distribution_type: DistributionType,
    pub state_table_ids: I32Array,
    pub upstream_fragment_id: I32Array,
    pub parallelism: i64,
}

/// List all objects that are using the given one in a cascade way. It runs a recursive CTE to find all the dependencies.
pub async fn get_referring_objects_cascade<C>(
    obj_id: ObjectId,
    db: &C,
) -> MetaResult<Vec<PartialObject>>
where
    C: ConnectionTrait,
{
    let query = construct_obj_dependency_query(obj_id);
    let (sql, values) = query.build_any(&*db.get_database_backend().get_query_builder());
    let objects = PartialObject::find_by_statement(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        values,
    ))
    .all(db)
    .await?;
    Ok(objects)
}

/// Check if create a sink with given dependent objects into the target table will cause a cycle, return true if it will.
pub async fn check_sink_into_table_cycle<C>(
    target_table: ObjectId,
    dependent_objs: Vec<ObjectId>,
    db: &C,
) -> MetaResult<bool>
where
    C: ConnectionTrait,
{
    if dependent_objs.is_empty() {
        return Ok(false);
    }

    let query = construct_sink_cycle_check_query(target_table, dependent_objs);
    let (sql, values) = query.build_any(&*db.get_database_backend().get_query_builder());

    let res = db
        .query_one(Statement::from_sql_and_values(
            db.get_database_backend(),
            sql,
            values,
        ))
        .await?
        .unwrap();

    let cnt: i64 = res.try_get_by(0)?;

    Ok(cnt != 0)
}

/// `ensure_object_id` ensures the existence of target object in the cluster.
pub async fn ensure_object_id<C>(
    object_type: ObjectType,
    obj_id: ObjectId,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Object::find_by_id(obj_id).count(db).await?;
    if count == 0 {
        return Err(MetaError::catalog_id_not_found(
            object_type.as_str(),
            obj_id,
        ));
    }
    Ok(())
}

/// `ensure_user_id` ensures the existence of target user in the cluster.
pub async fn ensure_user_id<C>(user_id: UserId, db: &C) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = User::find_by_id(user_id).count(db).await?;
    if count == 0 {
        return Err(anyhow!("user {} was concurrently dropped", user_id).into());
    }
    Ok(())
}

/// `check_database_name_duplicate` checks whether the database name is already used in the cluster.
pub async fn check_database_name_duplicate<C>(name: &str, db: &C) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Database::find()
        .filter(database::Column::Name.eq(name))
        .count(db)
        .await?;
    if count > 0 {
        assert_eq!(count, 1);
        return Err(MetaError::catalog_duplicated("database", name));
    }
    Ok(())
}

/// `check_function_signature_duplicate` checks whether the function name and its signature is already used in the target namespace.
pub async fn check_function_signature_duplicate<C>(
    pb_function: &PbFunction,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Function::find()
        .inner_join(Object)
        .filter(
            object::Column::DatabaseId
                .eq(pb_function.database_id as DatabaseId)
                .and(object::Column::SchemaId.eq(pb_function.schema_id as SchemaId))
                .and(function::Column::Name.eq(&pb_function.name))
                .and(
                    function::Column::ArgTypes
                        .eq(DataTypeArray::from(pb_function.arg_types.clone())),
                ),
        )
        .count(db)
        .await?;
    if count > 0 {
        assert_eq!(count, 1);
        return Err(MetaError::catalog_duplicated("function", &pb_function.name));
    }
    Ok(())
}

/// `check_connection_name_duplicate` checks whether the connection name is already used in the target namespace.
pub async fn check_connection_name_duplicate<C>(
    pb_connection: &PbConnection,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Connection::find()
        .inner_join(Object)
        .filter(
            object::Column::DatabaseId
                .eq(pb_connection.database_id as DatabaseId)
                .and(object::Column::SchemaId.eq(pb_connection.schema_id as SchemaId))
                .and(connection::Column::Name.eq(&pb_connection.name)),
        )
        .count(db)
        .await?;
    if count > 0 {
        assert_eq!(count, 1);
        return Err(MetaError::catalog_duplicated(
            "connection",
            &pb_connection.name,
        ));
    }
    Ok(())
}

pub async fn check_subscription_name_duplicate<C>(
    pb_subscription: &PbSubscription,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Subscription::find()
        .inner_join(Object)
        .filter(
            object::Column::DatabaseId
                .eq(pb_subscription.database_id as DatabaseId)
                .and(object::Column::SchemaId.eq(pb_subscription.schema_id as SchemaId))
                .and(subscription::Column::Name.eq(&pb_subscription.name)),
        )
        .count(db)
        .await?;
    if count > 0 {
        assert_eq!(count, 1);
        return Err(MetaError::catalog_duplicated(
            "subscription",
            &pb_subscription.name,
        ));
    }
    Ok(())
}

/// `check_user_name_duplicate` checks whether the user is already existed in the cluster.
pub async fn check_user_name_duplicate<C>(name: &str, db: &C) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = User::find()
        .filter(user::Column::Name.eq(name))
        .count(db)
        .await?;
    if count > 0 {
        assert_eq!(count, 1);
        return Err(MetaError::catalog_duplicated("user", name));
    }
    Ok(())
}

/// `check_relation_name_duplicate` checks whether the relation name is already used in the target namespace.
pub async fn check_relation_name_duplicate<C>(
    name: &str,
    database_id: DatabaseId,
    schema_id: SchemaId,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    macro_rules! check_duplicated {
        ($obj_type:expr, $entity:ident, $table:ident) => {
            let count = Object::find()
                .inner_join($entity)
                .filter(
                    object::Column::DatabaseId
                        .eq(Some(database_id))
                        .and(object::Column::SchemaId.eq(Some(schema_id)))
                        .and($table::Column::Name.eq(name)),
                )
                .count(db)
                .await?;
            if count != 0 {
                return Err(MetaError::catalog_duplicated($obj_type.as_str(), name));
            }
        };
    }
    check_duplicated!(ObjectType::Table, Table, table);
    check_duplicated!(ObjectType::Source, Source, source);
    check_duplicated!(ObjectType::Sink, Sink, sink);
    check_duplicated!(ObjectType::Index, Index, index);
    check_duplicated!(ObjectType::View, View, view);

    Ok(())
}

/// `check_schema_name_duplicate` checks whether the schema name is already used in the target database.
pub async fn check_schema_name_duplicate<C>(
    name: &str,
    database_id: DatabaseId,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Object::find()
        .inner_join(Schema)
        .filter(
            object::Column::ObjType
                .eq(ObjectType::Schema)
                .and(object::Column::DatabaseId.eq(Some(database_id)))
                .and(schema::Column::Name.eq(name)),
        )
        .count(db)
        .await?;
    if count != 0 {
        return Err(MetaError::catalog_duplicated("schema", name));
    }

    Ok(())
}

/// `ensure_object_not_refer` ensures that object are not used by any other ones except indexes.
pub async fn ensure_object_not_refer<C>(
    object_type: ObjectType,
    object_id: ObjectId,
    db: &C,
) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    // Ignore indexes.
    let count = if object_type == ObjectType::Table {
        ObjectDependency::find()
            .join(
                JoinType::InnerJoin,
                object_dependency::Relation::Object1.def(),
            )
            .filter(
                object_dependency::Column::Oid
                    .eq(object_id)
                    .and(object::Column::ObjType.ne(ObjectType::Index)),
            )
            .count(db)
            .await?
    } else {
        ObjectDependency::find()
            .filter(object_dependency::Column::Oid.eq(object_id))
            .count(db)
            .await?
    };
    if count != 0 {
        return Err(MetaError::permission_denied(format!(
            "{} used by {} other objects.",
            object_type.as_str(),
            count
        )));
    }
    Ok(())
}

/// List all objects that are using the given one.
pub async fn get_referring_objects<C>(object_id: ObjectId, db: &C) -> MetaResult<Vec<PartialObject>>
where
    C: ConnectionTrait,
{
    let objs = ObjectDependency::find()
        .filter(object_dependency::Column::Oid.eq(object_id))
        .join(
            JoinType::InnerJoin,
            object_dependency::Relation::Object1.def(),
        )
        .into_partial_model()
        .all(db)
        .await?;

    Ok(objs)
}

/// `ensure_schema_empty` ensures that the schema is empty, used by `DROP SCHEMA`.
pub async fn ensure_schema_empty<C>(schema_id: SchemaId, db: &C) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = Object::find()
        .filter(object::Column::SchemaId.eq(Some(schema_id)))
        .count(db)
        .await?;
    if count != 0 {
        return Err(MetaError::permission_denied("schema is not empty"));
    }

    Ok(())
}

/// `list_user_info_by_ids` lists all users' info by their ids.
pub async fn list_user_info_by_ids<C>(user_ids: Vec<UserId>, db: &C) -> MetaResult<Vec<PbUserInfo>>
where
    C: ConnectionTrait,
{
    let mut user_infos = vec![];
    for user_id in user_ids {
        let user = User::find_by_id(user_id)
            .one(db)
            .await?
            .ok_or_else(|| MetaError::catalog_id_not_found("user", user_id))?;
        let mut user_info: PbUserInfo = user.into();
        user_info.grant_privileges = get_user_privilege(user_id, db).await?;
        user_infos.push(user_info);
    }
    Ok(user_infos)
}

/// `get_object_owner` returns the owner of the given object.
pub async fn get_object_owner<C>(object_id: ObjectId, db: &C) -> MetaResult<UserId>
where
    C: ConnectionTrait,
{
    let obj_owner: UserId = Object::find_by_id(object_id)
        .select_only()
        .column(object::Column::OwnerId)
        .into_tuple()
        .one(db)
        .await?
        .ok_or_else(|| MetaError::catalog_id_not_found("object", object_id))?;
    Ok(obj_owner)
}

/// `construct_privilege_dependency_query` constructs a query to find all privileges that are dependent on the given one.
///
/// # Examples
///
/// ```
/// use risingwave_meta::controller::utils::construct_privilege_dependency_query;
/// use sea_orm::sea_query::*;
/// use sea_orm::*;
///
/// let query = construct_privilege_dependency_query(vec![1, 2, 3]);
///
/// assert_eq!(
///    query.to_string(MysqlQueryBuilder),
///   r#"WITH RECURSIVE `granted_privilege_ids` (`id`, `user_id`) AS (SELECT `id`, `user_id` FROM `user_privilege` WHERE `user_privilege`.`id` IN (1, 2, 3) UNION ALL (SELECT `user_privilege`.`id`, `user_privilege`.`user_id` FROM `user_privilege` INNER JOIN `granted_privilege_ids` ON `granted_privilege_ids`.`id` = `dependent_id`)) SELECT `id`, `user_id` FROM `granted_privilege_ids`"#
/// );
/// assert_eq!(
///   query.to_string(PostgresQueryBuilder),
///  r#"WITH RECURSIVE "granted_privilege_ids" ("id", "user_id") AS (SELECT "id", "user_id" FROM "user_privilege" WHERE "user_privilege"."id" IN (1, 2, 3) UNION ALL (SELECT "user_privilege"."id", "user_privilege"."user_id" FROM "user_privilege" INNER JOIN "granted_privilege_ids" ON "granted_privilege_ids"."id" = "dependent_id")) SELECT "id", "user_id" FROM "granted_privilege_ids""#
/// );
/// assert_eq!(
///  query.to_string(SqliteQueryBuilder),
///  r#"WITH RECURSIVE "granted_privilege_ids" ("id", "user_id") AS (SELECT "id", "user_id" FROM "user_privilege" WHERE "user_privilege"."id" IN (1, 2, 3) UNION ALL SELECT "user_privilege"."id", "user_privilege"."user_id" FROM "user_privilege" INNER JOIN "granted_privilege_ids" ON "granted_privilege_ids"."id" = "dependent_id") SELECT "id", "user_id" FROM "granted_privilege_ids""#
/// );
/// ```
pub fn construct_privilege_dependency_query(ids: Vec<PrivilegeId>) -> WithQuery {
    let cte_alias = Alias::new("granted_privilege_ids");
    let cte_return_privilege_alias = Alias::new("id");
    let cte_return_user_alias = Alias::new("user_id");

    let mut base_query = SelectStatement::new()
        .columns([user_privilege::Column::Id, user_privilege::Column::UserId])
        .from(UserPrivilege)
        .and_where(user_privilege::Column::Id.is_in(ids))
        .to_owned();

    let cte_referencing = Query::select()
        .columns([
            (UserPrivilege, user_privilege::Column::Id),
            (UserPrivilege, user_privilege::Column::UserId),
        ])
        .from(UserPrivilege)
        .inner_join(
            cte_alias.clone(),
            Expr::col((cte_alias.clone(), cte_return_privilege_alias.clone()))
                .equals(user_privilege::Column::DependentId),
        )
        .to_owned();

    let common_table_expr = CommonTableExpression::new()
        .query(base_query.union(UnionType::All, cte_referencing).to_owned())
        .columns([
            cte_return_privilege_alias.clone(),
            cte_return_user_alias.clone(),
        ])
        .table_name(cte_alias.clone())
        .to_owned();

    SelectStatement::new()
        .columns([cte_return_privilege_alias, cte_return_user_alias])
        .from(cte_alias.clone())
        .to_owned()
        .with(
            WithClause::new()
                .recursive(true)
                .cte(common_table_expr)
                .to_owned(),
        )
        .to_owned()
}

#[derive(Clone, DerivePartialModel, FromQueryResult)]
#[sea_orm(entity = "UserPrivilege")]
pub struct PartialUserPrivilege {
    pub id: PrivilegeId,
    pub user_id: UserId,
}

pub async fn get_referring_privileges_cascade<C>(
    ids: Vec<PrivilegeId>,
    db: &C,
) -> MetaResult<Vec<PartialUserPrivilege>>
where
    C: ConnectionTrait,
{
    let query = construct_privilege_dependency_query(ids);
    let (sql, values) = query.build_any(&*db.get_database_backend().get_query_builder());
    let privileges = PartialUserPrivilege::find_by_statement(Statement::from_sql_and_values(
        db.get_database_backend(),
        sql,
        values,
    ))
    .all(db)
    .await?;

    Ok(privileges)
}

/// `ensure_privileges_not_referred` ensures that the privileges are not granted to any other users.
pub async fn ensure_privileges_not_referred<C>(ids: Vec<PrivilegeId>, db: &C) -> MetaResult<()>
where
    C: ConnectionTrait,
{
    let count = UserPrivilege::find()
        .filter(user_privilege::Column::DependentId.is_in(ids))
        .count(db)
        .await?;
    if count != 0 {
        return Err(MetaError::permission_denied(format!(
            "privileges granted to {} other ones.",
            count
        )));
    }
    Ok(())
}

/// `get_user_privilege` returns the privileges of the given user.
pub async fn get_user_privilege<C>(user_id: UserId, db: &C) -> MetaResult<Vec<PbGrantPrivilege>>
where
    C: ConnectionTrait,
{
    let user_privileges = UserPrivilege::find()
        .find_also_related(Object)
        .filter(user_privilege::Column::UserId.eq(user_id))
        .all(db)
        .await?;
    Ok(user_privileges
        .into_iter()
        .map(|(privilege, object)| {
            let object = object.unwrap();
            let oid = object.oid as _;
            let obj = match object.obj_type {
                ObjectType::Database => PbObject::DatabaseId(oid),
                ObjectType::Schema => PbObject::SchemaId(oid),
                ObjectType::Table | ObjectType::Index => PbObject::TableId(oid),
                ObjectType::Source => PbObject::SourceId(oid),
                ObjectType::Sink => PbObject::SinkId(oid),
                ObjectType::View => PbObject::ViewId(oid),
                ObjectType::Function => PbObject::FunctionId(oid),
                ObjectType::Connection => unreachable!("connection is not supported yet"),
                ObjectType::Subscription => PbObject::SubscriptionId(oid),
            };
            PbGrantPrivilege {
                action_with_opts: vec![PbActionWithGrantOption {
                    action: PbAction::from(privilege.action) as _,
                    with_grant_option: privilege.with_grant_option,
                    granted_by: privilege.granted_by as _,
                }],
                object: Some(obj),
            }
        })
        .collect())
}

// todo: remove it after migrated to sql backend.
pub fn extract_grant_obj_id(object: &PbObject) -> ObjectId {
    match object {
        PbObject::DatabaseId(id)
        | PbObject::SchemaId(id)
        | PbObject::TableId(id)
        | PbObject::SourceId(id)
        | PbObject::SinkId(id)
        | PbObject::ViewId(id)
        | PbObject::FunctionId(id)
        | PbObject::SubscriptionId(id) => *id as _,
        _ => unreachable!("invalid object type: {:?}", object),
    }
}

pub async fn get_actor_dispatchers<C>(
    db: &C,
    actor_ids: Vec<ActorId>,
) -> MetaResult<HashMap<ActorId, Vec<actor_dispatcher::Model>>>
where
    C: ConnectionTrait,
{
    let actor_dispatchers = ActorDispatcher::find()
        .filter(actor_dispatcher::Column::ActorId.is_in(actor_ids))
        .all(db)
        .await?;

    let mut actor_dispatchers_map = HashMap::new();
    for actor_dispatcher in actor_dispatchers {
        actor_dispatchers_map
            .entry(actor_dispatcher.actor_id)
            .or_insert_with(Vec::new)
            .push(actor_dispatcher);
    }
    Ok(actor_dispatchers_map)
}

/// `get_fragment_mappings` returns the fragment vnode mappings of the given job.
pub async fn get_fragment_mappings<C>(
    db: &C,
    job_id: ObjectId,
) -> MetaResult<Vec<PbFragmentParallelUnitMapping>>
where
    C: ConnectionTrait,
{
    let fragment_mappings: Vec<(FragmentId, FragmentVnodeMapping)> = Fragment::find()
        .select_only()
        .columns([fragment::Column::FragmentId, fragment::Column::VnodeMapping])
        .filter(fragment::Column::JobId.eq(job_id))
        .into_tuple()
        .all(db)
        .await?;

    Ok(fragment_mappings
        .into_iter()
        .map(|(fragment_id, mapping)| PbFragmentParallelUnitMapping {
            fragment_id: fragment_id as _,
            mapping: Some(mapping.to_protobuf()),
        })
        .collect())
}

/// `get_fragment_mappings_by_jobs` returns the fragment vnode mappings of the given job list.
pub async fn get_fragment_mappings_by_jobs<C>(
    db: &C,
    job_ids: Vec<ObjectId>,
) -> MetaResult<Vec<PbFragmentParallelUnitMapping>>
where
    C: ConnectionTrait,
{
    if job_ids.is_empty() {
        return Ok(vec![]);
    }

    let fragment_mappings: Vec<(FragmentId, FragmentVnodeMapping)> = Fragment::find()
        .select_only()
        .columns([fragment::Column::FragmentId, fragment::Column::VnodeMapping])
        .filter(fragment::Column::JobId.is_in(job_ids))
        .into_tuple()
        .all(db)
        .await?;

    Ok(fragment_mappings
        .into_iter()
        .map(|(fragment_id, mapping)| PbFragmentParallelUnitMapping {
            fragment_id: fragment_id as _,
            mapping: Some(mapping.to_protobuf()),
        })
        .collect())
}

/// `get_fragment_actor_ids` returns the fragment actor ids of the given fragments.
pub async fn get_fragment_actor_ids<C>(
    db: &C,
    fragment_ids: Vec<FragmentId>,
) -> MetaResult<HashMap<FragmentId, Vec<ActorId>>>
where
    C: ConnectionTrait,
{
    let fragment_actors: Vec<(FragmentId, ActorId)> = Actor::find()
        .select_only()
        .columns([actor::Column::FragmentId, actor::Column::ActorId])
        .filter(actor::Column::FragmentId.is_in(fragment_ids))
        .into_tuple()
        .all(db)
        .await?;

    Ok(fragment_actors.into_iter().into_group_map())
}

/// Find the external stream source info inside the stream node, if any.
pub fn find_stream_source(stream_node: &PbStreamNode) -> Option<&StreamSource> {
    if let Some(NodeBody::Source(source)) = &stream_node.node_body {
        if let Some(inner) = &source.source_inner {
            return Some(inner);
        }
    }

    for child in &stream_node.input {
        if let Some(source) = find_stream_source(child) {
            return Some(source);
        }
    }

    None
}

/// Resolve fragment list that are subscribing to sources and actor lists.
pub async fn resolve_source_register_info_for_jobs<C>(
    db: &C,
    streaming_jobs: Vec<ObjectId>,
) -> MetaResult<(HashMap<SourceId, BTreeSet<FragmentId>>, HashSet<ActorId>)>
where
    C: ConnectionTrait,
{
    if streaming_jobs.is_empty() {
        return Ok((HashMap::default(), HashSet::default()));
    }

    let mut fragments: Vec<(FragmentId, i32, StreamNode)> = Fragment::find()
        .select_only()
        .columns([
            fragment::Column::FragmentId,
            fragment::Column::FragmentTypeMask,
            fragment::Column::StreamNode,
        ])
        .filter(fragment::Column::JobId.is_in(streaming_jobs))
        .into_tuple()
        .all(db)
        .await?;
    let actors: Vec<ActorId> = Actor::find()
        .select_only()
        .column(actor::Column::ActorId)
        .filter(
            actor::Column::FragmentId.is_in(fragments.iter().map(|(id, _, _)| *id).collect_vec()),
        )
        .into_tuple()
        .all(db)
        .await?;

    fragments.retain(|(_, mask, _)| *mask & PbFragmentTypeFlag::Source as i32 != 0);

    let mut source_fragment_ids = HashMap::new();
    for (fragment_id, _, stream_node) in fragments {
        if let Some(source) = find_stream_source(&stream_node.to_protobuf()) {
            source_fragment_ids
                .entry(source.source_id as SourceId)
                .or_insert_with(BTreeSet::new)
                .insert(fragment_id);
        }
    }

    Ok((source_fragment_ids, actors.into_iter().collect()))
}
