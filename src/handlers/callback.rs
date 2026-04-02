use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, InlineKeyboardMarkup, MaybeInaccessibleMessage};

use super::{balance_view, cancel_keyboard, deposit_view, menu_view, ConvoState, HTML};
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
        "cb:menu" => {
            state.conversations.remove(&user_id);
            state.pending_withdrawals.remove(&user_id);
            let (text, kb) = menu_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        "cb:balance" | "cb:balance:refresh" => {
            let (text, kb) = balance_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

        "cb:deposit" => {
            let (text, kb) = deposit_view(&state, user_id).await;
            edit(&bot, chat_id, message_id, &text, kb).await;
        }

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
            &format!("❌ Insufficient balance: {}{bal_fmt} {}", token.prefix, token.symbol),
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
            let text = format!(
                "<b>✅ Withdrawal sent</b>\n\n\
                 Amount: <b>{pfx}{display}</b> {sym}\n\
                 To: <code>{addr}</code>\n\
                 Network: {chain}",
                pfx = token.prefix,
                display = pending.amount_display,
                sym = token.symbol,
                addr = pending.address,
                chain = pending.chain,
            );
            edit(
                bot,
                chat_id,
                message_id,
                &text,
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
