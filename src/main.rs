mod handlers;
mod outlayer;

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, BotCommandScope, ReplyParameters};
use teloxide::utils::command::BotCommands;

use handlers::dice::DiceGame;
use handlers::{ConvoState, PendingSwap, PendingWithdrawal, TokenConfig, HTML};
use outlayer::{parse_amount, OutlayerClient};

pub struct AppState {
    pub outlayer: OutlayerClient,
    pub near_token: TokenConfig,
    pub usdc_token: TokenConfig,
    pub fund_base_url: String,
    pub deposit_contract: String,
    pub bot_username: String,
    pub rate_limiter: DashMap<u64, Instant>,
    pub tip_locks: DashMap<u64, ()>,
    pub conversations: DashMap<u64, ConvoState>,
    pub pending_withdrawals: DashMap<u64, PendingWithdrawal>,
    pub pending_swaps: DashMap<u64, PendingSwap>,
    // Dice game state
    pub dice_games: DashMap<u64, DiceGame>,
    pub dice_msg_index: DashMap<(i64, i32), u64>,
    pub dice_player_index: DashMap<(i64, u64), u64>,
    pub dice_next_id: AtomicU64,
    pub dice_allowed_chats: Vec<i64>,
    pub dice_games_file: String,
    pub dice_betting_timeout: u64,
    pub dice_rolling_timeout: u64,
    pub dice_min_near: u128,
    pub dice_max_near: u128,
    pub dice_min_usdc: u128,
    pub dice_max_usdc: u128,
    pub dice_fee: u8,            // 0-100 percent taken from pot
    pub dice_fee_account: u64,   // tg user_id that receives fee
    pub dice_demo: bool,
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
    #[command(description = "Dice game (in group)")]
    Dice(String),
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
    let fund_base_url = std::env::var("FUND_BASE_URL")
        .unwrap_or_else(|_| "https://outlayer.fastnear.com/wallet/fund".into());

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

    // Dice game config
    let dice_allowed_chats: Vec<i64> = std::env::var("DICE_ALLOWED_CHATS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let dice_betting_timeout: u64 = std::env::var("DICE_BETTING_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let dice_rolling_timeout: u64 = std::env::var("DICE_ROLLING_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let dice_games_file = std::env::var("DICE_GAMES_FILE")
        .unwrap_or_else(|_| "./dice_games.json".into());
    let dice_fee: u8 = std::env::var("DICE_FEE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
        .min(100);
    let dice_fee_account: u64 = std::env::var("DICE_FEE_ACCOUNT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let dice_demo = std::env::var("DICE_DEMO")
        .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let dice_min_near = parse_amount(
        &std::env::var("DICE_MIN_NEAR").unwrap_or_else(|_| "0.1".into()), 24,
    ).unwrap_or(100_000_000_000_000_000_000_000);
    let dice_max_near = parse_amount(
        &std::env::var("DICE_MAX_NEAR").unwrap_or_else(|_| "10".into()), 24,
    ).unwrap_or(10_000_000_000_000_000_000_000_000);
    let dice_min_usdc = parse_amount(
        &std::env::var("DICE_MIN_USDC").unwrap_or_else(|_| "0.1".into()), 6,
    ).unwrap_or(100_000);
    let dice_max_usdc = parse_amount(
        &std::env::var("DICE_MAX_USDC").unwrap_or_else(|_| "10".into()), 6,
    ).unwrap_or(10_000_000);

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
            BotCommand::new("dice", "Dice game"),
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
        fund_base_url,
        deposit_contract,
        bot_username,
        rate_limiter: DashMap::new(),
        tip_locks: DashMap::new(),
        conversations: DashMap::new(),
        pending_withdrawals: DashMap::new(),
        pending_swaps: DashMap::new(),
        dice_games: DashMap::new(),
        dice_msg_index: DashMap::new(),
        dice_player_index: DashMap::new(),
        dice_next_id: AtomicU64::new(1), // overwritten by restore_games if file exists
        dice_allowed_chats,
        dice_games_file,
        dice_betting_timeout,
        dice_rolling_timeout,
        dice_min_near,
        dice_max_near,
        dice_min_usdc,
        dice_max_usdc,
        dice_fee,
        dice_fee_account,
        dice_demo,
    });

    // Restore dice games from disk
    handlers::dice::restore_games(&state, &bot);

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<Command>()
                .endpoint(handle_command),
        )
        .branch(
            Update::filter_message()
                .filter(|msg: Message| {
                    msg.dice()
                        .map(|d| d.emoji == teloxide::types::DiceEmoji::Dice)
                        .unwrap_or(false)
                })
                .endpoint(handle_dice_message),
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
            let (text, kb) = handlers::deposit_choose_token();
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

        Command::Dice(args) => {
            handlers::dice::start_game(bot, msg, state, args).await
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

async fn handle_dice_message(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    let dice_value = msg.dice().unwrap().value as u8;
    let user_id = match msg.from.as_ref() {
        Some(u) => u.id.0,
        None => return Ok(()),
    };
    let chat_id = msg.chat.id.0;
    handlers::dice::handle_dice_roll(bot, msg, state, user_id, chat_id, dice_value).await
}

async fn handle_text(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    handlers::withdraw::handle_text(bot, msg, state).await
}
