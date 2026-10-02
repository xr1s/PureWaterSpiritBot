//! 可移植的持久化模型。领域枚举由 SeaORM 编码为 INTEGER。

/// 已配置的管理员。配置文件是权威来源；从配置中移除的管理员仍保留在这里，
/// 但 `active` 会变为 false。
pub mod admins {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "admins")]
    pub struct Model {
        /// Telegram 用户 ID，也是管理员的稳定身份。
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: i64,
        /// 该管理员是否仍存在于配置文件中。
        pub active: bool,
        /// 可选的 IANA 时区覆盖值；NULL 表示使用发布时间表默认值。
        #[sea_orm(column_type = "Text", nullable)]
        pub timezone: Option<String>,
        /// 错过发布时间段后仍可补发的宽限期，例如 `2h`；NULL 表示使用默认值。
        #[sea_orm(column_type = "Text", nullable)]
        pub misfire_grace: Option<String>,
        /// 同一时段内两篇投稿之间的间隔，例如 `3s`；NULL 表示使用默认值。
        #[sea_orm(column_type = "Text", nullable)]
        pub send_interval: Option<String>,
        /// 管理员时区下的每日库存提醒时间 `HH:MM`；NULL 表示关闭提醒。
        pub reminder_time: Option<String>,
        /// 创建时间，Unix 微秒。
        pub created_at: i64,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 管理员拥有的投稿队列分类。已发布投稿仍会引用分类，因此分类只归档、不删除。
pub mod categories {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "categories")]
    pub struct Model {
        /// 数据库生成的分类 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 拥有该分类的管理员 ID。
        pub admin_user_id: i64,
        /// 展示给用户的分类名称；同一管理员的未归档分类中唯一。
        #[sea_orm(column_type = "String(StringLen::N(512))")]
        pub label: String,
        /// 按钮和库存摘要中的显示顺序；数值越小越靠前。
        pub position: i32,
        /// 归档时间；归档分类仍供历史投稿引用。
        pub archived_at: Option<i64>,
        /// 未归档时复制 `label`，归档后为 NULL；在事务中维护，用于支持归档后复用名称的
        /// 可移植唯一索引。
        #[sea_orm(column_type = "String(StringLen::N(512))", nullable)]
        pub active_label: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 每日发布时间段。每个时段按配额选择投稿，并在管理员配置的本地时间发布。
pub mod slots {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "slots")]
    pub struct Model {
        /// 数据库生成的时段 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 拥有该时段的管理员 ID。
        pub admin_user_id: i64,
        /// 管理员时区下的本地时间，格式为补零的 `HH:MM`。
        pub time: String,
        /// 最早触发时间，Unix 微秒；避免新建或修改时段后立即补发今天已经过去的时间。
        pub effective_from: i64,
        /// 归档时间；归档时段不再调度。
        pub archived_at: Option<i64>,
        /// 未归档时复制 `time`，归档后为 NULL；在事务中维护，用于支持归档后复用时刻的
        /// 可移植唯一约束。
        pub active_time: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 发布时间段的配额项。
pub mod slot_picks {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "slot_picks")]
    pub struct Model {
        /// 数据库生成的配额 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 所属发布时间段 ID。
        pub slot_id: i64,
        /// 提供投稿的分类 ID。
        pub category_id: i64,
        /// 从该分类选择的投稿数量，范围为 1 到 65535。
        pub count: i32,
        /// 配额项的显示和处理顺序。
        pub position: i32,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 某项配额不足时使用的补位分类。
pub mod slot_pick_fallbacks {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "slot_pick_fallbacks")]
    pub struct Model {
        /// 拥有该补位项的配额 ID。
        #[sea_orm(primary_key, auto_increment = false)]
        pub pick_id: i64,
        /// 作为补位来源的分类 ID。
        #[sea_orm(primary_key, auto_increment = false)]
        pub category_id: i64,
        /// 补位顺序；数值越小越先尝试。
        pub position: i32,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 投稿主体。一篇投稿由一条或多条 `post_messages` 记录组成。
pub mod posts {
    use crate::model::PostStatus;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "posts")]
    pub struct Model {
        /// 数据库生成的投稿 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 投稿人的 Telegram 用户 ID。
        pub submitter_user_id: i64,
        /// 拥有该投稿队列的管理员；普通投稿审核通过前为 NULL。
        pub owner_admin_id: Option<i64>,
        /// 队列分类；草稿尚未入队或普通投稿等待审核时为 NULL。
        pub category_id: Option<i64>,
        /// 生命周期状态：0 草稿、1 待审核、2 已拒绝、3 排队、4 已预留、5 已发布、
        /// 6 失败、7 已取消。
        pub status: PostStatus,
        /// 投稿回复的频道消息 ID；不回复时为空。
        pub reply_to_message_id: Option<i32>,
        /// 创建时间，Unix 微秒。
        pub created_at: i64,
        /// 进入管理员队列的时间，Unix 微秒。队列按此字段倒序选择，因此最新投稿优先。
        pub queued_at: Option<i64>,
        /// 发布完成时间，Unix 微秒。
        pub published_at: Option<i64>,
        /// 普通用户提交审核的时间，Unix 微秒。
        pub submitted_at: Option<i64>,
        /// 处理该投稿的管理员 ID。
        pub reviewed_by_admin_id: Option<i64>,
        /// 审核处理时间，Unix 微秒。
        pub reviewed_at: Option<i64>,
        /// 拒绝或其他审核操作的说明。
        #[sea_orm(column_type = "Text", nullable)]
        pub review_note: Option<String>,
        /// Bot 控制消息所在的聊天 ID。
        pub control_chat_id: Option<i64>,
        /// Bot 最近一条控制消息的 ID。
        pub control_message_id: Option<i32>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 组成投稿的 Telegram 消息。媒体组中的每个媒体各占一条记录。
pub mod post_messages {
    use crate::model::MediaKind;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "post_messages")]
    pub struct Model {
        /// 数据库生成的消息行 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 所属投稿 ID。
        pub post_id: i64,
        /// 投稿内从 0 开始的顺序；对于相册也决定发送顺序。
        pub position: i32,
        /// 原始 Telegram 聊天 ID。
        pub source_chat_id: i64,
        /// 原始 Telegram 消息 ID，与 `source_chat_id` 配对后用于识别重复投递。
        pub source_message_id: i32,
        /// 正文或媒体说明文字。
        #[sea_orm(column_type = "Text", nullable)]
        pub text: Option<String>,
        /// Telegram MessageEntity 数组 JSON，描述 `text` 中各段的格式化范围。
        #[sea_orm(column_type = "Text")]
        pub entities_json: String,
        /// 用户修改过的链接预览设置 JSON；未修改时为空。
        #[sea_orm(column_type = "Text", nullable)]
        pub link_preview_json: Option<String>,
        /// 是否要求 Telegram 将媒体说明显示在媒体上方。
        pub show_caption_above_media: bool,
        /// 媒体类型：0 图片、1 视频、2 动画、3 文件；纯文字消息为 NULL。
        pub media_kind: Option<MediaKind>,
        /// 重新发送媒体所需的 Telegram file ID；不保证永久有效。
        #[sea_orm(column_type = "Text", nullable)]
        pub media_file_id: Option<String>,
        /// Telegram 为文件提供的稳定标识，目前仅保存作信息用途。
        #[sea_orm(column_type = "Text", nullable)]
        pub media_file_unique_id: Option<String>,
        /// 媒体是否标记为剧透。
        pub media_has_spoiler: bool,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 某个发布时间段在某个本地日期的一次执行。
pub mod publication_runs {
    use crate::model::RunStatus;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "publication_runs")]
    pub struct Model {
        /// 数据库生成的执行 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 被执行的发布时间段 ID。
        pub slot_id: i64,
        /// 时段所属时区的日期，格式为 `YYYY-MM-DD`。
        pub local_date: String,
        /// 计划执行时间，Unix 微秒。
        pub scheduled_at: i64,
        /// 实际开始时间，Unix 微秒；跳过的执行为 NULL。
        pub started_at: Option<i64>,
        /// 完成时间，Unix 微秒。
        pub completed_at: Option<i64>,
        /// 执行状态：0 运行中、1 完成、2 有问题完成、3 中止、4 跳过。
        pub status: RunStatus,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 某次执行发布某篇投稿的一次尝试。一篇投稿可能有多次尝试。
pub mod publication_attempts {
    use crate::model::AttemptStatus;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "publication_attempts")]
    pub struct Model {
        /// 数据库生成的发布尝试 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 所属执行 ID；立即手动发送的投稿为 NULL。
        pub run_id: Option<i64>,
        /// 本次尝试发布的投稿 ID。
        pub post_id: i64,
        /// 时段为本次选择配置的分类 ID，可能是补位分类。
        pub configured_category_id: Option<i64>,
        /// 尝试状态：0 待发送、1 发送中、2 成功、3 回队、4 失败、5 未知、6 未尝试。
        pub status: AttemptStatus,
        /// Telegram 请求开始时间，Unix 微秒。
        pub started_at: Option<i64>,
        /// 尝试完成时间，Unix 微秒。
        pub completed_at: Option<i64>,
        /// Telegram 操作返回的错误或结果不确定说明。
        #[sea_orm(column_type = "Text", nullable)]
        pub error: Option<String>,
        /// 人工确认未知结果的管理员 ID。
        pub resolved_by_user_id: Option<i64>,
        /// 人工确认未知结果的时间，Unix 微秒。
        pub resolved_at: Option<i64>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 成功发布尝试在 Telegram 中创建的消息。人工确认已发布的尝试没有记录，
/// 因为 Bot 不知道消息 ID。
pub mod published_messages {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "published_messages")]
    pub struct Model {
        /// 数据库生成的已发布消息行 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 产生该消息的成功发布尝试 ID。
        pub attempt_id: i64,
        /// 目标 Telegram 聊天 ID。
        pub chat_id: i64,
        /// 目标 Telegram 消息 ID。
        pub message_id: i32,
        /// 本次发布内从 0 开始的消息顺序，包括相册顺序。
        pub position: i32,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 发送给管理员或投稿人的待发送及历史通知。
pub mod notifications {
    use crate::model::{NotificationKind, NotificationStatus};
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "notifications")]
    pub struct Model {
        /// 数据库生成的通知 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 通知类型：0 发布未知、1 发布失败、2 执行中止、3 库存提醒、4 投稿通过、
        /// 5 投稿拒绝、6 投稿撤下。
        pub kind: NotificationKind,
        /// 通知对象 ID，含义由 `kind` 决定，可以是尝试、执行、提醒日期或投稿。
        pub subject_id: i64,
        /// 接收通知的 Telegram 用户 ID。
        pub recipient_user_id: i64,
        /// 投递状态：0 待发送、1 发送中、2 已发送、3 失败、4 取消。
        pub status: NotificationStatus,
        /// 通知发送后对应的 Telegram 消息 ID。
        pub telegram_message_id: Option<i32>,
        /// 通知入队时间，Unix 微秒。
        pub created_at: i64,
        /// 通知发送时间，Unix 微秒。
        pub sent_at: Option<i64>,
        /// 最近一次投递错误。
        #[sea_orm(column_type = "Text", nullable)]
        pub error: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 普通投稿人。数据库不保存用户名；需要显示名称时向 Telegram 查询。
pub mod submitters {
    use crate::model::SubmitterStatus;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "submitters")]
    pub struct Model {
        /// Telegram 用户 ID。
        #[sea_orm(primary_key, auto_increment = false)]
        pub user_id: i64,
        /// 投稿人状态：0 正常、1 已拉黑。
        pub status: SubmitterStatus,
        /// 拉黑原因。
        #[sea_orm(column_type = "Text", nullable)]
        pub blocked_reason: Option<String>,
        /// 拉黑时间，Unix 微秒。
        pub blocked_at: Option<i64>,
        /// 执行拉黑操作的管理员 ID。
        pub blocked_by_admin_id: Option<i64>,
        /// 投稿人记录创建时间，Unix 微秒。
        pub created_at: i64,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// Bot 为普通投稿发送到审核群的消息。
pub mod review_messages {
    use crate::model::ReviewMessageKind;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "review_messages")]
    pub struct Model {
        /// 数据库生成的审核消息行 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 正在审核的投稿 ID。
        pub post_id: i64,
        /// 审核群聊天 ID。
        pub chat_id: i64,
        /// 审核群消息 ID。
        pub message_id: i32,
        /// 从 0 开始的投稿内容顺序；控制消息约定使用 0。
        pub position: i32,
        /// 消息类型：0 投稿内容、1 带按钮的 Bot 控制消息。
        pub kind: ReviewMessageKind,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// 普通用户投稿审核操作的审计日志。
pub mod post_audit_log {
    use crate::model::PostAuditKind;
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "post_audit_log")]
    pub struct Model {
        /// 数据库生成的审计日志 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 审核历史发生变化的投稿 ID。
        pub post_id: i64,
        /// 执行操作的 Telegram 用户 ID。
        pub actor_user_id: i64,
        /// 操作类型：0 提交、1 撤回、2 通过、3 拒绝、4 替换文字、5 清空文字、6 撤下。
        pub kind: PostAuditKind,
        /// 操作详情 JSON，例如被替换的原文或撤下理由。
        #[sea_orm(column_type = "Text", nullable)]
        pub detail_json: Option<String>,
        /// 操作时间，Unix 微秒。
        pub created_at: i64,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// AI 辅助标签词表的分组。有历史引用的分组只归档、不删除。
pub mod tag_groups {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "tag_groups")]
    pub struct Model {
        /// 数据库生成的标签分组 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 用于整理标签并提供提示词上下文的自由文本分组名称。
        #[sea_orm(column_type = "String(StringLen::N(512))")]
        pub label: String,
        /// 显示顺序；数值越小越靠前。
        pub position: i32,
        /// 归档时间；归档分组仍供历史标签引用。
        pub archived_at: Option<i64>,
        /// 未归档时复制 `label`，归档后为 NULL；用于支持归档后复用名称。
        #[sea_orm(column_type = "String(StringLen::N(512))", nullable)]
        pub active_label: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}

/// AI 标签词表。名称不含 `#`，比较时不区分大小写，生成 hashtag 时空格会替换为下划线。
pub mod tags {
    use sea_orm::entity::prelude::*;
    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "tags")]
    pub struct Model {
        /// 数据库生成的标签 ID。
        #[sea_orm(primary_key)]
        pub id: i64,
        /// 所属标签分组 ID。
        pub group_id: i64,
        /// 不含 `#` 的标签名；未归档标签中不区分 ASCII 大小写且全局唯一。
        #[sea_orm(column_type = "String(StringLen::N(512))")]
        pub name: String,
        /// 归档时间；归档标签不再提供给 AI 模型选择。
        pub archived_at: Option<i64>,
        /// 未归档时为 `name` 的 ASCII 小写副本，归档后为 NULL；用于可移植的大小写不敏感唯一约束。
        #[sea_orm(column_type = "String(StringLen::N(512))", nullable)]
        pub active_name: Option<String>,
    }
    impl ActiveModelBehavior for ActiveModel {}
}
