mod app;
mod bot;
mod cli;
mod collector;
mod config;
mod fetch;
mod jobs;
mod model;
mod schedule;
mod selection;
mod store;
mod telegram;
mod vision;

use std::{borrow::Cow, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use jiff::Timestamp;
use secrecy::ExposeSecret;
use teloxide::{
    prelude::*,
    types::{BotCommandScope, ChatFullInfo},
    utils::command::BotCommands,
};
use tokio::{spawn, sync::watch};

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::Args::parse();
    init_tracing();
    if let Some(database_url) = &args.init_db {
        let _store = store::Store::open(database_url).await?;
        tracing::info!("Database schema initialized.");
        return Ok(());
    }
    tracing::info!("Starting PureWaterSpiritBot");

    let config = config::Config::load(&args.config)?;
    let timezone = &config.schedule.timezone;
    let human_friendly_timezone = timezone
        .to_fixed_offset()
        .ok()
        .map(|offset| offset.to_string())
        .map(Cow::Owned)
        .or_else(|| timezone.iana_name().map(Cow::Borrowed))
        .unwrap_or_else(|| Cow::Owned(format!("{:?}", timezone)));
    tracing::info!(
        "Configuration loaded: {} admin(s), review group {}abled, default timezone {}",
        config.admins.len(),
        config.review.group.as_ref().map_or("dis", |_| "en"),
        human_friendly_timezone,
    );

    let bot = Bot::new(config.token.expose_secret());
    let me = bot.get_me().await?;
    let channel = telegram::verify_channel(&bot, &me, &config.channel).await?;
    // 清空私聊的默认菜单，在管理员第一次向 Bot 说话时注册
    bot.delete_my_commands()
        .scope(BotCommandScope::AllPrivateChats)
        .await
        .context("failed to clear the default bot commands")?;
    let review: Option<ChatFullInfo> = if let Some(group) = &config.review.group {
        let review = telegram::verify_review_group(&bot, &me, group).await?;
        bot.set_my_commands(bot::ReviewCommand::bot_commands())
            .scope(BotCommandScope::Chat {
                chat_id: group.clone(),
            })
            .await
            .context("failed to register the review chat commands")?;
        Some(review)
    } else {
        None
    };

    let store = store::Store::open(&config.database.url).await?;
    let synced = store
        .sync_admins(&config.admin_ids(), Timestamp::now())
        .await?;
    for admin in &synced.deactivated {
        tracing::warn!(
            "Admin {} is no longer configured and was deactivated",
            admin.0
        );
    }
    tracing::info!("Database ready");

    let fetcher = fetch::Fetcher::new(&config.fetch).await;
    if !fetcher.is_available() {
        tracing::warn!("Neither gallery-dl nor yt-dlp is available; links will not be fetched");
    }
    let (collector, batches) = collector::Collector::new(config.submission.media_group_wait.get());
    let ai = app::Ai::new(config.vision.as_ref());
    match &config.vision {
        Some(vision) => tracing::info!("AI image analysis enabled with model {}", vision.model),
        None => tracing::info!("AI image analysis is not configured"),
    }
    let app = Arc::new(app::App {
        bot,
        channel,
        review,
        config,
        store,
        collector,
        fetcher,
        inputs: bot::Inputs::default(),
        registered_commands: Default::default(),
        ai,
    });

    let (shutdown_sender, shutdown) = watch::channel(false);
    spawn(bot::create_drafts(app.clone(), batches));
    let housekeeping = spawn(jobs::housekeeping::run(app.clone(), shutdown.clone()));
    let scheduler = spawn(jobs::scheduler::run(app.clone(), shutdown));

    tracing::info!("Bot is running and waiting for messages; press Ctrl-C to stop");
    Dispatcher::builder(app.bot.clone(), bot::handler())
        .dependencies(dptree::deps![app])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    tracing::info!("Shutting down");
    shutdown_sender.send_replace(true);
    let _ = tokio::join!(housekeeping, scheduler);
    Ok(())
}

fn init_tracing() {
    use jiff::tz::TimeZone;
    use tracing::Level;
    use tracing_subscriber::{
        fmt::{format::Writer, time::FormatTime},
        layer::SubscriberExt,
        util::SubscriberInitExt,
    };
    struct TimeFormatter {
        tz: TimeZone,
    }
    impl FormatTime for TimeFormatter {
        fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
            let now = jiff::Timestamp::now().to_zoned(self.tz.clone());
            w.write_str(&now.strftime("%Y-%m-%d %H:%M:%S%z").to_string())
        }
    }
    let timer = TimeFormatter {
        tz: TimeZone::system(),
    };
    let targets = tracing_subscriber::filter::Targets::new()
        .with_default(Level::WARN)
        .with_target(env!("CARGO_PKG_NAME"), tracing::Level::INFO);
    let formatter = tracing_subscriber::fmt::layer().with_timer(timer).compact();
    tracing_subscriber::registry()
        .with(targets)
        .with(formatter)
        .init();
}
