use std::{collections::HashSet, sync::Mutex};

use teloxide::{prelude::*, types::ChatFullInfo};

use crate::{
    bot::Inputs,
    collector::Collector,
    config::{Config, VisionConfig},
    fetch::Fetcher,
    model::PostId,
    store::Store,
    vision::Vision,
};

/// 处理器和后台任务共享的全部状态。
pub struct App {
    pub bot: Bot,
    /// 目标频道的完整信息
    pub channel: ChatFullInfo,
    /// 审核群的完整信息
    pub review: Option<ChatFullInfo>,
    pub config: Config,
    pub store: Store,
    pub collector: Collector,
    /// 抓取链接里的媒体。
    pub fetcher: Fetcher,
    /// 等待用户回复的编辑提示。
    pub inputs: Inputs,
    /// 已经设置过命令菜单的管理员。
    pub registered_commands: Mutex<HashSet<UserId>>,
    /// 识图自动打标签。
    pub ai: Ai,
}

/// AI 识图功能的状态。没有配置模型时 `vision` 为空，整个功能关闭。
#[derive(Default)]
pub struct Ai {
    pub vision: Option<Vision>,
    running: Mutex<HashSet<PostId>>,
}

impl Ai {
    pub fn new(config: Option<&VisionConfig>) -> Self {
        Self {
            vision: config.map(Vision::new),
            running: Mutex::default(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.vision.is_some()
    }

    /// 开始处理一篇投稿。同一篇投稿同时只处理一次，避免连点重复扣费；
    /// 已经在处理时返回 `None`。返回值被丢弃时，这篇投稿又可以处理了。
    pub fn begin(&self, post: PostId) -> Option<Running<'_>> {
        let mut running = self.running.lock().expect("the lock is never poisoned");
        // 不能用 `then_some`：它会先构造 `Running`，已经在处理时再把它丢掉，
        // 丢弃时要重新加锁，而这里的锁还没释放。
        if running.insert(post) {
            Some(Running { ai: self, post })
        } else {
            None
        }
    }
}

pub struct Running<'a> {
    ai: &'a Ai,
    post: PostId,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut running = self.ai.running.lock().expect("the lock is never poisoned");
        running.remove(&self.post);
    }
}
