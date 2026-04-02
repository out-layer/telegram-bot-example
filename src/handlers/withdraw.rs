use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ReplyParameters};

use super::{ConvoState, PendingWithdrawal, HTML};
use crate::outlayer::{format_amount, parse_amount};
use crate::AppState;

pub async fn handle_text(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }

    let user_id = match &msg.from {
        Some(u) => u.id.0,
        None => return Ok(()),
    };

    let convo = match state.conversations.get(&user_id) {
        Some(c) => c.clone(),
        None => return Ok(()),
    };

    match convo {
        ConvoState::WithdrawAddress { token_key } => {
            handle_address(&bot, &msg, &state, user_id, &token_key).await
        }
        ConvoState::WithdrawAmount {
            token_key,
            address,
            chain,
        } => handle_amount(&bot, &msg, &state, user_id, &token_key, &address, &chain).await,
    }
}

async fn handle_address(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    user_id: u64,
    token_key: &str,
) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t.trim(),
        None => {
            bot.send_message(msg.chat.id, "Please send the address as text.")
                .await?;
            return Ok(());
        }
    };

    if text.starts_with('/') {
        return Ok(());
    }

    let address = text.to_string();
    let chain = detect_chain(&address);

    let token = state.token_by_key(token_key);
    let balance = state
        .outlayer
        .get_balance(user_id, &token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let bal_fmt = format_amount(&balance, token.decimals, token.display_dp);

    state.conversations.insert(
        user_id,
        ConvoState::WithdrawAmount {
            token_key: token_key.to_string(),
            address: address.clone(),
            chain: chain.to_string(),
        },
    );

    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback(
            format!("MAX ({bal_fmt})"),
            "w:max",
        )],
        vec![InlineKeyboardButton::callback("❌ Cancel", "cb:menu")],
    ]);

    bot.send_message(
        msg.chat.id,
        format!(
            "<b>📤 Withdraw {sym}</b>\n\n\
             To: <code>{address}</code>\n\
             Network: {chain}\n\
             Available: {pfx}{bal_fmt} {sym}\n\n\
             Enter the amount:",
            sym = token.symbol,
            pfx = token.prefix,
        ),
    )
    .parse_mode(HTML)
    .reply_markup(kb)
    .await?;

    Ok(())
}

async fn handle_amount(
    bot: &Bot,
    msg: &Message,
    state: &AppState,
    user_id: u64,
    token_key: &str,
    address: &str,
    chain: &str,
) -> ResponseResult<()> {
    let text = match msg.text() {
        Some(t) => t.trim(),
        None => {
            bot.send_message(msg.chat.id, "Please send the amount as a number.")
                .await?;
            return Ok(());
        }
    };

    if text.starts_with('/') {
        return Ok(());
    }

    let token = state.token_by_key(token_key);

    let amount_raw = match parse_amount(text, token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            bot.send_message(msg.chat.id, "Invalid amount.")
                .reply_parameters(ReplyParameters::new(msg.id))
                .await?;
            return Ok(());
        }
    };

    let amount_str = amount_raw.to_string();
    let display = format_amount(&amount_str, token.decimals, token.display_dp);

    state.conversations.remove(&user_id);
    state.pending_withdrawals.insert(
        user_id,
        PendingWithdrawal {
            token_key: token_key.to_string(),
            address: address.to_string(),
            chain: chain.to_string(),
            amount_raw: amount_str,
            amount_display: display.clone(),
        },
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ Confirm", "w:ok"),
        InlineKeyboardButton::callback("❌ Cancel", "w:no"),
    ]]);

    bot.send_message(
        msg.chat.id,
        format!(
            "<b>📤 Confirm withdrawal</b>\n\n\
             To: <code>{address}</code>\n\
             Network: {chain}\n\
             Amount: <b>{pfx}{display}</b> {sym}",
            pfx = token.prefix,
            sym = token.symbol,
        ),
    )
    .parse_mode(HTML)
    .reply_markup(kb)
    .await?;

    Ok(())
}

pub async fn handle_max_callback(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
    user_id: u64,
) -> ResponseResult<()> {
    let convo = state.conversations.get(&user_id).map(|c| c.clone());
    let (token_key, address, chain) = match convo {
        Some(ConvoState::WithdrawAmount {
            token_key,
            address,
            chain,
        }) => (token_key, address, chain),
        _ => return Ok(()),
    };

    let token = state.token_by_key(&token_key);

    let balance = state
        .outlayer
        .get_balance(user_id, &token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let bal_val: u128 = balance.parse().unwrap_or(0);

    if bal_val == 0 {
        bot.edit_message_text(chat_id, message_id, "Balance is empty.")
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![super::back_button()]]))
            .await?;
        state.conversations.remove(&user_id);
        return Ok(());
    }

    let display = format_amount(&balance, token.decimals, token.display_dp);

    state.conversations.remove(&user_id);
    state.pending_withdrawals.insert(
        user_id,
        PendingWithdrawal {
            token_key,
            address: address.clone(),
            chain: chain.clone(),
            amount_raw: balance,
            amount_display: display.clone(),
        },
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ Confirm", "w:ok"),
        InlineKeyboardButton::callback("❌ Cancel", "w:no"),
    ]]);

    bot.edit_message_text(
        chat_id,
        message_id,
        format!(
            "<b>📤 Confirm withdrawal</b>\n\n\
             To: <code>{address}</code>\n\
             Network: {chain}\n\
             Amount: <b>{pfx}{display}</b> {sym} (all)",
            pfx = token.prefix,
            sym = token.symbol,
        ),
    )
    .parse_mode(HTML)
    .reply_markup(kb)
    .await?;

    Ok(())
}

fn detect_chain(address: &str) -> &'static str {
    if address.ends_with(".near") || address.ends_with(".testnet") {
        return "near";
    }
    if address.starts_with("0x") && address.len() == 42 {
        return "ethereum";
    }
    if address.len() == 64 && address.chars().all(|c| c.is_ascii_hexdigit()) {
        return "near";
    }
    if (32..=44).contains(&address.len())
        && address
            .chars()
            .all(|c| c.is_ascii_alphanumeric() && c != '0' && c != 'O' && c != 'I' && c != 'l')
    {
        return "solana";
    }
    "near"
}
