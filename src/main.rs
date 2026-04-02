mod handlers;
mod outlayer;

use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, BotCommandScope, ReplyParameters};
use teloxide::utils::command::BotCommands;

use handlers::{ConvoState, PendingWithdrawal, TokenConfig, HTML};
use outlayer::OutlayerClient;

pub struct AppState {
    pub outlayer: OutlayerClient,
    pub near_token: TokenConfig,
    pub usdc_token: TokenConfig,
    pub deposit_contract: String,
    pub bot_username: String,
    pub rate_limiter: DashMap<u64, Instant>,
    pub conversations: DashMap<u64, ConvoState>,
    pub pending_withdrawals: DashMap<u64, PendingWithdrawal>,
}

impl AppState {
    pub fn token_by_key(&self, key: &str) -> &TokenConfig {
        match key {
            "near" => &self.near_token,
            _ => &self.usdc_token,
        }
    }
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "Main menu")]
    Start,
    #[command(description = "Check balance")]
    Balance,
    #[command(description = "Deposit")]
    Deposit,
    #[command(description = "Tip NEAR (reply in group)")]
    Near(String),
    #[command(description = "Tip USDC (reply in group)")]
    Usd(String),
    #[command(description = "Withdraw funds")]
    Withdraw,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "outlayer_tipbot=info".parse().unwrap()),
        )
        .init();

    let token = std::env::var("TELEGRAM_BOT_TOKEN").expect("TELEGRAM_BOT_TOKEN must be set");
    let near_account_id = std::env::var("NEAR_ACCOUNT_ID").expect("NEAR_ACCOUNT_ID must be set");
    let near_private_key =
        std::env::var("NEAR_PRIVATE_KEY").expect("NEAR_PRIVATE_KEY must be set");
    let outlayer_api = std::env::var("OUTLAYER_API")
        .unwrap_or_else(|_| "https://api.outlayer.fastnear.com".into());
    let deposit_contract =
        std::env::var("DEPOSIT_CONTRACT").unwrap_or_else(|_| "deposit.tipbot.near".into());

    let wnear_contract = std::env::var("WNEAR_TOKEN")
        .unwrap_or_else(|_| "wrap.near".into());
    let usdc_contract = std::env::var("USDC_TOKEN").unwrap_or_else(|_| {
        "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1".into()
    });

    let near_token = TokenConfig {
        contract: wnear_contract,
        symbol: "NEAR".into(),
        decimals: 24,
        display_dp: 4,
        dust: 10_000_000_000_000_000_000_000, // 0.01 NEAR
        prefix: "",
    };

    let usdc_token = TokenConfig {
        contract: usdc_contract,
        symbol: "USDC".into(),
        decimals: 6,
        display_dp: 2,
        dust: 10_000, // $0.01
        prefix: "$",
    };

    let outlayer_client = OutlayerClient::new(near_account_id, &near_private_key, outlayer_api);
    let bot = Bot::new(&token);

    // Register commands in Telegram's menu
    let _ = bot
        .set_my_commands(vec![
            BotCommand::new("start", "Main menu"),
            BotCommand::new("balance", "Check balance"),
            BotCommand::new("deposit", "Deposit"),
            BotCommand::new("withdraw", "Withdraw"),
        ])
        .scope(BotCommandScope::AllPrivateChats)
        .await;
    let _ = bot
        .set_my_commands(vec![
            BotCommand::new("near", "Tip NEAR (reply)"),
            BotCommand::new("usd", "Tip USDC (reply)"),
        ])
        .scope(BotCommandScope::AllGroupChats)
        .await;

    let me = bot.get_me().await.expect("failed to get bot info");
    let bot_username = me.username().to_string();
    tracing::info!("Starting @{bot_username}");

    let state = Arc::new(AppState {
        outlayer: outlayer_client,
        near_token,
        usdc_token,
        deposit_contract,
        bot_username,
        rate_limiter: DashMap::new(),
        conversations: DashMap::new(),
        pending_withdrawals: DashMap::new(),
    });

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(Update::filter_message().endpoint(handle_text))
        .branch(Update::filter_callback_query().endpoint(handlers::callback::handle));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    state: Arc<AppState>,
) -> ResponseResult<()> {
    if let Some(u) = &msg.from {
        state.conversations.remove(&u.id.0);
        state.pending_withdrawals.remove(&u.id.0);
    }

    match cmd {
        Command::Start => handlers::start::handle(bot, msg, state).await,

        Command::Balance => {
            if !ensure_private(&bot, &msg, &state).await? {
                return Ok(());
            }
            let user_id = msg.from.as_ref().unwrap().id.0;
            let (text, kb) = handlers::balance_view(&state, user_id).await;
            bot.send_message(msg.chat.id, text)
                .parse_mode(HTML)
                .reply_markup(kb)
                .await?;
            Ok(())
        }

        Command::Deposit => {
            if !ensure_private(&bot, &msg, &state).await? {
                return Ok(());
            }
            let user_id = msg.from.as_ref().unwrap().id.0;
            let (text, kb) = handlers::deposit_view(&state, user_id).await;
            bot.send_message(msg.chat.id, text)
                .parse_mode(HTML)
                .reply_markup(kb)
                .await?;
            Ok(())
        }

        Command::Near(args) => {
            handlers::tip::handle(bot, msg, state.clone(), args, &state.near_token, 0.01, 100.0)
                .await
        }

        Command::Usd(args) => {
            handlers::tip::handle(bot, msg, state.clone(), args, &state.usdc_token, 0.01, 100.0)
                .await
        }

        Command::Withdraw => {
            if !ensure_private(&bot, &msg, &state).await? {
                return Ok(());
            }
            // Show token choice
            let kb = teloxide::types::InlineKeyboardMarkup::new(vec![
                vec![
                    teloxide::types::InlineKeyboardButton::callback("NEAR", "cb:withdraw:near"),
                    teloxide::types::InlineKeyboardButton::callback("USDC", "cb:withdraw:usdc"),
                ],
                vec![teloxide::types::InlineKeyboardButton::callback(
                    "❌ Cancel",
                    "cb:menu",
                )],
            ]);

            bot.send_message(
                msg.chat.id,
                "<b>📤 Withdraw</b>\n\nChoose token:",
            )
            .parse_mode(HTML)
            .reply_markup(kb)
            .await?;
            Ok(())
        }
    }
}

async fn ensure_private(bot: &Bot, msg: &Message, state: &AppState) -> ResponseResult<bool> {
    if msg.chat.is_private() {
        return Ok(true);
    }
    bot.send_message(
        msg.chat.id,
        format!("DM me to use this: @{}", state.bot_username),
    )
    .reply_parameters(ReplyParameters::new(msg.id))
    .await?;
    Ok(false)
}

async fn handle_text(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    handlers::withdraw::handle_text(bot, msg, state).await
}
