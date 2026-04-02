pub mod callback;
pub mod start;
pub mod swap;
pub mod tip;
pub mod withdraw;

use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

use crate::outlayer::format_amount;

// ── Token config ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct TokenConfig {
    pub contract: String,
    pub symbol: String,
    pub decimals: u32,
    pub display_dp: u32,
    pub dust: u128,
    pub prefix: &'static str,
}

// ── Conversation state ─────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum ConvoState {
    WithdrawAddress { token_key: String },
    WithdrawAmount { token_key: String, address: String, chain: String },
    SwapAmount { from_key: String, to_key: String },
    DepositAmount { token_key: String },
}

#[derive(Clone, Debug)]
pub struct PendingWithdrawal {
    pub token_key: String,
    pub address: String,
    pub chain: String,
    pub amount_raw: String,
    pub amount_display: String,
}

#[derive(Clone, Debug)]
pub struct PendingSwap {
    pub from_key: String,
    pub to_key: String,
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

/// Deposit: choose token
pub fn deposit_choose_token() -> (String, InlineKeyboardMarkup) {
    let text = "<b>📥 Deposit</b>\n\nChoose token:".to_string();
    let kb = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("NEAR", "dep:near"),
            InlineKeyboardButton::callback("USDC", "dep:usdc"),
        ],
        vec![back_button()],
    ]);
    (text, kb)
}

/// Deposit: choose amount (preset buttons + custom)
pub fn deposit_choose_amount(token_key: &str) -> (String, InlineKeyboardMarkup) {
    let symbol = if token_key == "near" { "NEAR" } else { "USDC" };
    let text = format!(
        "<b>📥 Deposit {symbol}</b>\n\nChoose amount or enter a custom one:"
    );
    let presets = ["0.5", "1", "5", "10"];
    let kb = InlineKeyboardMarkup::new(vec![
        presets
            .iter()
            .map(|a| InlineKeyboardButton::callback(*a, format!("dep:amt:{token_key}:{a}")))
            .collect(),
        vec![InlineKeyboardButton::callback("Custom amount", format!("dep:custom:{token_key}"))],
        vec![InlineKeyboardButton::callback("← Back", "cb:deposit")],
    ]);
    (text, kb)
}

/// Build a fund URL for a given token and amount.
pub fn fund_url(state: &crate::AppState, addr: &str, token_key: &str, amount: &str) -> String {
    let base = &state.fund_base_url;
    if token_key == "near" {
        let args_json = format!(r#"{{"msg":"{addr}"}}"#);
        let args = urlencoding::encode(&args_json);
        format!(
            "{base}?to={addr}&amount={amount}&token=near&via={}&method=deposit&args={args}&gas=100",
            state.deposit_contract
        )
    } else {
        format!(
            "{base}?to={addr}&amount={amount}&token={}&dest=intents",
            state.usdc_token.contract
        )
    }
}

// ── Keyboards ──────────────────────────────────────────────────────

pub fn main_menu_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("💰 Balance", "cb:balance"),
            InlineKeyboardButton::callback("📥 Deposit", "cb:deposit"),
        ],
        vec![InlineKeyboardButton::callback("🔄 Swap", "cb:swap")],
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
