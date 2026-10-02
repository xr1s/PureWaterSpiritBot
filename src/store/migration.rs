//! 初始可移植 schema。整数枚举值是明确且稳定的持久化契约。

use super::entities::*;
use sea_orm::{ColumnTrait, DbBackend, EntityTrait, Iterable, PrimaryKeyToColumn, Schema};
use sea_orm_migration::prelude::*;

pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(Initial)]
    }
}

pub struct Initial;

impl MigrationName for Initial {
    fn name(&self) -> &str {
        "m20261005_000001_initial"
    }
}

#[async_trait::async_trait]
impl MigrationTrait for Initial {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        for mut table in tables(backend) {
            if backend == DbBackend::MySql {
                // NO PAD 且区分大小写，使 MySQL 的比较行为与 SQLite/PostgreSQL 一致。
                table.extra("ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_bin");
            }
            manager.create_table(table).await?;
        }
        for index in indexes() {
            manager.create_index(index).await?;
        }
        if backend == DbBackend::Postgres {
            apply_postgres_comments(manager).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for table in [
            "tags",
            "tag_groups",
            "notifications",
            "published_messages",
            "publication_attempts",
            "publication_runs",
            "post_audit_log",
            "review_messages",
            "submitters",
            "post_messages",
            "posts",
            "slot_pick_fallbacks",
            "slot_picks",
            "slots",
            "categories",
            "admins",
        ] {
            manager
                .drop_table(Table::drop().table(Alias::new(table)).to_owned())
                .await?;
        }
        Ok(())
    }
}

fn foreign_key(table: &mut TableCreateStatement, from: &str, to_table: &str, to: &str) {
    table.foreign_key(
        ForeignKey::create()
            .from_col(Alias::new(from))
            .to(Alias::new(to_table), Alias::new(to))
            .on_delete(ForeignKeyAction::Restrict),
    );
}

fn enum_check(table: &mut TableCreateStatement, column: &str, maximum: i32) {
    table.check(Expr::col(Alias::new(column)).is_in(0..=maximum));
}

fn active_check(table: &mut TableCreateStatement, active: &str, value: &str) {
    table.check(
        Expr::col(Alias::new("archived_at"))
            .is_null()
            .and(Expr::col(Alias::new(active)).is_not_null())
            .and(Expr::col(Alias::new(active)).eq(Expr::col(Alias::new(value))))
            .or(Expr::col(Alias::new("archived_at"))
                .is_not_null()
                .and(Expr::col(Alias::new(active)).is_null())),
    );
}

pub(super) fn tables(backend: DbBackend) -> Vec<TableCreateStatement> {
    let schema = PortableSchema {
        schema: Schema::new(backend),
        backend,
    };
    let admins = schema.create_table_from_entity(admins::Entity);
    let mut categories = schema.create_table_from_entity(categories::Entity);
    foreign_key(&mut categories, "admin_user_id", "admins", "user_id");
    active_check(&mut categories, "active_label", "label");
    let mut slots = schema.create_table_from_entity(slots::Entity);
    foreign_key(&mut slots, "admin_user_id", "admins", "user_id");
    active_check(&mut slots, "active_time", "time");
    let mut picks = schema.create_table_from_entity(slot_picks::Entity);
    foreign_key(&mut picks, "slot_id", "slots", "id");
    foreign_key(&mut picks, "category_id", "categories", "id");
    picks.check(Expr::col(Alias::new("count")).between(1, 65535));
    let mut fallbacks = schema.create_table_from_entity(slot_pick_fallbacks::Entity);
    foreign_key(&mut fallbacks, "pick_id", "slot_picks", "id");
    foreign_key(&mut fallbacks, "category_id", "categories", "id");
    let mut posts = schema.create_table_from_entity(posts::Entity);
    foreign_key(&mut posts, "owner_admin_id", "admins", "user_id");
    foreign_key(&mut posts, "reviewed_by_admin_id", "admins", "user_id");
    foreign_key(&mut posts, "category_id", "categories", "id");
    enum_check(&mut posts, "status", 7);
    posts.check(
        Expr::col(Alias::new("status"))
            .is_in([0, 1, 2, 7])
            .or(Expr::col(Alias::new("owner_admin_id"))
                .is_not_null()
                .and(Expr::col(Alias::new("queued_at")).is_not_null())
                .and(
                    Expr::col(Alias::new("category_id"))
                        .is_not_null()
                        .or(Expr::col(Alias::new("status")).is_in([4, 5, 6])),
                )),
    );
    let mut messages = schema.create_table_from_entity(post_messages::Entity);
    foreign_key(&mut messages, "post_id", "posts", "id");
    enum_check(&mut messages, "media_kind", 3);
    messages.check(Expr::col(Alias::new("position")).gte(0));
    messages.check(
        Expr::col(Alias::new("media_kind"))
            .is_null()
            .eq(Expr::col(Alias::new("media_file_id")).is_null()),
    );
    messages.check(
        Expr::col(Alias::new("media_kind"))
            .is_null()
            .eq(Expr::col(Alias::new("media_file_unique_id")).is_null()),
    );
    messages.check(
        Expr::col(Alias::new("text"))
            .is_not_null()
            .or(Expr::col(Alias::new("media_kind")).is_not_null()),
    );
    let mut submitters = schema.create_table_from_entity(submitters::Entity);
    foreign_key(&mut submitters, "blocked_by_admin_id", "admins", "user_id");
    enum_check(&mut submitters, "status", 1);
    let mut review = schema.create_table_from_entity(review_messages::Entity);
    foreign_key(&mut review, "post_id", "posts", "id");
    enum_check(&mut review, "kind", 1);
    review.check(Expr::col(Alias::new("position")).gte(0));
    let mut audit = schema.create_table_from_entity(post_audit_log::Entity);
    foreign_key(&mut audit, "post_id", "posts", "id");
    enum_check(&mut audit, "kind", 6);
    let mut runs = schema.create_table_from_entity(publication_runs::Entity);
    foreign_key(&mut runs, "slot_id", "slots", "id");
    enum_check(&mut runs, "status", 4);
    let mut attempts = schema.create_table_from_entity(publication_attempts::Entity);
    foreign_key(&mut attempts, "run_id", "publication_runs", "id");
    foreign_key(&mut attempts, "post_id", "posts", "id");
    foreign_key(&mut attempts, "configured_category_id", "categories", "id");
    enum_check(&mut attempts, "status", 6);
    let mut published = schema.create_table_from_entity(published_messages::Entity);
    foreign_key(&mut published, "attempt_id", "publication_attempts", "id");
    published.check(Expr::col(Alias::new("position")).gte(0));
    let mut notifications = schema.create_table_from_entity(notifications::Entity);
    enum_check(&mut notifications, "kind", 6);
    enum_check(&mut notifications, "status", 4);
    let mut groups = schema.create_table_from_entity(tag_groups::Entity);
    active_check(&mut groups, "active_label", "label");
    let mut tags = schema.create_table_from_entity(tags::Entity);
    foreign_key(&mut tags, "group_id", "tag_groups", "id");
    tags.check(
        Expr::col(Alias::new("archived_at"))
            .is_null()
            .and(Expr::col(Alias::new("active_name")).is_not_null())
            .or(Expr::col(Alias::new("archived_at"))
                .is_not_null()
                .and(Expr::col(Alias::new("active_name")).is_null())),
    );
    vec![
        admins,
        categories,
        slots,
        picks,
        fallbacks,
        posts,
        messages,
        submitters,
        review,
        audit,
        runs,
        attempts,
        published,
        notifications,
        groups,
        tags,
    ]
}

struct PortableSchema {
    schema: Schema,
    backend: DbBackend,
}

impl PortableSchema {
    fn create_table_from_entity<E: EntityTrait>(&self, entity: E) -> TableCreateStatement {
        let table_name = entity.to_string();
        let mut table = Table::create();
        table.table(entity);
        if let Some(comment) = table_comment(&table_name) {
            table.comment(comment);
        }
        for column in E::Column::iter() {
            let column_name = column.to_string();
            let mut definition = self.schema.get_column_def::<E>(column);
            if self.backend == DbBackend::MySql
                && matches!(column.def().get_column_type(), ColumnType::Text)
            {
                // TEXT 的 64 KiB 限制不足以容纳任意序列化内容。
                definition.custom(Alias::new("LONGTEXT"));
            }
            if let Some(comment) = column_comment(&table_name, &column_name) {
                definition.comment(comment);
            }
            table.col(definition);
        }
        if E::PrimaryKey::iter().count() > 1 {
            let mut key = Index::create();
            for column in E::PrimaryKey::iter() {
                key.col(column.into_column());
            }
            table.primary_key(&mut key);
        }
        table.to_owned()
    }
}

const TABLE_COMMENTS: &[(&str, &str)] = &[
    (
        "admins",
        "管理员及其个人发布时间表设置；配置中移除的管理员保留为停用记录。",
    ),
    (
        "categories",
        "管理员的投稿分类；历史投稿仍会引用分类，因此只归档不删除。",
    ),
    ("slots", "管理员每日执行的发布时间段。"),
    ("slot_picks", "发布时间段从某个分类选取多少篇投稿的配额。"),
    ("slot_pick_fallbacks", "某项配额不足时依次使用的补位分类。"),
    (
        "posts",
        "投稿主体；每篇投稿由一条或多条 post_messages 组成。",
    ),
    (
        "post_messages",
        "投稿中的 Telegram 消息；相册每个媒体各占一行。",
    ),
    (
        "publication_runs",
        "某个发布时间段在某个本地日期的一次执行。",
    ),
    ("publication_attempts", "某次执行发布某篇投稿的一次尝试。"),
    (
        "published_messages",
        "成功发布尝试在频道中产生的 Telegram 消息。",
    ),
    ("notifications", "发送给管理员或投稿人的通知及其投递状态。"),
    ("submitters", "普通投稿人的状态和拉黑记录；不保存用户名。"),
    (
        "review_messages",
        "Bot 在审核群中为投稿发送的内容消息和控制消息。",
    ),
    ("post_audit_log", "普通用户投稿审核过程的审计日志。"),
    ("tag_groups", "AI 识图标签词表的分组。"),
    ("tags", "AI 识图可使用的标签词表。"),
];

const COLUMN_COMMENTS: &[(&str, &str, &str)] = &[
    (
        "admins",
        "user_id",
        "Telegram 用户 ID，也是管理员的稳定身份。",
    ),
    ("admins", "active", "是否仍存在于配置文件的管理员名单中。"),
    (
        "admins",
        "timezone",
        "管理员的 IANA 时区；为空时使用全局默认值。",
    ),
    (
        "admins",
        "misfire_grace",
        "错过发布时间段后仍可补发的宽限期，例如 2h；为空时使用默认值。",
    ),
    (
        "admins",
        "send_interval",
        "同一时段内两篇投稿之间的发送间隔，例如 3s；为空时使用默认值。",
    ),
    (
        "admins",
        "reminder_time",
        "管理员时区下的每日库存提醒时间 HH:MM；为空表示不提醒。",
    ),
    ("admins", "created_at", "记录创建时间，Unix 微秒。"),
    ("categories", "id", "数据库生成的分类 ID。"),
    ("categories", "admin_user_id", "拥有该分类的管理员 ID。"),
    (
        "categories",
        "label",
        "展示给用户的分类名称；同一管理员的未归档分类中唯一。",
    ),
    (
        "categories",
        "position",
        "按钮和库存列表中的显示顺序，数值越小越靠前。",
    ),
    (
        "categories",
        "archived_at",
        "归档时间；归档分类仍供历史投稿引用。",
    ),
    (
        "categories",
        "active_label",
        "未归档时复制 label，归档后为 NULL；用于归档后复用名称的唯一约束。",
    ),
    ("slots", "id", "数据库生成的时段 ID。"),
    ("slots", "admin_user_id", "拥有该时段的管理员 ID。"),
    (
        "slots",
        "time",
        "管理员时区下的本地时间，格式为补零的 HH:MM。",
    ),
    (
        "slots",
        "effective_from",
        "开始调度的时间，Unix 微秒；避免新建或修改时段后立即补发今天已过去的时间。",
    ),
    ("slots", "archived_at", "归档时间；归档时段不再调度。"),
    (
        "slots",
        "active_time",
        "未归档时复制 time，归档后为 NULL；用于归档后复用时刻的唯一约束。",
    ),
    ("slot_picks", "id", "数据库生成的配额 ID。"),
    ("slot_picks", "slot_id", "所属发布时间段 ID。"),
    ("slot_picks", "category_id", "提供投稿的分类 ID。"),
    (
        "slot_picks",
        "count",
        "从该分类选取的投稿数，范围为 1 到 65535。",
    ),
    ("slot_picks", "position", "配额在时段中的显示和处理顺序。"),
    ("slot_pick_fallbacks", "pick_id", "所属配额 ID。"),
    (
        "slot_pick_fallbacks",
        "category_id",
        "作为补位来源的分类 ID。",
    ),
    (
        "slot_pick_fallbacks",
        "position",
        "补位顺序，数值越小越先尝试。",
    ),
    ("posts", "id", "数据库生成的投稿 ID。"),
    ("posts", "submitter_user_id", "投稿人的 Telegram 用户 ID。"),
    (
        "posts",
        "owner_admin_id",
        "拥有该投稿队列的管理员；普通投稿审核通过前为空。",
    ),
    (
        "posts",
        "category_id",
        "队列分类；草稿或待审核投稿尚未选择分类时为空。",
    ),
    (
        "posts",
        "status",
        "投稿状态：0 草稿、1 待审核、2 已拒绝、3 排队、4 已预留、5 已发布、6 失败、7 已取消。",
    ),
    (
        "posts",
        "reply_to_message_id",
        "投稿回复的频道消息 ID；不回复时为空。",
    ),
    ("posts", "created_at", "投稿创建时间，Unix 微秒。"),
    (
        "posts",
        "queued_at",
        "进入管理员队列的时间，Unix 微秒；同一队列按倒序选择。",
    ),
    ("posts", "published_at", "发布完成时间，Unix 微秒。"),
    (
        "posts",
        "submitted_at",
        "普通用户提交审核的时间，Unix 微秒。",
    ),
    ("posts", "reviewed_by_admin_id", "处理该投稿的管理员 ID。"),
    ("posts", "reviewed_at", "审核处理时间，Unix 微秒。"),
    ("posts", "review_note", "拒绝或其他审核操作的说明。"),
    ("posts", "control_chat_id", "Bot 控制消息所在的聊天 ID。"),
    ("posts", "control_message_id", "Bot 最近一条控制消息的 ID。"),
    ("post_messages", "id", "数据库生成的消息行 ID。"),
    ("post_messages", "post_id", "所属投稿 ID。"),
    (
        "post_messages",
        "position",
        "投稿内从 0 开始的顺序，也决定相册发送顺序。",
    ),
    ("post_messages", "source_chat_id", "原始 Telegram 聊天 ID。"),
    (
        "post_messages",
        "source_message_id",
        "原始 Telegram 消息 ID，与 source_chat_id 配对识别重复投递。",
    ),
    ("post_messages", "text", "正文或媒体说明文字。"),
    (
        "post_messages",
        "entities_json",
        "Telegram MessageEntity 数组 JSON，描述正文格式化范围。",
    ),
    (
        "post_messages",
        "link_preview_json",
        "用户修改过的链接预览设置 JSON；未修改时为空。",
    ),
    (
        "post_messages",
        "show_caption_above_media",
        "是否要求 Telegram 将媒体说明显示在媒体上方。",
    ),
    (
        "post_messages",
        "media_kind",
        "媒体类型：0 图片、1 视频、2 动画、3 文件；纯文字消息为空。",
    ),
    (
        "post_messages",
        "media_file_id",
        "重新发送媒体所需的 Telegram file ID。",
    ),
    (
        "post_messages",
        "media_file_unique_id",
        "同一文件的稳定标识，目前仅保存作信息用途。",
    ),
    ("post_messages", "media_has_spoiler", "媒体是否标记为剧透。"),
    ("publication_runs", "id", "数据库生成的执行 ID。"),
    ("publication_runs", "slot_id", "被执行的发布时间段 ID。"),
    (
        "publication_runs",
        "local_date",
        "时段所属时区的日期，格式为 YYYY-MM-DD。",
    ),
    (
        "publication_runs",
        "scheduled_at",
        "计划执行时间，Unix 微秒。",
    ),
    (
        "publication_runs",
        "started_at",
        "实际开始时间，Unix 微秒；跳过的执行为空。",
    ),
    (
        "publication_runs",
        "completed_at",
        "完成或中止时间，Unix 微秒。",
    ),
    (
        "publication_runs",
        "status",
        "执行状态：0 运行中、1 完成、2 有问题完成、3 中止、4 跳过。",
    ),
    ("publication_attempts", "id", "数据库生成的发布尝试 ID。"),
    (
        "publication_attempts",
        "run_id",
        "所属执行 ID；立即发送的投稿为空。",
    ),
    ("publication_attempts", "post_id", "本次尝试发布的投稿 ID。"),
    (
        "publication_attempts",
        "configured_category_id",
        "时段为本次选择配置的分类 ID，可能是补位分类。",
    ),
    (
        "publication_attempts",
        "status",
        "尝试状态：0 待发送、1 发送中、2 成功、3 回队、4 失败、5 未知、6 未尝试。",
    ),
    (
        "publication_attempts",
        "started_at",
        "Telegram 请求开始时间，Unix 微秒。",
    ),
    (
        "publication_attempts",
        "completed_at",
        "尝试完成时间，Unix 微秒。",
    ),
    (
        "publication_attempts",
        "error",
        "Telegram 错误或结果不确定的说明。",
    ),
    (
        "publication_attempts",
        "resolved_by_user_id",
        "人工确认未知结果的管理员 ID。",
    ),
    (
        "publication_attempts",
        "resolved_at",
        "人工确认未知结果的时间，Unix 微秒。",
    ),
    ("published_messages", "id", "数据库生成的已发布消息行 ID。"),
    (
        "published_messages",
        "attempt_id",
        "产生该消息的成功发布尝试 ID。",
    ),
    ("published_messages", "chat_id", "目标 Telegram 聊天 ID。"),
    (
        "published_messages",
        "message_id",
        "目标 Telegram 消息 ID。",
    ),
    (
        "published_messages",
        "position",
        "本次发布内从 0 开始的消息顺序，相册按此排序。",
    ),
    ("notifications", "id", "数据库生成的通知 ID。"),
    (
        "notifications",
        "kind",
        "通知类型：0 发布未知、1 发布失败、2 执行中止、3 库存提醒、4 投稿通过、5 投稿拒绝、6 投稿撤下。",
    ),
    (
        "notifications",
        "subject_id",
        "通知对象 ID，含义由 kind 决定，可为尝试、执行、提醒日期或投稿。",
    ),
    (
        "notifications",
        "recipient_user_id",
        "接收通知的 Telegram 用户 ID。",
    ),
    (
        "notifications",
        "status",
        "投递状态：0 待发送、1 发送中、2 已发送、3 失败、4 取消。",
    ),
    (
        "notifications",
        "telegram_message_id",
        "通知发送后对应的 Telegram 消息 ID。",
    ),
    ("notifications", "created_at", "通知入队时间，Unix 微秒。"),
    ("notifications", "sent_at", "通知发送时间，Unix 微秒。"),
    ("notifications", "error", "最近一次投递错误。"),
    ("submitters", "user_id", "普通投稿人的 Telegram 用户 ID。"),
    ("submitters", "status", "投稿人状态：0 正常、1 已拉黑。"),
    ("submitters", "blocked_reason", "拉黑原因。"),
    ("submitters", "blocked_at", "拉黑时间，Unix 微秒。"),
    (
        "submitters",
        "blocked_by_admin_id",
        "执行拉黑操作的管理员 ID。",
    ),
    (
        "submitters",
        "created_at",
        "投稿人记录创建时间，Unix 微秒。",
    ),
    ("review_messages", "id", "数据库生成的审核消息行 ID。"),
    ("review_messages", "post_id", "正在审核的投稿 ID。"),
    ("review_messages", "chat_id", "审核群聊天 ID。"),
    ("review_messages", "message_id", "审核群消息 ID。"),
    (
        "review_messages",
        "position",
        "投稿内容从 0 开始的顺序；控制消息约定使用 0。",
    ),
    (
        "review_messages",
        "kind",
        "消息类型：0 投稿内容、1 带按钮的 Bot 控制消息。",
    ),
    ("post_audit_log", "id", "数据库生成的审计日志 ID。"),
    ("post_audit_log", "post_id", "发生审核变化的投稿 ID。"),
    (
        "post_audit_log",
        "actor_user_id",
        "执行操作的 Telegram 用户 ID。",
    ),
    (
        "post_audit_log",
        "kind",
        "操作类型：0 提交、1 撤回、2 通过、3 拒绝、4 替换文字、5 清空文字、6 撤下。",
    ),
    (
        "post_audit_log",
        "detail_json",
        "操作详情 JSON，例如被替换的原文或撤下理由。",
    ),
    ("post_audit_log", "created_at", "操作时间，Unix 微秒。"),
    ("tag_groups", "id", "数据库生成的标签分组 ID。"),
    (
        "tag_groups",
        "label",
        "分组名称，用于整理标签并作为 AI 提示词上下文。",
    ),
    ("tag_groups", "position", "分组显示顺序，数值越小越靠前。"),
    (
        "tag_groups",
        "archived_at",
        "归档时间；有历史标签引用的分组只归档不删除。",
    ),
    (
        "tag_groups",
        "active_label",
        "未归档时复制 label，归档后为 NULL；用于复用已归档名称。",
    ),
    ("tags", "id", "数据库生成的标签 ID。"),
    ("tags", "group_id", "所属标签分组 ID。"),
    (
        "tags",
        "name",
        "不含 # 的标签名；未归档标签中不区分 ASCII 大小写。",
    ),
    (
        "tags",
        "archived_at",
        "归档时间；归档标签不再提供给 AI 模型选择。",
    ),
    (
        "tags",
        "active_name",
        "未归档时为 name 的 ASCII 小写形式，归档后为 NULL；用于全局唯一约束。",
    ),
];

fn table_comment(table: &str) -> Option<&'static str> {
    TABLE_COMMENTS
        .iter()
        .find_map(|(name, comment)| (*name == table).then_some(*comment))
}

fn column_comment(table: &str, column: &str) -> Option<&'static str> {
    COLUMN_COMMENTS
        .iter()
        .find_map(|(table_name, column_name, comment)| {
            (*table_name == table && *column_name == column).then_some(*comment)
        })
}

fn sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

async fn apply_postgres_comments(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let connection = manager.get_connection();
    for &(table, comment) in TABLE_COMMENTS {
        connection
            .execute_unprepared(&format!(
                "COMMENT ON TABLE {} IS {}",
                sql_identifier(table),
                sql_string(comment)
            ))
            .await?;
    }
    for &(table, column, comment) in COLUMN_COMMENTS {
        connection
            .execute_unprepared(&format!(
                "COMMENT ON COLUMN {}.{} IS {}",
                sql_identifier(table),
                sql_identifier(column),
                sql_string(comment)
            ))
            .await?;
    }
    Ok(())
}

fn index(name: &str, table: &str, columns: &[&str], unique: bool) -> IndexCreateStatement {
    let mut index = Index::create();
    index.name(name).table(Alias::new(table));
    for column in columns {
        index.col(Alias::new(*column));
    }
    if unique {
        index.unique();
    }
    index.to_owned()
}

fn indexes() -> Vec<IndexCreateStatement> {
    vec![
        index(
            "categories_label_idx",
            "categories",
            &["admin_user_id", "active_label"],
            true,
        ),
        index(
            "slots_time_idx",
            "slots",
            &["admin_user_id", "active_time"],
            true,
        ),
        index(
            "slot_picks_category_idx",
            "slot_picks",
            &["slot_id", "category_id"],
            true,
        ),
        index(
            "fallbacks_position_idx",
            "slot_pick_fallbacks",
            &["pick_id", "position"],
            true,
        ),
        index(
            "posts_queue_idx",
            "posts",
            &["owner_admin_id", "status", "queued_at", "id"],
            false,
        ),
        index(
            "posts_review_idx",
            "posts",
            &["status", "submitted_at", "id"],
            false,
        ),
        index(
            "messages_source_idx",
            "post_messages",
            &["source_chat_id", "source_message_id"],
            true,
        ),
        index(
            "messages_position_idx",
            "post_messages",
            &["post_id", "position"],
            true,
        ),
        index(
            "review_source_idx",
            "review_messages",
            &["chat_id", "message_id"],
            true,
        ),
        index(
            "review_post_idx",
            "review_messages",
            &["post_id", "kind", "position"],
            false,
        ),
        index(
            "audit_post_idx",
            "post_audit_log",
            &["post_id", "id"],
            false,
        ),
        index(
            "runs_slot_date_idx",
            "publication_runs",
            &["slot_id", "local_date"],
            true,
        ),
        index(
            "attempts_run_post_idx",
            "publication_attempts",
            &["run_id", "post_id"],
            true,
        ),
        index(
            "attempts_status_idx",
            "publication_attempts",
            &["status", "completed_at"],
            false,
        ),
        index(
            "published_source_idx",
            "published_messages",
            &["chat_id", "message_id"],
            true,
        ),
        index(
            "published_position_idx",
            "published_messages",
            &["attempt_id", "position"],
            true,
        ),
        index(
            "notifications_subject_idx",
            "notifications",
            &["kind", "subject_id", "recipient_user_id"],
            true,
        ),
        index(
            "notifications_status_idx",
            "notifications",
            &["status", "created_at"],
            false,
        ),
        index(
            "tag_groups_label_idx",
            "tag_groups",
            &["active_label"],
            true,
        ),
        index("tags_name_idx", "tags", &["active_name"], true),
        index("tags_group_idx", "tags", &["group_id"], false),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_schema_builds_for_every_backend() {
        for backend in [DbBackend::Sqlite, DbBackend::MySql, DbBackend::Postgres] {
            let statements = tables(backend);
            assert_eq!(statements.len(), 16);
            for table in &statements {
                let sql = backend.build(table).sql;
                assert!(sql.starts_with("CREATE TABLE"));
                assert!(!sql.contains("CREATE TYPE"));
                assert!(!sql.contains("store_write_lock"));
            }
            for statement in indexes() {
                assert!(backend.build(&statement).sql.contains("INDEX"));
            }
            let sql = backend.build(&statements[5]).sql;
            let (status, integer) = if backend == DbBackend::MySql {
                ("`status`", "int")
            } else {
                ("\"status\"", "integer")
            };
            assert!(
                sql.contains(&format!("{status} {integer} NOT NULL")),
                "{sql}"
            );
        }
    }

    #[test]
    fn mysql_payloads_use_longtext_and_nullable_columns_stay_nullable() {
        let sql = DbBackend::MySql.build(&tables(DbBackend::MySql)[6]).sql;
        assert!(sql.contains("`entities_json` LONGTEXT NOT NULL"), "{sql}");
        assert!(sql.contains("`text` LONGTEXT"), "{sql}");
        assert!(!sql.contains("`text` LONGTEXT NOT NULL"), "{sql}");
        assert!(sql.contains("`media_file_id` LONGTEXT"), "{sql}");
        assert!(!sql.contains("`media_file_id` LONGTEXT NOT NULL"), "{sql}");
        for backend in [DbBackend::Sqlite, DbBackend::Postgres] {
            assert!(!backend.build(&tables(backend)[6]).sql.contains("LONGTEXT"));
        }
    }
}
