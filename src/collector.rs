use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use teloxide::types::ChatId;
use tokio::sync::mpsc;

use crate::model::IncomingMessage;

type GroupKey = (ChatId, String);

#[derive(Default)]
struct Group {
    generation: u64,
    messages: Vec<IncomingMessage>,
}

/// 把收到的消息转成批次，每个批次对应一个帖子。
///
/// 普通消息自成一个批次。Telegram 媒体组的消息会被暂存，直到在 `wait` 时间内
/// 没有新成员到达，然后按消息 ID 排序后一起输出。仍在
/// 收集中的媒体组会在进程停止时丢失。
pub struct Collector {
    wait: Duration,
    groups: Arc<Mutex<HashMap<GroupKey, Group>>>,
    batches: mpsc::UnboundedSender<Vec<IncomingMessage>>,
}

impl Collector {
    pub fn new(wait: Duration) -> (Self, mpsc::UnboundedReceiver<Vec<IncomingMessage>>) {
        let (batches, receiver) = mpsc::unbounded_channel();
        let collector = Self {
            wait,
            groups: Arc::default(),
            batches,
        };
        (collector, receiver)
    }

    pub fn push(&self, message: IncomingMessage) {
        let Some(group_id) = message.media_group_id.clone() else {
            let _ = self.batches.send(vec![message]);
            return;
        };
        let key = (message.source_chat_id, group_id);
        let generation = {
            let mut groups = self
                .groups
                .lock()
                .expect("collector lock is never poisoned");
            let group = groups.entry(key.clone()).or_default();
            group.generation += 1;
            group.messages.push(message);
            group.generation
        };

        let groups = Arc::clone(&self.groups);
        let batches = self.batches.clone();
        let wait = self.wait;
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let finished = {
                let mut groups = groups.lock().expect("collector lock is never poisoned");
                let is_latest = groups
                    .get(&key)
                    .is_some_and(|group| group.generation == generation);
                if is_latest { groups.remove(&key) } else { None }
            };
            if let Some(mut group) = finished {
                group
                    .messages
                    .sort_by_key(|message| message.source_message_id.0);
                let _ = batches.send(group.messages);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use teloxide::types::{MessageId, UserId};

    use super::*;
    use crate::model::Content;

    fn message(id: i32, group: Option<&str>) -> IncomingMessage {
        IncomingMessage {
            submitter: UserId(1),
            source_chat_id: ChatId(1),
            source_message_id: MessageId(id),
            media_group_id: group.map(ToOwned::to_owned),
            target: None,
            channel_reply: None,
            content: Content {
                text: Some(id.to_string()),
                entities: Vec::new(),
                link_preview_options: None,
                show_caption_above_media: false,
                media: None,
            },
        }
    }

    fn ids(batch: &[IncomingMessage]) -> Vec<i32> {
        batch
            .iter()
            .map(|message| message.source_message_id.0)
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn plain_messages_are_emitted_immediately() {
        let (collector, mut batches) = Collector::new(Duration::from_secs(1));
        collector.push(message(1, None));
        assert_eq!(ids(&batches.recv().await.unwrap()), [1]);
    }

    #[tokio::test(start_paused = true)]
    async fn group_is_emitted_sorted_after_a_quiet_period() {
        let (collector, mut batches) = Collector::new(Duration::from_secs(1));
        collector.push(message(20, Some("album")));
        tokio::time::sleep(Duration::from_millis(600)).await;
        collector.push(message(10, Some("album")));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(batches.try_recv().is_err());
        assert_eq!(ids(&batches.recv().await.unwrap()), [10, 20]);
    }

    #[tokio::test(start_paused = true)]
    async fn groups_are_collected_independently() {
        let (collector, mut batches) = Collector::new(Duration::from_secs(1));
        collector.push(message(1, Some("a")));
        collector.push(message(2, Some("b")));
        let mut all = vec![
            ids(&batches.recv().await.unwrap()),
            ids(&batches.recv().await.unwrap()),
        ];
        all.sort();
        assert_eq!(all, [vec![1], vec![2]]);
    }
}
