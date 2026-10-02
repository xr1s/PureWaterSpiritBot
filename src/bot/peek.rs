//! `/peek`：按预计的发送顺序查看已排队、还没发布的投稿。
//!
//! 普通管理员直接看到自己的队列；超级管理员先选管理员，可以查看任何人的队列。
//! 列表从下一个发布时段开始一个时段一个时段地模拟出发送顺序，分页往后翻，越晚发的越靠后。
//! 点列表里的 `#编号`，Bot 把这篇投稿原样发到私聊里，并附一张控制卡片；
//! 「发送本页内容」一次发出本页所有投稿。
//!
//! 自己的投稿，卡片上可以取消投稿。别人的投稿（只有超级管理员看得到）可以撤下：
//! 投稿变成 `rejected`，不会再发布，原管理员收到通知。
//! 撤下时可以直接撤，也可以点「撤下并写理由」再回复一条理由。

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use jiff::{Timestamp, tz::TimeZone};
use teloxide::{
    prelude::*,
    types::{CallbackQuery, CallbackQueryId, InlineKeyboardMarkup, Message, MessageId, Recipient},
};

use super::{
    callback::PeekAction,
    clear_keyboard,
    edit::{self, Input, PendingInput},
    mark_control_cancelled, mark_control_removed,
    names::user_name,
    replace_card, replying_to, send,
    view::{self, PeekRow},
};
use crate::{
    app::App,
    model::{NotificationSubject, Post, PostId, PostStatus},
    schedule::parse_reason,
    selection::{self, Upcoming},
    store::RemoveOutcome,
    telegram::publish_post,
};

/// `/peek`：发一条新消息。超级管理员先选管理员，普通管理员直接看自己的队列。
pub async fn open(app: &App, user: UserId, chat: ChatId) -> Result<()> {
    if !app.config.is_admin(user) {
        return send(app, chat, view::NOT_ALLOWED).await;
    }
    let (text, keyboard) = if app.config.is_super(user) {
        admins_panel(app).await?
    } else {
        list_panel(app, user, user, 0).await?
    };
    app.bot
        .send_message(chat, text)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn admins_panel(app: &App) -> Result<(String, InlineKeyboardMarkup)> {
    let counts = app.store.queued_counts_by_admin().await?;
    let mut rows = Vec::new();
    for admin in app.config.admin_ids().into_iter() {
        let name = user_name(app, admin).await;
        let queued = counts.get(&admin).copied().unwrap_or(0);
        rows.push(view::peek_admin_button(&name, queued, admin));
    }
    if rows.is_empty() {
        return Ok((
            view::PEEK_NO_ONE.to_owned(),
            InlineKeyboardMarkup::default(),
        ));
    }
    Ok((
        view::PEEK_PICK_ADMIN.to_owned(),
        view::peek_admins_keyboard(rows),
    ))
}

/// `admin` 所有排队投稿的发送顺序。读不出时间表时调度器也不会替他发布，
/// 所以这时所有投稿都算没有时段会发。
async fn send_order(app: &App, admin: UserId) -> Result<Vec<Upcoming>> {
    let candidates = app.store.queued_candidates(admin).await?;
    let schedule = match app.store.load_schedule(admin, &app.config.schedule).await {
        Ok(schedule) => schedule,
        Err(error) => {
            tracing::warn!(
                "Failed to load the schedule of admin {}: {error:#}",
                admin.0
            );
            None
        }
    };
    match schedule {
        Some(schedule) => selection::upcoming(
            &schedule.slots,
            &schedule.timezone,
            Timestamp::now(),
            &candidates,
        ),
        None => Ok(candidates
            .into_iter()
            .map(|candidate| Upcoming {
                post: candidate.id,
                at: None,
            })
            .collect()),
    }
}

/// 取一页排队投稿。页码超出范围（比如期间有投稿发出或被撤下、页数变少）就取最后一页。
/// 返回实际的页码、总篇数和这一页的内容。
async fn load_page(app: &App, admin: UserId, page: u32) -> Result<(usize, usize, Vec<PeekRow>)> {
    let order = send_order(app, admin).await?;
    let total = order.len();
    let index = usize::try_from(page)?.min(view::peek_page_count(total) - 1);
    let page: Vec<&Upcoming> = order
        .iter()
        .skip(index * view::PEEK_PAGE_SIZE)
        .take(view::PEEK_PAGE_SIZE)
        .collect();
    let ids: Vec<PostId> = page.iter().map(|upcoming| upcoming.post).collect();
    let times: HashMap<PostId, Option<Timestamp>> = page
        .iter()
        .map(|upcoming| (upcoming.post, upcoming.at))
        .collect();
    let rows = app
        .store
        .peek_items(&ids)
        .await?
        .into_iter()
        .map(|item| PeekRow {
            at: times.get(&item.id).copied().flatten(),
            item,
        })
        .collect();
    Ok((index, total, rows))
}

/// 查看者自己的时区，用来显示发送时间。
async fn viewer_zone(app: &App, me: UserId) -> TimeZone {
    match app.store.load_schedule(me, &app.config.schedule).await {
        Ok(Some(schedule)) => schedule.timezone,
        _ => app.config.schedule.timezone.clone(),
    }
}

async fn list_panel(
    app: &App,
    me: UserId,
    admin: UserId,
    page: u32,
) -> Result<(String, InlineKeyboardMarkup)> {
    let (index, total, rows) = load_page(app, admin, page).await?;
    let name = user_name(app, admin).await;
    let zone = viewer_zone(app, me).await;
    Ok((
        view::peek_list(&name, total, index, &rows, &zone),
        view::peek_list_keyboard(admin, total, index, &rows, app.config.is_super(me)),
    ))
}

async fn alert(app: &App, query: CallbackQueryId, text: &str) -> Result<()> {
    app.bot
        .answer_callback_query(query)
        .text(text)
        .show_alert(true)
        .await?;
    Ok(())
}

/// 这篇投稿是否还在排队，并且属于 `me`。
async fn owns_queued(app: &App, post: PostId, me: UserId) -> Result<bool> {
    Ok(app
        .store
        .post_summary(post)
        .await?
        .is_some_and(|summary| summary.status == PostStatus::Queued && summary.owner == Some(me)))
}

pub async fn handle_action(app: &App, query: CallbackQuery, action: PeekAction) -> Result<()> {
    let Some(message) = &query.message else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    let me = query.from.id;
    // 只有管理员在私聊里的按钮有效。
    if !app.config.is_admin(me) || !message.chat().is_private() {
        return alert(app, query.id, view::NOT_ALLOWED).await;
    }
    let is_super = app.config.is_super(me);
    // 普通管理员只能看自己的队列；单篇投稿的权限在下面按投稿检查。
    let allowed = match action {
        PeekAction::Admins | PeekAction::Remove(_) | PeekAction::RemoveWithReason(_) => is_super,
        PeekAction::List { admin, .. } | PeekAction::SendPage { admin, .. } => {
            app.config.is_admin(admin) && (admin == me || is_super)
        }
        PeekAction::View(_)
        | PeekAction::Cancel(_)
        | PeekAction::ConfirmCancel(_)
        | PeekAction::Back(_) => true,
    };
    if !allowed {
        return alert(app, query.id, view::NOT_ALLOWED).await;
    }
    let (chat, card) = (message.chat().id, message.id());
    match action {
        PeekAction::Admins => {
            app.bot.answer_callback_query(query.id).await?;
            let (text, keyboard) = admins_panel(app).await?;
            replace_card(&app.bot, chat, card, text, keyboard).await;
        }
        PeekAction::List { admin, page } => {
            app.bot.answer_callback_query(query.id).await?;
            let (text, keyboard) = list_panel(app, me, admin, page).await?;
            replace_card(&app.bot, chat, card, text, keyboard).await;
        }
        PeekAction::SendPage { admin, page } => {
            app.bot.answer_callback_query(query.id).await?;
            let (_, _, rows) = load_page(app, admin, page).await?;
            for row in rows {
                let post = row.item.id;
                if let Err(error) = send_post(app, chat, post, me).await {
                    tracing::error!(
                        "Failed to send post {} to admin {}: {error:#}",
                        post.0,
                        me.0
                    );
                    send(app, chat, &view::peek_send_failed(post)).await?;
                }
            }
        }
        PeekAction::View(post) => match send_post(app, chat, post, me).await {
            Ok(true) => {
                app.bot.answer_callback_query(query.id).await?;
            }
            Ok(false) => return alert(app, query.id, view::PEEK_NOT_QUEUED).await,
            Err(error) => {
                tracing::error!(
                    "Failed to send post {} to admin {}: {error:#}",
                    post.0,
                    me.0
                );
                return alert(app, query.id, &view::peek_send_failed(post)).await;
            }
        },
        PeekAction::Cancel(post) => {
            if !owns_queued(app, post, me).await? {
                clear_keyboard(&app.bot, chat, card).await;
                return alert(app, query.id, view::PEEK_NOT_QUEUED).await;
            }
            app.bot
                .answer_callback_query(query.id)
                .text(view::cancel_question(post))
                .await?;
            app.bot
                .edit_message_reply_markup(chat, card)
                .reply_markup(view::peek_cancel_confirm_keyboard(post))
                .await?;
        }
        PeekAction::ConfirmCancel(post) => {
            if !app.store.cancel_post(post, me).await? {
                clear_keyboard(&app.bot, chat, card).await;
                return alert(app, query.id, view::ALREADY_HANDLED).await;
            }
            tracing::info!("Post {} cancelled from /peek", post.0);
            let text = view::post_cancelled(post);
            app.bot
                .answer_callback_query(query.id)
                .text(text.clone())
                .await?;
            mark_control_cancelled(app, post).await;
            replace_card(&app.bot, chat, card, text, InlineKeyboardMarkup::default()).await;
        }
        PeekAction::Back(post) => {
            app.bot.answer_callback_query(query.id).await?;
            if owns_queued(app, post, me).await? {
                app.bot
                    .edit_message_reply_markup(chat, card)
                    .reply_markup(view::peek_card_keyboard(post, true))
                    .await?;
            } else {
                clear_keyboard(&app.bot, chat, card).await;
            }
        }
        PeekAction::Remove(post) => match remove(app, post, me, None).await? {
            Some(owner) => {
                app.bot.answer_callback_query(query.id).await?;
                let name = user_name(app, owner).await;
                replace_card(
                    &app.bot,
                    chat,
                    card,
                    view::peek_removed(post, &name),
                    InlineKeyboardMarkup::default(),
                )
                .await;
            }
            None => {
                clear_keyboard(&app.bot, chat, card).await;
                return alert(app, query.id, view::PEEK_NOT_QUEUED).await;
            }
        },
        PeekAction::RemoveWithReason(post) => {
            let summary = app.store.post_summary(post).await?;
            let Some(owner) = summary
                .filter(|summary| summary.status == PostStatus::Queued)
                .and_then(|summary| summary.owner)
            else {
                clear_keyboard(&app.bot, chat, card).await;
                return alert(app, query.id, view::PEEK_NOT_QUEUED).await;
            };
            app.bot.answer_callback_query(query.id).await?;
            let name = user_name(app, owner).await;
            edit::ask_reason(
                app,
                &query.from,
                (chat, card),
                Input::RemoveReason(post),
                &view::remove_reason_prompt(post, &name),
            )
            .await?;
        }
    }
    Ok(())
}

/// 把排队中的投稿原样发到 `chat`，再发一张控制卡片。返回 `false` 表示它已经不在队列里，
/// 或者 `viewer` 不能看它（普通管理员只能看自己的投稿）。
async fn send_post(app: &App, chat: ChatId, post_id: PostId, viewer: UserId) -> Result<bool> {
    let Some(summary) = app.store.post_summary(post_id).await? else {
        return Ok(false);
    };
    if summary.status != PostStatus::Queued {
        return Ok(false);
    }
    let own = summary.owner == Some(viewer);
    if !own && !app.config.is_super(viewer) {
        return Ok(false);
    }
    let Some(post) = app.store.load_post(post_id).await? else {
        return Ok(false);
    };
    let preview = Post {
        reply_to: None,
        ..post
    };
    let sent = publish_post(&app.bot, &Recipient::Id(chat), &preview)
        .await
        .map_err(|error| anyhow!("failed to send the post: {error:?}"))?;
    let first = sent
        .first()
        .map(|message| message.message_id)
        .ok_or_else(|| anyhow!("the post has no messages"))?;
    let owner = match summary.owner {
        Some(owner) => user_name(app, owner).await,
        None => "?".to_owned(),
    };
    let category = summary
        .category
        .map_or_else(String::new, |category| category.label);
    app.bot
        .send_message(chat, view::peek_card(post_id, &category, &owner))
        .reply_parameters(replying_to(first))
        .reply_markup(view::peek_card_keyboard(post_id, own))
        .await?;
    Ok(true)
}

/// 撤下一篇排队中的投稿，通知原管理员。返回原管理员，`None` 表示投稿已经不在队列里。
async fn remove(
    app: &App,
    post: PostId,
    by: UserId,
    reason: Option<String>,
) -> Result<Option<UserId>> {
    let now = Timestamp::now();
    match app
        .store
        .remove_queued_post(post, by, reason.as_deref(), now)
        .await?
    {
        RemoveOutcome::NotQueued => Ok(None),
        RemoveOutcome::Removed { owner } => {
            tracing::info!(
                "Super admin {} removed post {} of admin {}",
                by.0,
                post.0,
                owner.0
            );
            app.store
                .enqueue_notification(NotificationSubject::PostRemoved(post), &[owner], now)
                .await?;
            mark_control_removed(app, post).await;
            Ok(Some(owner))
        }
    }
}

/// 超级管理员回复了「撤下并写理由」的提示：用他写的理由撤下这篇投稿。
pub async fn finish_remove(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: &PendingInput,
    post: PostId,
    text: &str,
) -> Result<()> {
    let reason = match parse_reason(text) {
        Ok(reason) => reason,
        Err(error) => {
            return edit::complain(app, message, prompt, &format!("{error}，请重新回复。")).await;
        }
    };
    if !app.config.is_super(pending.admin) {
        return Ok(());
    }
    let removed = remove(app, post, pending.admin, Some(reason)).await?;
    edit::close_prompt(app, message, prompt, pending).await;
    let (chat, card) = pending.panel;
    match removed {
        Some(owner) => {
            let name = user_name(app, owner).await;
            replace_card(
                &app.bot,
                chat,
                card,
                view::peek_removed(post, &name),
                InlineKeyboardMarkup::default(),
            )
            .await;
            Ok(())
        }
        None => {
            clear_keyboard(&app.bot, chat, card).await;
            send(app, message.chat.id, view::PEEK_NOT_QUEUED).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicI32, Ordering},
        time::Duration,
    };

    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{body_string_contains, method, path_regex},
    };

    use super::*;
    use crate::{
        bot::{Inputs, callback::CallbackData, test_support::bodies_of},
        collector::Collector,
        config::Config,
        store::{
            Store,
            test_support::{ADMIN, incoming, insert_category, insert_slot},
        },
    };

    const EXAMPLE: &str = include_str!("../../PureWaterSpiritBot.example.toml");
    /// 示例配置里的第二个管理员，不是超级管理员。
    const OWNER: UserId = UserId(987654321);
    const PRIVATE_CHAT: i64 = 42;

    struct Sequence(AtomicI32);

    impl Respond for Sequence {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let id = self.0.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(id),
            }))
        }
    }

    fn message(id: i32) -> Value {
        json!({
            "message_id": id,
            "date": 1,
            "chat": { "id": PRIVATE_CHAT, "type": "private", "first_name": "chat" },
            "text": "hello"
        })
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        server: MockServer,
        app: App,
    }

    /// 超级管理员是测试数据库里的管理员 42，投稿属于管理员 `OWNER`。
    async fn fixture() -> Fixture {
        let server = MockServer::start().await;
        let (directory, store) = Store::open_temporary().await;
        store
            .sync_admins(&[ADMIN, OWNER], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let (collector, _batches) = Collector::new(Duration::from_secs(1));
        let app = App {
            bot: Bot::new("test-token").set_api_url(format!("{}/", server.uri()).parse().unwrap()),
            channel: crate::bot::test_support::channel(),
            review: None,
            config: Config::parse(&EXAMPLE.replace("123456789", "42")).unwrap(),
            store,
            collector,
            fetcher: crate::fetch::Fetcher::unavailable(),
            inputs: Inputs::default(),
            registered_commands: Default::default(),
            ai: Default::default(),
        };
        Fixture {
            _directory: directory,
            server,
            app,
        }
    }

    async fn mock(server: &MockServer, name: &str, result: Value) {
        Mock::given(method("POST"))
            .and(path_regex(format!("(?i).*/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": result,
            })))
            .mount(server)
            .await;
    }

    /// 让 `getChat` 把 `OWNER` 查成名叫 Bob 的用户。
    async fn mock_names(server: &MockServer) {
        let chat = json!({
            "id": OWNER.0,
            "type": "private",
            "first_name": "Bob",
            "max_reaction_count": 0,
            "accepted_gift_types": {
                "unlimited_gifts": true,
                "limited_gifts": true,
                "unique_gifts": true,
                "premium_subscription": true
            },
        });
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/getchat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": chat,
            })))
            .mount(server)
            .await;
    }

    /// `OWNER` 名下一篇排队中的投稿。
    async fn queued_post(app: &App, text: &str) -> PostId {
        let category = insert_category(&app.store, OWNER, "theirs").await;
        let mut message = incoming(1, text);
        message.submitter = OWNER;
        let post = app
            .store
            .create_draft(&[message], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        app.store
            .queue_post(post, OWNER, category, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        post
    }

    fn callback(from: u64, action: PeekAction) -> CallbackQuery {
        serde_json::from_value(json!({
            "id": "query-1",
            "from": { "id": from, "is_bot": false, "first_name": format!("User{from}") },
            "chat_instance": "instance",
            "data": CallbackData::Peek(action).encode(),
            "message": message(7),
        }))
        .unwrap()
    }

    async fn status_of(app: &App, post: PostId) -> PostStatus {
        app.store.review_info(post).await.unwrap().unwrap().status
    }

    async fn notified(app: &App) -> Vec<(NotificationSubject, UserId)> {
        app.store
            .undelivered_notifications()
            .await
            .unwrap()
            .into_iter()
            .map(|notification| (notification.subject, notification.recipient))
            .collect()
    }

    fn private_message(from: u64, id: i32, text: &str) -> Message {
        serde_json::from_value(json!({
            "message_id": id,
            "date": 1,
            "chat": { "id": PRIVATE_CHAT, "type": "private", "first_name": "chat" },
            "from": { "id": from, "is_bot": false, "first_name": format!("User{from}") },
            "text": text,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn removing_notifies_the_owner_once() {
        let fixture = fixture().await;
        let app = &fixture.app;
        let post = queued_post(app, "a").await;

        let owner = remove(app, post, ADMIN, Some("重复了".to_owned()))
            .await
            .unwrap();
        assert_eq!(owner, Some(OWNER));
        assert_eq!(status_of(app, post).await, PostStatus::Rejected);
        assert_eq!(
            notified(app).await,
            [(NotificationSubject::PostRemoved(post), OWNER)]
        );

        assert_eq!(remove(app, post, ADMIN, None).await.unwrap(), None);
        assert_eq!(notified(app).await.len(), 1);
    }

    #[tokio::test]
    async fn the_owners_control_card_loses_its_cancel_button() {
        let fixture = fixture().await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/editmessagetext"))
            .and(body_string_contains("撤下"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(77),
            })))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;
        app.store
            .set_control_message(post, ChatId(PRIVATE_CHAT), MessageId(77))
            .await
            .unwrap();
        remove(app, post, ADMIN, None).await.unwrap();
    }

    #[tokio::test]
    async fn only_super_admins_can_remove() {
        let fixture = fixture().await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("\"show_alert\":true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;

        // OWNER 是管理员，但不是超级管理员。
        let action = PeekAction::Remove(post);
        handle_action(app, callback(OWNER.0, action), action)
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
    }

    #[tokio::test]
    async fn the_remove_button_removes_and_updates_the_card() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        mock(&fixture.server, "answercallbackquery", json!(true)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/editmessagetext"))
            .and(body_string_contains("已通知 Bob"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(7),
            })))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;

        let action = PeekAction::Remove(post);
        handle_action(app, callback(ADMIN.0, action), action)
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Rejected);
        assert_eq!(notified(app).await.len(), 1);
    }

    #[tokio::test]
    async fn viewing_sends_the_post_and_a_card_with_remove_buttons() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        mock(&fixture.server, "answercallbackquery", json!(true)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(2)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "hello").await;

        let action = PeekAction::View(post);
        handle_action(app, callback(ADMIN.0, action), action)
            .await
            .unwrap();
        // 只是看，投稿还在队列里。
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
    }

    #[tokio::test]
    async fn a_removed_post_cannot_be_viewed() {
        let fixture = fixture().await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("已经不在队列里"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;
        remove(app, post, ADMIN, None).await.unwrap();

        let action = PeekAction::View(post);
        handle_action(app, callback(ADMIN.0, action), action)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn remove_with_reason_asks_for_a_reply_and_uses_it() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        mock(&fixture.server, "answercallbackquery", json!(true)).await;
        mock(&fixture.server, "deletemessage", json!(true)).await;
        mock(&fixture.server, "editmessagetext", message(7)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            // 私聊里用强制回复。
            .and(body_string_contains("force_reply"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;

        let action = PeekAction::RemoveWithReason(post);
        handle_action(app, callback(ADMIN.0, action), action)
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
        let pending = app
            .inputs
            .get(ChatId(PRIVATE_CHAT), MessageId(500))
            .expect("the prompt is waiting for a reply");

        let reply = private_message(ADMIN.0, 600, "已经发过了");
        finish_remove(app, &reply, MessageId(500), &pending, post, "已经发过了")
            .await
            .unwrap();

        let info = app.store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.status, PostStatus::Rejected);
        assert_eq!(info.review_note.as_deref(), Some("已经发过了"));
        assert_eq!(info.reviewed_by, Some(ADMIN));
        assert_eq!(notified(app).await.len(), 1);
        assert!(
            app.inputs
                .get(ChatId(PRIVATE_CHAT), MessageId(500))
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_blank_reason_keeps_the_post_queued() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        mock(&fixture.server, "answercallbackquery", json!(true)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;
        let action = PeekAction::RemoveWithReason(post);
        handle_action(app, callback(ADMIN.0, action), action)
            .await
            .unwrap();
        let pending = app
            .inputs
            .get(ChatId(PRIVATE_CHAT), MessageId(500))
            .unwrap();

        let reply = private_message(ADMIN.0, 600, "  ");
        finish_remove(app, &reply, MessageId(500), &pending, post, "  ")
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
        assert!(
            app.inputs
                .get(ChatId(PRIVATE_CHAT), MessageId(500))
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_page_is_clamped_when_posts_disappear() {
        let fixture = fixture().await;
        let app = &fixture.app;
        queued_post(app, "a").await;
        let (index, total, rows) = load_page(app, OWNER, 7).await.unwrap();
        assert_eq!((index, total), (0, 1));
        assert_eq!(rows.len(), 1);
        // OWNER 没有发布时段，这篇投稿不会被发出。
        assert_eq!(rows[0].at, None);

        let (index, total, rows) = load_page(app, ADMIN, 3).await.unwrap();
        assert_eq!((index, total), (0, 0));
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn the_page_follows_the_send_order() {
        let fixture = fixture().await;
        let app = &fixture.app;
        // 测试里的时段只认识 gi 这类固定的分类名。
        let category = insert_category(&app.store, OWNER, "gi").await;
        let mut queued = Vec::new();
        for (id, at) in [(1, 0), (2, 10)] {
            let mut message = incoming(id, "post");
            message.submitter = OWNER;
            let post = app
                .store
                .create_draft(&[message], Timestamp::UNIX_EPOCH, None)
                .await
                .unwrap()
                .unwrap();
            app.store
                .queue_post(post, OWNER, category, Timestamp::from_second(at).unwrap())
                .await
                .unwrap();
            queued.push(post);
        }
        let (first, second) = (queued[0], queued[1]);
        insert_slot(&app.store, OWNER, "09:00", &[("gi", 1, &[])]).await;

        let (_, total, rows) = load_page(app, OWNER, 0).await.unwrap();
        assert_eq!(total, 2);
        // 最晚入队的先发，每天发一篇。
        assert_eq!(
            rows.iter().map(|row| row.item.id).collect::<Vec<_>>(),
            [second, first]
        );
        let (first_at, second_at) = (rows[0].at.unwrap(), rows[1].at.unwrap());
        assert!(first_at > Timestamp::now());
        assert_eq!(
            second_at.duration_since(first_at),
            jiff::SignedDuration::from_hours(24)
        );
    }

    #[tokio::test]
    async fn a_normal_admin_cannot_look_at_other_queues() {
        let fixture = fixture().await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("\"show_alert\":true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(3)
            .mount(&fixture.server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(0)
            .mount(&fixture.server)
            .await;
        let mut message = incoming(1, "mine");
        message.submitter = ADMIN;
        let post = app
            .store
            .create_draft(&[message], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        let category = insert_category(&app.store, ADMIN, "mine").await;
        app.store
            .queue_post(post, ADMIN, category, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();

        for action in [
            PeekAction::Admins,
            PeekAction::List {
                admin: ADMIN,
                page: 0,
            },
            PeekAction::SendPage {
                admin: ADMIN,
                page: 0,
            },
        ] {
            let result = handle_action(app, callback(OWNER.0, action), action).await;
            result.unwrap();
        }
    }

    #[tokio::test]
    async fn a_normal_admin_cannot_view_someone_elses_post() {
        let fixture = fixture().await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("已经不在队列里"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(1)
            .mount(&fixture.server)
            .await;
        let mut message = incoming(1, "mine");
        message.submitter = ADMIN;
        let post = app
            .store
            .create_draft(&[message], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        let category = insert_category(&app.store, ADMIN, "mine").await;
        app.store
            .queue_post(post, ADMIN, category, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();

        let action = PeekAction::View(post);
        handle_action(app, callback(OWNER.0, action), action)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_normal_admin_opens_their_own_list_directly() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "a").await;

        open(app, OWNER, ChatId(PRIVATE_CHAT)).await.unwrap();

        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[0].contains("按发送顺序排列"), "{}", sent[0]);
        assert!(sent[0].contains(&format!("pv:{}", post.0)), "{}", sent[0]);
        assert!(!sent[0].contains("\"p\""), "{}", sent[0]);
    }

    #[tokio::test]
    async fn own_posts_get_a_cancel_button_that_cancels_after_confirming() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        mock(&fixture.server, "answercallbackquery", json!(true)).await;
        mock(&fixture.server, "editmessagereplymarkup", message(7)).await;
        mock(&fixture.server, "editmessagetext", message(7)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(2)
            .mount(&fixture.server)
            .await;
        let post = queued_post(app, "hello").await;

        let view = PeekAction::View(post);
        handle_action(app, callback(OWNER.0, view), view)
            .await
            .unwrap();
        let card = bodies_of(&fixture.server, "sendmessage").await.remove(1);
        assert!(card.contains(&format!("pc:{}", post.0)), "{card}");
        assert!(!card.contains(&format!("px:{}", post.0)), "{card}");

        let cancel = PeekAction::Cancel(post);
        handle_action(app, callback(OWNER.0, cancel), cancel)
            .await
            .unwrap();
        let confirm = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(
            confirm[0].contains(&format!("pk:{}", post.0)),
            "{}",
            confirm[0]
        );
        assert_eq!(status_of(app, post).await, PostStatus::Queued);

        let back = PeekAction::Back(post);
        handle_action(app, callback(OWNER.0, back), back)
            .await
            .unwrap();
        let restored = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(
            restored[1].contains(&format!("pc:{}", post.0)),
            "{}",
            restored[1]
        );

        let confirm = PeekAction::ConfirmCancel(post);
        handle_action(app, callback(OWNER.0, confirm), confirm)
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Cancelled);
        // 取消自己的投稿不发通知。
        assert!(notified(app).await.is_empty());
        let cards = bodies_of(&fixture.server, "editmessagetext").await;
        assert!(cards.last().unwrap().contains("已取消"), "{cards:?}");
    }

    #[tokio::test]
    async fn the_admin_picker_includes_the_viewer() {
        let fixture = fixture().await;
        let app = &fixture.app;
        mock_names(&fixture.server).await;
        queued_post(app, "a").await;
        let (text, keyboard) = admins_panel(app).await.unwrap();
        assert_eq!(text, view::PEEK_PICK_ADMIN);
        let labels: Vec<_> = keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.text.clone())
            .collect();
        assert_eq!(labels, ["Bob · 排队 0", "Bob · 排队 1"]);
    }
}
