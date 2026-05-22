use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{
    CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage,
};

use super::{balance_view, cancel_keyboard, deposit_choose_amount, deposit_choose_token, fund_url, menu_view, ConvoState, HTML};
use crate::outlayer::{adjust_for_dust, format_amount};
use crate::AppState;

pub async fn handle(bot: Bot, q: CallbackQuery, state: Arc<AppState>) -> ResponseResult<()> {
    bot.answer_callback_query(&q.id).await?;

    let data = q.data.as_deref().unwrap_or("");

    let (chat_id, message_id) = match &q.message {
        Some(MaybeInaccessibleMessage::Regular(msg)) => (msg.chat.id, msg.id),
        _ => return Ok(()),
    };

    let user_id = q.from.id.0;

    match data {
        // ── Menu ───────────────────────────────────────────────
        "cb:menu" => {
            state.conversations.remove(&user_id);
            state.pending_withdrawals.remove(&user_id);
            state.pending_swaps.remove(&user_id);
            let (text, kb) = menu_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        "cb:balance" | "cb:balance:refresh" => {
            let (text, kb) = balance_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        "cb:deposit" => {
            let (text, kb) = deposit_choose_token();
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        "dep:near" | "dep:usdc" => {
            let token_key = if data == "dep:near" { "near" } else { "usdc" };
            let (text, kb) = deposit_choose_amount(token_key);
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        _ if data.starts_with("dep:amt:") => {
            // dep:amt:{token_key}:{amount}
            let parts: Vec<&str> = data.splitn(4, ':').collect();
            if parts.len() == 4 {
                let token_key = parts[2];
                let amount = parts[3];
                let _ = state.outlayer.register_wallet(user_id).await;
                let addr = match state.outlayer.get_address(user_id, "near").await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::error!(user = user_id, "get_address(near): {e}");
                        "error".into()
                    }
                };
                let url = fund_url(&state, &addr, token_key, amount);
                let symbol = state.token_by_key(token_key).symbol.clone();
                let kb = InlineKeyboardMarkup::new(vec![
                    vec![InlineKeyboardButton::url(
                        format!("Open wallet — {amount} {symbol}"),
                        url.parse().unwrap(),
                    )],
                    vec![InlineKeyboardButton::callback("← Back", if token_key == "near" { "dep:near" } else { "dep:usdc" })],
                ]);
                edit(
                    &bot, chat_id, message_id,
                    &format!(
                        "<b>📥 Deposit {amount} {symbol}</b>\n\n\
                         Tap the button below to open your wallet and send {amount} {symbol}."
                    ),
                    kb,
                ).await;
            }
        }

        _ if data.starts_with("dep:custom:") => {
            let token_key = data.strip_prefix("dep:custom:").unwrap_or("near");
            let symbol = state.token_by_key(token_key).symbol.clone();
            state.conversations.insert(user_id, ConvoState::DepositAmount { token_key: token_key.to_string() });
            edit(
                &bot, chat_id, message_id,
                &format!("<b>📥 Deposit {symbol}</b>\n\nEnter the amount:"),
                cancel_keyboard(),
            ).await;
        }

        // ── Swap ───────────────────────────────────────────────
        "cb:swap" => {
            let kb = InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("NEAR → USDC", "cb:swap:near_usdc"),
                    InlineKeyboardButton::callback("USDC → NEAR", "cb:swap:usdc_near"),
                ],
                vec![super::back_button()],
            ]);
            edit(
                &bot,
                chat_id,
                message_id,
                "<b>🔄 Swap</b>\n\nChoose direction:",
                kb,
            )
            .await;
        }

        "cb:swap:near_usdc" | "cb:swap:usdc_near" => {
            let (from_key, to_key) = if data == "cb:swap:near_usdc" {
                ("near", "usdc")
            } else {
                ("usdc", "near")
            };

            let from_token = state.token_by_key(from_key);
            let balance = state
                .outlayer
                .get_balance(user_id, &from_token.contract)
                .await
                .unwrap_or_else(|_| "0".into());
            let bal_fmt =
                format_amount(&balance, from_token.decimals, from_token.display_dp);

            state.conversations.insert(
                user_id,
                ConvoState::SwapAmount {
                    from_key: from_key.into(),
                    to_key: to_key.into(),
                },
            );

            let kb = InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback(
                    format!("MAX ({bal_fmt})"),
                    "s:max",
                )],
                vec![InlineKeyboardButton::callback("❌ Cancel", "cb:menu")],
            ]);

            let to_token = state.token_by_key(to_key);
            edit(
                &bot,
                chat_id,
                message_id,
                &format!(
                    "<b>🔄 Swap {from} → {to}</b>\n\n\
                     Available: {pfx}{bal_fmt} {from}\n\n\
                     Enter the amount to swap:",
                    from = from_token.symbol,
                    to = to_token.symbol,
                    pfx = from_token.prefix,
                ),
                kb,
            )
            .await;
        }

        "s:max" => {
            super::swap::handle_max_callback(&bot, &state, chat_id, message_id, user_id)
                .await
                .ok();
        }

        "s:ok" => {
            if let Some((_, pending)) = state.pending_swaps.remove(&user_id) {
                super::swap::execute(&bot, &state, chat_id, message_id, user_id, &pending).await;
            }
        }

        "s:no" => {
            state.conversations.remove(&user_id);
            state.pending_swaps.remove(&user_id);
            let (text, kb) = menu_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        // ── Withdraw ───────────────────────────────────────────
        "cb:withdraw:near" | "cb:withdraw:usdc" => {
            let token_key = if data == "cb:withdraw:near" {
                "near"
            } else {
                "usdc"
            };
            let token = state.token_by_key(token_key);

            state.conversations.insert(
                user_id,
                ConvoState::WithdrawAddress {
                    token_key: token_key.to_string(),
                },
            );

            edit(
                &bot,
                chat_id,
                message_id,
                &format!(
                    "<b>📤 Withdraw {}</b>\n\nEnter the recipient address:",
                    token.symbol
                ),
                cancel_keyboard(),
            )
            .await;
        }

        "w:max" => {
            super::withdraw::handle_max_callback(&bot, &state, chat_id, message_id, user_id)
                .await
                .ok();
        }

        "w:ok" => {
            if let Some((_, pending)) = state.pending_withdrawals.remove(&user_id) {
                execute_withdrawal(&bot, &state, chat_id, message_id, user_id, &pending).await;
            }
        }

        "w:no" => {
            state.conversations.remove(&user_id);
            state.pending_withdrawals.remove(&user_id);
            let (text, kb) = menu_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        #[cfg(feature = "dice")]
        _ if data.starts_with("dice:refresh:") => {
            if let Some(game_id_str) = data.strip_prefix("dice:refresh:") {
                if let Ok(game_id) = game_id_str.parse::<u64>() {
                    crate::extensions::dice::handle_refresh(&bot, &state, chat_id.0, message_id.0, game_id).await;
                }
            }
        }

        _ => {}
    }

    Ok(())
}

async fn edit(
    bot: &Bot,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
    text: &str,
    kb: InlineKeyboardMarkup,
) {
    bot.edit_message_text(chat_id, message_id, text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .await
        .ok();
}

async fn execute_withdrawal(
    bot: &Bot,
    state: &AppState,
    chat_id: ChatId,
    message_id: teloxide::types::MessageId,
    user_id: u64,
    pending: &super::PendingWithdrawal,
) {
    edit(
        bot,
        chat_id,
        message_id,
        "⏳ Processing withdrawal...",
        InlineKeyboardMarkup::default(),
    )
    .await;

    let token = state.token_by_key(&pending.token_key);

    let balance_str = state
        .outlayer
        .get_balance(user_id, &token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let balance: u128 = balance_str.parse().unwrap_or(0);
    let amount: u128 = pending.amount_raw.parse().unwrap_or(0);
    let amount = adjust_for_dust(balance, amount, token.dust);

    if balance < amount {
        let bal_fmt = format_amount(&balance.to_string(), token.decimals, token.display_dp);
        edit(
            bot,
            chat_id,
            message_id,
            &format!(
                "❌ Insufficient balance: {}{bal_fmt} {}",
                token.prefix, token.symbol
            ),
            InlineKeyboardMarkup::new(vec![vec![super::back_button()]]),
        )
        .await;
        return;
    }

    match state
        .outlayer
        .withdraw(
            user_id,
            &token.contract,
            &amount.to_string(),
            &pending.chain,
            &pending.address,
        )
        .await
    {
        Ok(_) => {
            edit(
                bot,
                chat_id,
                message_id,
                &format!(
                    "<b>✅ Withdrawal sent</b>\n\n\
                     Amount: <b>{pfx}{display}</b> {sym}\n\
                     To: <code>{addr}</code>\n\
                     Network: {chain}\n\n\
                     Withdrawals are processed via NEAR Intents.\n\
                     Track at <a href=\"https://near-intents.org/account\">near-intents.org</a>",
                    pfx = token.prefix,
                    display = pending.amount_display,
                    sym = token.symbol,
                    addr = pending.address,
                    chain = pending.chain,
                ),
                InlineKeyboardMarkup::new(vec![vec![super::back_button()]]),
            )
            .await;

            tracing::info!(
                user_id,
                amount = pending.amount_raw.as_str(),
                chain = pending.chain.as_str(),
                address = pending.address.as_str(),
                symbol = token.symbol.as_str(),
                "withdrawal sent"
            );
        }
        Err(e) => {
            tracing::error!(user_id, "withdraw: {e}");
            edit(
                bot,
                chat_id,
                message_id,
                "❌ Withdrawal failed. Try again later.",
                InlineKeyboardMarkup::new(vec![vec![super::back_button()]]),
            )
            .await;
        }
    }
}
