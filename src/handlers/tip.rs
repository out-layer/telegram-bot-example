use std::sync::Arc;
use std::time::Instant;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ReplyParameters};

use super::TokenConfig;
use crate::outlayer::{adjust_for_dust, format_amount, parse_amount};
use crate::AppState;

const RATE_LIMIT_SECS: u64 = 5;
const CLAIM_RETRIES: u32 = 3;
const CLAIM_RETRY_DELAY_SECS: u64 = 2;

macro_rules! reply {
    ($bot:expr, $msg:expr, $text:expr) => {
        $bot.send_message($msg.chat.id, $text)
            .reply_parameters(ReplyParameters::new($msg.id))
            .await?
    };
}

pub async fn handle(
    bot: Bot,
    msg: Message,
    state: Arc<AppState>,
    args: String,
    token: &TokenConfig,
    min_tip: f64,
    max_tip: f64,
) -> ResponseResult<()> {
    if msg.chat.is_private() {
        reply!(
            bot,
            msg,
            format!("Use /{} in a group chat by replying to a message.", token.symbol.to_lowercase())
        );
        return Ok(());
    }

    let reply = match msg.reply_to_message() {
        Some(r) => r,
        None => {
            reply!(
                bot,
                msg,
                format!(
                    "Reply to a message to tip.\nExample: /{} {min_tip}",
                    token.symbol.to_lowercase()
                )
            );
            return Ok(());
        }
    };

    // If replying to a bot's dice game message, route to join game
    if reply.from.as_ref().map(|u| u.is_bot).unwrap_or(false) {
        let key = (msg.chat.id.0, reply.id.0);
        if state.dice_msg_index.contains_key(&key) {
            return super::dice::join_game(bot, msg, state, args, token).await;
        }
    }

    let sender = match &msg.from {
        Some(u) => u,
        None => return Ok(()),
    };
    let receiver = match &reply.from {
        Some(u) => u,
        None => {
            reply!(bot, msg, "Can't identify the recipient.");
            return Ok(());
        }
    };

    if sender.id == receiver.id {
        reply!(bot, msg, "You can't tip yourself.");
        return Ok(());
    }
    if receiver.is_bot {
        reply!(bot, msg, "You can't tip a bot.");
        return Ok(());
    }
    if receiver.username.as_deref() == Some("GroupAnonymousBot") {
        reply!(bot, msg, "Can't tip an anonymous admin.");
        return Ok(());
    }
    if sender.username.as_deref() == Some("GroupAnonymousBot") {
        reply!(bot, msg, "Anonymous admins can't send tips.");
        return Ok(());
    }

    // Parse amount (first word only — rest is optional message)
    let amount_str = args.trim().split_whitespace().next().unwrap_or("");
    let amount_raw = match parse_amount(amount_str, token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            reply!(
                bot,
                msg,
                format!("Invalid amount. Example: /{} {min_tip}", token.symbol.to_lowercase())
            );
            return Ok(());
        }
    };

    let min_raw = parse_amount(&format!("{min_tip}"), token.decimals).unwrap_or(1);
    let max_raw = parse_amount(&format!("{max_tip}"), token.decimals).unwrap_or(u128::MAX);

    if amount_raw < min_raw {
        reply!(bot, msg, format!("Minimum: {min_tip} {}", token.symbol));
        return Ok(());
    }
    if amount_raw > max_raw {
        reply!(bot, msg, format!("Maximum: {max_tip} {}", token.symbol));
        return Ok(());
    }

    // Rate limiting
    let now = Instant::now();
    if let Some(last) = state.rate_limiter.get(&sender.id.0) {
        if now.duration_since(*last).as_secs() < RATE_LIMIT_SECS {
            reply!(bot, msg, "Please wait 5 seconds between tips.");
            return Ok(());
        }
    }
    state.rate_limiter.insert(sender.id.0, now);

    // Prevent concurrent tips from same sender (double-spend protection)
    if state.tip_locks.contains_key(&sender.id.0) {
        reply!(bot, msg, "Another tip is being processed. Please wait.");
        return Ok(());
    }
    state.tip_locks.insert(sender.id.0, ());

    let result = do_tip(&bot, &msg, &state, sender, receiver, amount_raw, token).await;
    state.tip_locks.remove(&sender.id.0);
    return result;
}

async fn do_tip(
    bot: &Bot,
    msg: &Message,
    state: &crate::AppState,
    sender: &teloxide::types::User,
    receiver: &teloxide::types::User,
    amount_raw: u128,
    token: &TokenConfig,
) -> ResponseResult<()> {
    // Register both wallets
    if let Err(e) = state.outlayer.register_wallet(sender.id.0).await {
        tracing::error!(sender = sender.id.0, "register sender: {e}");
        reply!(bot, msg, "Failed to set up sender wallet.");
        return Ok(());
    }
    if let Err(e) = state.outlayer.register_wallet(receiver.id.0).await {
        tracing::error!(receiver = receiver.id.0, "register receiver: {e}");
        reply!(bot, msg, "Failed to set up recipient wallet.");
        return Ok(());
    }

    // Check balance
    let balance_str = match state
        .outlayer
        .get_balance(sender.id.0, &token.contract)
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(sender = sender.id.0, "get_balance: {e}");
            reply!(bot, msg, "Failed to check balance.");
            return Ok(());
        }
    };

    let balance: u128 = balance_str.parse().unwrap_or(0);
    let amount_raw = adjust_for_dust(balance, amount_raw, token.dust);

    if balance < amount_raw {
        let bal_fmt = format_amount(&balance.to_string(), token.decimals, token.display_dp);
        reply!(
            bot,
            msg,
            format!("Insufficient balance: {}{bal_fmt} {}", token.prefix, token.symbol)
        );
        return Ok(());
    }

    let amount_str = amount_raw.to_string();
    let display = format_amount(&amount_str, token.decimals, token.display_dp);

    // Create payment check
    let (check_id, check_key) = match state
        .outlayer
        .create_payment_check(
            sender.id.0,
            &token.contract,
            &amount_str,
            &format!("tip:tg:{}", receiver.id.0),
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(sender = sender.id.0, "create_payment_check: {e}");
            reply!(bot, msg, "Failed to create tip.");
            return Ok(());
        }
    };

    // Claim with retry
    let mut claimed = false;
    for attempt in 0..CLAIM_RETRIES {
        match state
            .outlayer
            .claim_payment_check(receiver.id.0, &check_key)
            .await
        {
            Ok(_) => {
                claimed = true;
                break;
            }
            Err(e) => {
                tracing::warn!(attempt, "claim retry: {e}");
                if attempt + 1 < CLAIM_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_secs(CLAIM_RETRY_DELAY_SECS))
                        .await;
                }
            }
        }
    }

    if !claimed {
        let _ = state
            .outlayer
            .reclaim_payment_check(sender.id.0, &check_id)
            .await;
        reply!(bot, msg, "Tip failed. Funds returned to sender.");
        return Ok(());
    }

    let sender_name = display_name(sender);
    let receiver_name = display_name(receiver);

    let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::url(
        "Open wallet",
        format!("https://t.me/{}?start=tips", state.bot_username)
            .parse()
            .unwrap(),
    )]]);

    bot.send_message(
        msg.chat.id,
        format!(
            "{sender_name} tipped {}{display} {} to {receiver_name}",
            token.prefix, token.symbol
        ),
    )
    .reply_parameters(ReplyParameters::new(msg.id))
    .reply_markup(keyboard)
    .await?;

    let _ = bot
        .send_message(
            ChatId(receiver.id.0 as i64),
            format!(
                "You received {}{display} {} from {sender_name}!\n/start to check your balance.",
                token.prefix, token.symbol
            ),
        )
        .await;

    tracing::info!(
        sender = sender.id.0,
        receiver = receiver.id.0,
        amount = amount_str.as_str(),
        symbol = token.symbol.as_str(),
        "tip sent"
    );

    Ok(())
}

fn display_name(user: &teloxide::types::User) -> String {
    user.username
        .as_ref()
        .map(|u| format!("@{u}"))
        .unwrap_or_else(|| user.first_name.clone())
}
