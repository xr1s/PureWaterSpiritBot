use anyhow::Result;
use jiff::Timestamp;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set, sea_query::OnConflict};
use teloxide::types::{MessageId, UserId};

use super::{
    Store,
    codec::{decode_user_id, timestamp, user_id},
    entities::notifications,
};
use crate::model::{NotificationKind, NotificationStatus, NotificationSubject};

#[derive(Debug, Clone)]
pub struct PendingNotification {
    pub id: i64,
    pub subject: NotificationSubject,
    pub recipient: UserId,
}

/// 发给普通投稿人的通知种类。
const SUBMITTER_KINDS: [NotificationKind; 2] = [
    NotificationKind::SubmissionApproved,
    NotificationKind::SubmissionRejected,
];
const UNDELIVERED: [NotificationStatus; 2] =
    [NotificationStatus::Pending, NotificationStatus::Sending];

impl Store {
    /// 为每个接收者排队一条通知。已经有相同主题通知的
    /// 接收者会被跳过。
    pub async fn enqueue_notification(
        &self,
        subject: NotificationSubject,
        recipients: &[UserId],
        now: Timestamp,
    ) -> Result<()> {
        let now = timestamp(now)?;
        for &recipient in recipients {
            let row = notifications::ActiveModel {
                kind: Set(subject.kind()),
                subject_id: Set(subject.id()),
                recipient_user_id: Set(user_id(recipient)?),
                status: Set(NotificationStatus::Pending),
                created_at: Set(now),
                telegram_message_id: Set(None),
                sent_at: Set(None),
                error: Set(None),
                ..Default::default()
            };
            notifications::Entity::insert(row)
                .on_conflict(
                    OnConflict::columns([
                        notifications::Column::Kind,
                        notifications::Column::SubjectId,
                        notifications::Column::RecipientUserId,
                    ])
                    .do_nothing_on([notifications::Column::Id])
                    .to_owned(),
                )
                .try_insert()
                .exec_without_returning(&self.db)
                .await?;
        }
        Ok(())
    }

    pub async fn undelivered_notifications(&self) -> Result<Vec<PendingNotification>> {
        let rows = notifications::Entity::find()
            .filter(notifications::Column::Status.is_in(UNDELIVERED))
            .order_by_asc(notifications::Column::CreatedAt)
            .order_by_asc(notifications::Column::Id)
            .all(&self.db)
            .await?;
        rows.into_iter()
            .map(|row| {
                Ok(PendingNotification {
                    id: row.id,
                    subject: NotificationSubject::from_parts(row.kind, row.subject_id)?,
                    recipient: decode_user_id(row.recipient_user_id)?,
                })
            })
            .collect()
    }

    pub async fn mark_notification_sending(&self, id: i64) -> Result<()> {
        notifications::Entity::update_many()
            .set(notifications::ActiveModel {
                status: Set(NotificationStatus::Sending),
                ..Default::default()
            })
            .filter(notifications::Column::Id.eq(id))
            .filter(notifications::Column::Status.eq(NotificationStatus::Pending))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    pub async fn mark_notification_sent(
        &self,
        id: i64,
        message_id: MessageId,
        now: Timestamp,
    ) -> Result<()> {
        notifications::Entity::update_many()
            .set(notifications::ActiveModel {
                status: Set(NotificationStatus::Sent),
                telegram_message_id: Set(Some(message_id.0)),
                sent_at: Set(Some(timestamp(now)?)),
                ..Default::default()
            })
            .filter(notifications::Column::Id.eq(id))
            .filter(notifications::Column::Status.is_in(UNDELIVERED))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// 记录一次失败的投递。可重试的失败会让通知保持待处理状态。
    pub async fn mark_notification_failed(&self, id: i64, retry: bool, error: &str) -> Result<()> {
        let status = if retry {
            NotificationStatus::Pending
        } else {
            NotificationStatus::Failed
        };
        notifications::Entity::update_many()
            .set(notifications::ActiveModel {
                status: Set(status),
                error: Set(Some(error.to_owned())),
                ..Default::default()
            })
            .filter(notifications::Column::Id.eq(id))
            .filter(notifications::Column::Status.is_in(UNDELIVERED))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// 取消发给已不在管理员名单里的人、还没送出的通知。
    /// 审核结果是发给普通投稿人的，收件人本来就不是管理员，所以不在此列。
    pub async fn cancel_notifications_except_for(&self, admins: &[UserId]) -> Result<()> {
        let admins = admins
            .iter()
            .copied()
            .map(user_id)
            .collect::<Result<Vec<_>>>()?;
        notifications::Entity::update_many()
            .set(notifications::ActiveModel {
                status: Set(NotificationStatus::Cancelled),
                ..Default::default()
            })
            .filter(notifications::Column::Status.is_in(UNDELIVERED))
            .filter(notifications::Column::Kind.is_not_in(SUBMITTER_KINDS))
            .filter(notifications::Column::RecipientUserId.is_not_in(admins))
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::model::AttemptId;
    use crate::store::Store;

    #[tokio::test]
    async fn enqueues_once_per_recipient_and_subject() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let subject = NotificationSubject::PublicationUnknown(AttemptId(1));
        let recipients = [UserId(1), UserId(2)];
        store
            .enqueue_notification(subject, &recipients, now)
            .await
            .unwrap();
        store
            .enqueue_notification(subject, &recipients, now)
            .await
            .unwrap();
        assert_eq!(store.undelivered_notifications().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn cancels_notifications_for_removed_users() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let subject = NotificationSubject::PublicationUnknown(AttemptId(1));
        store
            .enqueue_notification(subject, &[UserId(1), UserId(2)], now)
            .await
            .unwrap();
        store
            .cancel_notifications_except_for(&[UserId(2)])
            .await
            .unwrap();
        let pending = store.undelivered_notifications().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].recipient, UserId(2));
    }

    #[tokio::test]
    async fn keeps_notifications_for_submitters_who_are_not_admins() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let submitter = UserId(900);
        store
            .enqueue_notification(
                NotificationSubject::SubmissionApproved(crate::model::PostId(1)),
                &[submitter],
                now,
            )
            .await
            .unwrap();
        store
            .enqueue_notification(
                NotificationSubject::PublicationUnknown(AttemptId(1)),
                &[submitter],
                now,
            )
            .await
            .unwrap();
        store
            .cancel_notifications_except_for(&[UserId(2)])
            .await
            .unwrap();
        let pending = store.undelivered_notifications().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].subject,
            NotificationSubject::SubmissionApproved(crate::model::PostId(1))
        );
    }

    #[tokio::test]
    async fn failed_delivery_is_retried_only_when_transient() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let subject = NotificationSubject::StockReminder(date(2026, 1, 2));
        store
            .enqueue_notification(subject, &[UserId(1)], now)
            .await
            .unwrap();
        let id = store.undelivered_notifications().await.unwrap()[0].id;
        store
            .mark_notification_failed(id, true, "network")
            .await
            .unwrap();
        assert_eq!(store.undelivered_notifications().await.unwrap().len(), 1);
        store
            .mark_notification_failed(id, false, "forbidden")
            .await
            .unwrap();
        assert!(store.undelivered_notifications().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sent_notifications_are_not_overwritten_by_delivery_updates() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let subject = NotificationSubject::PublicationUnknown(AttemptId(1));
        store
            .enqueue_notification(subject, &[UserId(1)], now)
            .await
            .unwrap();
        let id = store.undelivered_notifications().await.unwrap()[0].id;
        store.mark_notification_sending(id).await.unwrap();
        store
            .mark_notification_sent(id, MessageId(7), now)
            .await
            .unwrap();
        store.mark_notification_sending(id).await.unwrap();
        store
            .mark_notification_failed(id, true, "network")
            .await
            .unwrap();
        store
            .mark_notification_sent(id, MessageId(8), now)
            .await
            .unwrap();
        let row = notifications::Entity::find_by_id(id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, NotificationStatus::Sent);
        assert_eq!(row.telegram_message_id, Some(7));
        assert_eq!(row.error, None);
        assert!(store.undelivered_notifications().await.unwrap().is_empty());
    }
}
