mod admins;
mod codec;
mod editing;
pub(crate) mod entities;
mod generated;
mod migration;
mod notifications;
mod peek;
mod posts;
mod publications;
mod review;
mod tags;
#[cfg(test)]
pub(crate) mod test_support;

use crate::model::PostId;
use anyhow::{Context, Result};
use sea_orm::DatabaseBackend;
use sea_orm::sea_query::{Expr, Query};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DatabaseTransaction, ExprTrait,
    IsolationLevel, SqliteTransactionMode, TransactionOptions, TransactionTrait,
};
use sea_orm_migration::MigratorTrait;
use teloxide::types::UserId;

pub use editing::{Applied, Rejection, Setting};
pub use generated::AppendTextOutcome;
pub use notifications::PendingNotification;
pub use peek::{PeekItem, RemoveOutcome};
pub use posts::{
    AppendOutcome, AppendRejection, EditOutcome, MoveOutcome, PostSummary, QueueOutcome,
    ReplaceMediaOutcome, ReplaceMediaRejection, StockCounts,
};
pub use publications::AttemptOutcome;
pub use review::{ApproveOutcome, ClearOutcome, PendingReview, ReviewInfo};

type Conn = DatabaseTransaction;

/// 根据数据库 URL 选择后端的异步连接池持久化层。
#[derive(Clone)]
pub struct Store {
    pub(super) db: DatabaseConnection,
}

impl Store {
    pub async fn open(database_url: &str) -> Result<Self> {
        let memory = database_url == "sqlite::memory:";
        let mut options = ConnectOptions::new(database_url);
        options
            .max_connections(if memory { 1 } else { 8 })
            .min_connections(1)
            .connect_timeout(std::time::Duration::from_secs(10))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .map_sqlx_sqlite_opts(|options| {
                options
                    .foreign_keys(true)
                    .busy_timeout(std::time::Duration::from_secs(5))
                    .pragma("journal_mode", "WAL")
            });
        let db = Database::connect(options)
            .await
            .context("failed to connect to database")?;
        migration::Migrator::up(&db, None)
            .await
            .context("failed to initialize database schema")?;
        Ok(Self { db })
    }

    async fn transaction<T, F>(&self, callback: F) -> Result<T>
    where
        F: for<'c> AsyncFnOnce(&'c Conn) -> Result<T> + Send,
        T: Send,
    {
        let backend = self.db.get_database_backend();
        let transaction = self
            .db
            .begin_with_options(TransactionOptions {
                isolation_level: (backend == DatabaseBackend::MySql)
                    .then_some(IsolationLevel::ReadCommitted),
                access_mode: None,
                sqlite_transaction_mode: Some(SqliteTransactionMode::Immediate),
            })
            .await?;
        let result = callback(&transaction).await;
        if result.is_ok() {
            transaction.commit().await?;
        } else {
            transaction.rollback().await?;
        }
        result
    }

    #[cfg(test)]
    pub async fn open_temporary() -> (tempfile::TempDir, Self) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("test.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let store = Self::open(&url).await.unwrap();
        test_support::seed(&store).await;
        (directory, store)
    }
}

/// 在读改写操作前锁定目标。MySQL/PostgreSQL 只锁定匹配的行；SQLite 在事务开始时已经
/// 占用了写锁。
async fn lock_post(conn: &Conn, post: PostId) -> Result<()> {
    use entities::posts::{Column, Entity};
    conn.execute(
        Query::update()
            .table(Entity)
            .value(Column::Id, Expr::col(Column::Id))
            .and_where(Expr::col(Column::Id).eq(post.0)),
    )
    .await?;
    Ok(())
}

async fn lock_admin(conn: &Conn, admin: UserId) -> Result<()> {
    use entities::admins::{Column, Entity};
    conn.execute(
        Query::update()
            .table(Entity)
            .value(Column::UserId, Expr::col(Column::UserId))
            .and_where(Expr::col(Column::UserId).eq(codec::user_id(admin)?)),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;
    use sea_orm::sea_query::{Alias, ColumnDef, Table};
    use sea_orm::{ActiveEnum, Iterable, TransactionTrait, TryGetable};

    async fn enum_roundtrip<E>(db: &DatabaseConnection)
    where
        E: ActiveEnum<Value = i32> + Iterable + TryGetable + Copy + PartialEq + std::fmt::Debug,
        sea_orm::Value: From<E>,
    {
        for value in E::iter() {
            let row = db
                .query_one(Query::select().expr_as(Expr::val(value), Alias::new("value")))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i32>("", "value").unwrap(), value.to_value());
            assert_eq!(row.try_get::<E>("", "value").unwrap(), value);
        }
        let row = db
            .query_one(Query::select().expr_as(Expr::val(99_i32), Alias::new("value")))
            .await
            .unwrap()
            .unwrap();
        assert!(row.try_get::<E>("", "value").is_err());
    }

    #[tokio::test]
    async fn every_persistence_enum_uses_integer_and_rejects_unknown_values() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        enum_roundtrip::<PostStatus>(&store.db).await;
        enum_roundtrip::<MediaKind>(&store.db).await;
        enum_roundtrip::<RunStatus>(&store.db).await;
        enum_roundtrip::<AttemptStatus>(&store.db).await;
        enum_roundtrip::<NotificationKind>(&store.db).await;
        enum_roundtrip::<NotificationStatus>(&store.db).await;
        enum_roundtrip::<SubmitterStatus>(&store.db).await;
        enum_roundtrip::<ReviewMessageKind>(&store.db).await;
        enum_roundtrip::<PostAuditKind>(&store.db).await;
    }

    #[tokio::test]
    async fn failed_transaction_rolls_back_and_pool_remains_usable() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        store
            .db
            .execute(
                Table::create()
                    .table(Alias::new("rollback_probe"))
                    .col(ColumnDef::new(Alias::new("id")).integer().primary_key()),
            )
            .await
            .unwrap();
        let result: Result<()> = store
            .transaction(async move |db| {
                db.execute(
                    Query::insert()
                        .into_table(Alias::new("rollback_probe"))
                        .columns([Alias::new("id")])
                        .values_panic([1_i32.into()]),
                )
                .await?;
                anyhow::bail!("intentional transaction failure")
            })
            .await;
        assert!(result.is_err());
        assert!(
            store
                .db
                .query_one(
                    Query::select()
                        .column(Alias::new("id"))
                        .from(Alias::new("rollback_probe"))
                )
                .await
                .unwrap()
                .is_none()
        );
        store
            .db
            .execute(
                Query::insert()
                    .into_table(Alias::new("rollback_probe"))
                    .columns([Alias::new("id")])
                    .values_panic([2_i32.into()]),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_transaction_rolls_back_before_connection_reuse() {
        let store = Store::open("sqlite::memory:").await.unwrap();
        store
            .db
            .execute(
                Table::create()
                    .table(Alias::new("cancel_probe"))
                    .col(ColumnDef::new(Alias::new("id")).integer().primary_key()),
            )
            .await
            .unwrap();
        let (started, ready) = tokio::sync::oneshot::channel();
        let writer = store.clone();
        let task = tokio::spawn(async move {
            writer
                .transaction(async move |db| {
                    db.execute(
                        Query::insert()
                            .into_table(Alias::new("cancel_probe"))
                            .columns([Alias::new("id")])
                            .values_panic([1_i32.into()]),
                    )
                    .await?;
                    let _ = started.send(());
                    std::future::pending::<Result<()>>().await
                })
                .await
        });
        ready.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let row = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            store.db.query_one(
                Query::select()
                    .column(Alias::new("id"))
                    .from(Alias::new("cancel_probe")),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(row.is_none());
    }

    #[tokio::test]
    async fn sqlite_pool_configures_each_connection() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("pool.db").display()
        );
        let store = Store::open(&url).await.unwrap();
        let first = store.db.begin().await.unwrap();
        let second = store.db.begin().await.unwrap();
        for connection in [&first, &second] {
            let row = connection
                .query_one_raw(sea_orm::Statement::from_string(
                    sea_orm::DbBackend::Sqlite,
                    "PRAGMA foreign_keys",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i32>("", "foreign_keys").unwrap(), 1);
            let row = connection
                .query_one_raw(sea_orm::Statement::from_string(
                    sea_orm::DbBackend::Sqlite,
                    "PRAGMA busy_timeout",
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.try_get::<i32>("", "timeout").unwrap(), 5000);
        }
        first.rollback().await.unwrap();
        second.rollback().await.unwrap();
    }
}
