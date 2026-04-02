pub mod callback;
pub mod start;
pub mod tip;
pub mod withdraw;

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

use crate::outlayer::format_amount;

// ── Token config ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct TokenConfig {
    pub contract: String,    // NEP-141 contract ID
    pub symbol: String,      // "NEAR" or "USDC"
    pub decimals: u32,       // 24 for wNEAR, 6 for USDC
    pub display_dp: u32,     // display decimal places (4 for NEAR, 2 for USDC)
    pub dust: u128,          // dust threshold for adjust_for_dust
    pub prefix: &'static str, // "$" for USDC, "" for NEAR
}

// ── Conversation state ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum ConvoState {
    WithdrawAddress { token_key: String },
    WithdrawAmount { token_key: String, address: String, chain: String },
}

#[derive(Clone, Debug)]
pub struct PendingWithdrawal {
    pub token_key: String,
    pub address: String,
    pub chain: String,
    pub amount_raw: String,
    pub amount_display: String,
}

// ── View builders ──────────────────────────────────────────────────

pub async fn menu_view(state: &crate::AppState, user_id: u64) -> (String, InlineKeyboardMarkup) {
    let _ = state.outlayer.register_wallet(user_id).await;

    let near_bal = state
        .outlayer
        .get_balance(user_id, &state.near_token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let usdc_bal = state
        .outlayer
        .get_balance(user_id, &state.usdc_token.contract)
        .await
        .unwrap_or_else(|_| "0".into());

    let near_fmt = format_amount(&near_bal, state.near_token.decimals, state.near_token.display_dp);
    let usdc_fmt = format_amount(&usdc_bal, state.usdc_token.decimals, state.usdc_token.display_dp);

    let text = format!(
        "<b>NEAR Tip Bot</b>\n\n\
         NEAR: <b>{near_fmt}</b>\n\
         USDC: <b>${usdc_fmt}</b>\n\n\
         In groups:\n\
         <code>/near 1.00</code> — tip NEAR\n\
         <code>/usd 1.00</code> — tip USDC"
    );

    (text, main_menu_keyboard())
}

pub async fn balance_view(
    state: &crate::AppState,
    user_id: u64,
) -> (String, InlineKeyboardMarkup) {
    let near_bal = state
        .outlayer
        .get_balance(user_id, &state.near_token.contract)
        .await
        .unwrap_or_else(|_| "0".into());
    let usdc_bal = state
        .outlayer
        .get_balance(user_id, &state.usdc_token.contract)
        .await
        .unwrap_or_else(|_| "0".into());

    let near_fmt = format_amount(&near_bal, state.near_token.decimals, state.near_token.display_dp);
    let usdc_fmt = format_amount(&usdc_bal, state.usdc_token.decimals, state.usdc_token.display_dp);

    let text = format!(
        "<b>💰 Balance</b>\n\n\
         NEAR: <b>{near_fmt}</b>\n\
         USDC: <b>${usdc_fmt}</b>"
    );

    let kb = InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("🔄 Refresh", "cb:balance:refresh")],
        vec![back_button()],
    ]);

    (text, kb)
}

pub async fn deposit_view(
    state: &crate::AppState,
    user_id: u64,
) -> (String, InlineKeyboardMarkup) {
    let _ = state.outlayer.register_wallet(user_id).await;

    let addr = state
        .outlayer
        .get_address(user_id, "near")
        .await
        .unwrap_or_else(|_| "error".into());

    let mut text = format!(
        "<b>📥 Deposit</b>\n\n\
         <b>USDC</b> — send directly to intents address:\n\
         <code>{addr}</code>\n\
         Network: NEAR (intents)\n"
    );

    if !state.deposit_contract.is_empty() {
        text.push_str(&format!(
            "\n<b>NEAR</b> — via deposit helper:\n\
             <code>near call {contract} deposit '{{\"{msg_key}\":\"{addr}\"}}' \\\n  \
             --accountId YOUR.near --deposit AMOUNT</code>",
            contract = state.deposit_contract,
            msg_key = "msg",
        ));
    }

    let kb = InlineKeyboardMarkup::new(vec![vec![back_button()]]);

    (text, kb)
}

// ── Keyboards ──────────────────────────────────────────────────────

pub fn main_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("💰 Balance", "cb:balance"),
            InlineKeyboardButton::callback("📥 Deposit", "cb:deposit"),
        ],
        vec![
            InlineKeyboardButton::callback("📤 Withdraw NEAR", "cb:withdraw:near"),
            InlineKeyboardButton::callback("📤 Withdraw USDC", "cb:withdraw:usdc"),
        ],
    ])
}

pub fn back_button() -> InlineKeyboardButton {
    InlineKeyboardButton::callback("← Menu", "cb:menu")
}

pub fn cancel_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "❌ Cancel",
        "cb:menu",
    )]])
}

pub const HTML: ParseMode = ParseMode::Html;
