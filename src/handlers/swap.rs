use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ReplyParameters};

use super::{ConvoState, PendingSwap, HTML};
use crate::outlayer::{format_amount, parse_amount};
use crate::AppState;

/// Handle free text amount input during swap flow.
pub async fn handle_text(bot: &Bot, msg: &Message, state: &AppState, user_id: u64) -> ResponseResult<()> {
    let convo = match state.conversations.get(&user_id) {
        Some(c) => c.clone(),
        None => return Ok(()),
    };

    let (from_key, to_key) = match convo {
        ConvoState::SwapAmount { from_key, to_key } => (from_key, to_key),
        _ => return Ok(()),
    };

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

    let from_token = state.token_by_key(&from_key);
    let to_token = state.token_by_key(&to_key);

    let amount_raw = match parse_amount(text, from_token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            bot.send_message(msg.chat.id, "Invalid amount.")
                .reply_parameters(ReplyParameters::new(msg.id))
                .await?;
            return Ok(());
        }
    };

    let amount_str = amount_raw.to_string();
    let display = format_amount(&amount_str, from_token.decimals, from_token.display_dp);

    state.conversations.remove(&user_id);
    state.pending_swaps.insert(
        user_id,
        PendingSwap {
            from_key: from_key.clone(),
            to_key: to_key.clone(),
            amount_raw: amount_str,
            amount_display: display.clone(),
        },
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ Confirm", "s:ok"),
        InlineKeyboardButton::callback("❌ Cancel", "s:no"),
    ]]);

    bot.send_message(
        msg.chat.id,
        format!(
            "<b>🔄 Confirm swap</b>\n\n\
             {pfx_from}{display} {sym_from} → {sym_to}",
            pfx_from = from_token.prefix,
            sym_from = from_token.symbol,
            sym_to = to_token.symbol,
        ),
    )
    .parse_mode(HTML)
    .reply_markup(kb)
    .await?;

    Ok(())
}

/// Handle MAX button during swap amount step.
pub async fn handle_max_callback(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
    user_id: u64,
) -> ResponseResult<()> {
    let convo = state.conversations.get(&user_id).map(|c| c.clone());
    let (from_key, to_key) = match convo {
        Some(ConvoState::SwapAmount { from_key, to_key }) => (from_key, to_key),
        _ => return Ok(()),
    };

    let from_token = state.token_by_key(&from_key);
    let to_token = state.token_by_key(&to_key);

    let balance = state
        .outlayer
        .get_balance(user_id, &from_token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let bal_val: u128 = balance.parse().unwrap_or(0);

    if bal_val == 0 {
        bot.edit_message_text(chat_id, message_id, format!("{} balance is empty.", from_token.symbol))
            .reply_markup(InlineKeyboardMarkup::new(vec![vec![super::back_button()]]))
            .await?;
        state.conversations.remove(&user_id);
        return Ok(());
    }

    let display = format_amount(&balance, from_token.decimals, from_token.display_dp);

    state.conversations.remove(&user_id);
    state.pending_swaps.insert(
        user_id,
        PendingSwap {
            from_key,
            to_key: to_key.clone(),
            amount_raw: balance,
            amount_display: display.clone(),
        },
    );

    let kb = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("✅ Confirm", "s:ok"),
        InlineKeyboardButton::callback("❌ Cancel", "s:no"),
    ]]);

    bot.edit_message_text(
        chat_id,
        message_id,
        format!(
            "<b>🔄 Confirm swap</b>\n\n\
             {pfx}{display} {sym_from} → {sym_to} (all)",
            pfx = from_token.prefix,
            sym_from = from_token.symbol,
            sym_to = to_token.symbol,
        ),
    )
    .parse_mode(HTML)
    .reply_markup(kb)
    .await?;

    Ok(())
}

/// Execute the swap after confirmation.
pub async fn execute(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
    user_id: u64,
    pending: &PendingSwap,
) {
    // Show processing
    bot.edit_message_text(chat_id, message_id, "⏳ Swapping...")
        .reply_markup(InlineKeyboardMarkup::default())
        .await
        .ok();

    let from_token = state.token_by_key(&pending.from_key);
    let to_token = state.token_by_key(&pending.to_key);

    match state
        .outlayer
        .swap(
            user_id,
            &from_token.contract,
            &to_token.contract,
            &pending.amount_raw,
        )
        .await
    {
        Ok(resp) => {
            // Try to extract output amount from response
            let out_amount = resp["amount_out"]
                .as_str()
                .or_else(|| resp["output_amount"].as_str())
                .unwrap_or("—");

            let out_display = if out_amount != "—" {
                format!(
                    "{}{} {}",
                    to_token.prefix,
                    format_amount(out_amount, to_token.decimals, to_token.display_dp),
                    to_token.symbol
                )
            } else {
                format!("{}", to_token.symbol)
            };

            let text = format!(
                "<b>✅ Swap complete</b>\n\n\
                 {pfx}{display} {sym_from} → {out_display}",
                pfx = from_token.prefix,
                display = pending.amount_display,
                sym_from = from_token.symbol,
            );

            bot.edit_message_text(chat_id, message_id, text)
                .parse_mode(HTML)
                .reply_markup(InlineKeyboardMarkup::new(vec![vec![super::back_button()]]))
                .await
                .ok();

            tracing::info!(
                user_id,
                from = from_token.symbol.as_str(),
                to = to_token.symbol.as_str(),
                amount = pending.amount_raw.as_str(),
                "swap complete"
            );
        }
        Err(e) => {
            tracing::error!(user_id, "swap: {e}");
            bot.edit_message_text(chat_id, message_id, "❌ Swap failed. Try again later.")
                .reply_markup(InlineKeyboardMarkup::new(vec![vec![super::back_button()]]))
                .await
                .ok();
        }
    }
}
