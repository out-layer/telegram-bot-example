use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ReplyParameters};

use super::{TokenConfig, HTML};
use crate::outlayer::{format_amount, parse_amount};
use crate::AppState;

// ── Types ─────────────────────────────────────────────────────────

pub type GameId = u64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum GamePhase {
    Betting,
    Rolling,
    Finished,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GamePlayer {
    pub user_id: u64,
    pub display_name: String,
    pub stake_raw: String,
    pub check_id: String,
    pub check_key: String,
    pub dice_value: Option<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiceGame {
    pub game_id: GameId,
    pub chat_id: i64,
    pub message_id: i32,
    pub token_key: String,
    pub min_stake_raw: String,
    pub phase: GamePhase,
    pub players: Vec<GamePlayer>,
    pub created_at: u64,
    pub betting_deadline: u64,
    pub rolling_deadline: Option<u64>,
    #[serde(default)]
    pub demo: bool,
}

#[derive(Serialize, Deserialize)]
pub struct DiceGameStore {
    pub games: Vec<DiceGame>,
    pub next_id: GameId,
}

impl Default for DiceGameStore {
    fn default() -> Self {
        Self {
            games: Vec::new(),
            next_id: 1,
        }
    }
}

// ── Persistence ───────────────────────────────────────────────────

pub fn load_games(path: &str) -> DiceGameStore {
    match std::fs::read_to_string(path) {
        Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
        Err(_) => DiceGameStore::default(),
    }
}

pub fn persist_games(state: &AppState) {
    let games: Vec<DiceGame> = state
        .dice_games
        .iter()
        .filter(|r| r.phase == GamePhase::Betting || r.phase == GamePhase::Rolling)
        .map(|r| r.value().clone())
        .collect();
    let store = DiceGameStore {
        games,
        next_id: state.dice_next_id.load(Ordering::Relaxed),
    };
    let path = &state.dice_games_file;
    let tmp = format!("{path}.tmp");
    if let Ok(data) = serde_json::to_string_pretty(&store) {
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

pub fn restore_games(state: &Arc<AppState>, bot: &Bot) {
    let store = load_games(&state.dice_games_file);
    state.dice_next_id.store(store.next_id, Ordering::Relaxed);
    let now = now_ts();

    for game in store.games {
        // Only restore active games
        if game.phase != GamePhase::Betting && game.phase != GamePhase::Rolling {
            continue;
        }

        let game_id = game.game_id;
        let chat_id = game.chat_id;
        let msg_id = game.message_id;

        // Rebuild indexes
        state.dice_msg_index.insert((chat_id, msg_id), game_id);
        if game.phase == GamePhase::Rolling {
            for p in &game.players {
                state.dice_player_index.insert((chat_id, p.user_id), game_id);
            }
        }

        let phase = game.phase.clone();
        state.dice_games.insert(game_id, game);

        // Re-arm timers
        match phase {
            GamePhase::Betting => {
                let deadline = state.dice_games.get(&game_id).map(|g| g.betting_deadline).unwrap_or(0);
                spawn_betting_timer(bot.clone(), state.clone(), game_id, deadline, now);
            }
            GamePhase::Rolling => {
                let deadline = state.dice_games.get(&game_id).and_then(|g| g.rolling_deadline).unwrap_or(0);
                spawn_rolling_timer(bot.clone(), state.clone(), game_id, deadline, now);
            }
            _ => {}
        }
    }

    let count = state.dice_games.len();
    if count > 0 {
        tracing::info!("Restored {count} active dice game(s)");
    }
}

// ── Helpers ───────────────────────────────────────────────────────

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn display_name(user: &teloxide::types::User) -> String {
    user.username
        .as_ref()
        .map(|u| format!("@{u}"))
        .unwrap_or_else(|| escape_html(&user.first_name))
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn game_token<'a>(state: &'a AppState, token_key: &str) -> &'a TokenConfig {
    state.token_by_key(token_key)
}

fn stake_limits(state: &AppState, token_key: &str) -> (u128, u128) {
    if token_key == "near" {
        (state.dice_min_near, state.dice_max_near)
    } else {
        (state.dice_min_usdc, state.dice_max_usdc)
    }
}

fn format_duration(secs: u64) -> String {
    let mins = secs / 60;
    let s = secs % 60;
    if mins > 0 && s > 0 {
        format!("{mins} min {s} sec")
    } else if mins > 0 {
        if mins == 1 { "1 minute".into() } else { format!("{mins} minutes") }
    } else {
        format!("{s} sec")
    }
}

fn demo_tag(game: &DiceGame) -> &'static str {
    if game.demo { " [DEMO]" } else { "" }
}

async fn claim_with_retry(state: &AppState, user_id: u64, check_key: &str, game_id: GameId) {
    for attempt in 0..3u32 {
        match state.outlayer.claim_payment_check(user_id, check_key).await {
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(game_id, attempt, "claim check: {e}");
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

async fn edit_game_msg(bot: &Bot, chat_id: i64, msg_id: i32, text: &str, kb: InlineKeyboardMarkup) {
    let _ = bot
        .edit_message_text(ChatId(chat_id), teloxide::types::MessageId(msg_id), text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .await;
}

macro_rules! reply {
    ($bot:expr, $msg:expr, $text:expr) => {
        $bot.send_message($msg.chat.id, $text)
            .reply_parameters(ReplyParameters::new($msg.id))
            .await?
    };
}

// ── Message formatting ────────────────────────────────────────────

fn dice_refresh_kb(game_id: GameId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("🔄 Refresh", format!("dice:refresh:{game_id}")),
    ]])
}

/// Trim trailing zeros: "1.0000" → "1", "1.5000" → "1.5"
fn trim_zeros(s: &str) -> &str {
    if s.contains('.') {
        let trimmed = s.trim_end_matches('0').trim_end_matches('.');
        if trimmed.is_empty() { "0" } else { trimmed }
    } else {
        s
    }
}

fn betting_message(game: &DiceGame, token: &TokenConfig, remaining_secs: u64) -> (String, InlineKeyboardMarkup) {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let stake_short = trim_zeros(&stake_display);
    let players_list: Vec<&str> = game.players.iter().map(|p| p.display_name.as_str()).collect();
    let mins = remaining_secs / 60;
    let secs = remaining_secs % 60;
    let cmd = token.symbol.to_lowercase();
    let dt = demo_tag(game);
    let text = format!(
        "🎲 <b>Dice Game{dt} — {prefix}{stake_short} {symbol}</b>\n\n\
         Bet: {prefix}{stake_short} {symbol}\n\
         Players ({count}): {players}\n\n\
         ⏳ Waiting for players... ({mins}:{secs:02})\n\
         Reply <code>/{cmd} {stake_short}</code> to join!",
        prefix = token.prefix,
        symbol = token.symbol,
        count = game.players.len(),
        players = players_list.join(", "),
    );
    (text, dice_refresh_kb(game.game_id))
}

fn rolling_message(game: &DiceGame, token: &TokenConfig, remaining_secs: u64) -> (String, InlineKeyboardMarkup) {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let stake_display = trim_zeros(&stake_display);
    let mins = remaining_secs / 60;
    let secs = remaining_secs % 60;
    let dt = demo_tag(game);

    let mut lines = Vec::new();
    for p in &game.players {
        let status = match p.dice_value {
            Some(v) => format!("🎲 {v}"),
            None => "⏳".to_string(),
        };
        lines.push(format!("{} — {status}", p.display_name));
    }

    let text = format!(
        "🎲 <b>Dice Game{dt} — Roll!</b>\n\
         Bet: {prefix}{stake_display} {symbol}\n\n\
         {players}\n\n\
         Send 🎲 to roll! ({mins}:{secs:02})",
        prefix = token.prefix,
        symbol = token.symbol,
        players = lines.join("\n"),
    );
    (text, dice_refresh_kb(game.game_id))
}

fn results_message(game: &DiceGame, token: &TokenConfig, winners: &[&GamePlayer], prize_each: &str, fee_pct: u8) -> String {
    let stake_fmt = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let stake_display = trim_zeros(&stake_fmt);
    let prize_fmt = format_amount(prize_each, token.decimals, token.display_dp);
    let prize_display = trim_zeros(&prize_fmt);
    let dt = demo_tag(game);

    let mut lines = Vec::new();
    for p in &game.players {
        let val = p.dice_value.unwrap_or(0);
        let is_winner = winners.iter().any(|w| w.user_id == p.user_id);
        let trophy = if is_winner { " 🏆" } else { "" };
        lines.push(format!("{} — 🎲 {val}{trophy}", p.display_name));
    }

    let winner_text = if winners.len() == 1 {
        format!(
            "Winner: {} wins {prefix}{prize_display} {symbol}!",
            winners[0].display_name,
            prefix = token.prefix,
            symbol = token.symbol,
        )
    } else {
        let names: Vec<&str> = winners.iter().map(|w| w.display_name.as_str()).collect();
        format!(
            "Winners: {} split {prefix}{prize_display} {symbol} each!",
            names.join(", "),
            prefix = token.prefix,
            symbol = token.symbol,
        )
    };

    let fee_line = if fee_pct > 0 {
        format!("\nFee: {fee_pct}%")
    } else {
        String::new()
    };

    format!(
        "🎲 <b>Dice Game{dt} — Results!</b>\n\
         Bet: {prefix}{stake_display} {symbol}{fee_line}\n\n\
         {players}\n\n\
         {winner_text}",
        prefix = token.prefix,
        symbol = token.symbol,
        players = lines.join("\n"),
    )
}

fn cancelled_message(game: &DiceGame, token: &TokenConfig) -> String {
    let stake_fmt = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let stake_display = trim_zeros(&stake_fmt);
    let creator = &game.players[0].display_name;
    let dt = demo_tag(game);
    format!(
        "🎲 <b>Dice Game{dt} — Cancelled</b>\n\n\
         No one joined. {prefix}{stake_display} {symbol} returned to {creator}.",
        prefix = token.prefix,
        symbol = token.symbol,
    )
}

fn all_zero_message(game: &DiceGame, token: &TokenConfig) -> String {
    let stake_fmt = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let stake_display = trim_zeros(&stake_fmt);
    let dt = demo_tag(game);
    format!(
        "🎲 <b>Dice Game{dt} — No Winners</b>\n\
         Bet: {prefix}{stake_display} {symbol}\n\n\
         Nobody rolled! All stakes refunded.",
        prefix = token.prefix,
        symbol = token.symbol,
    )
}

// ── Start game ────────────────────────────────────────────────────

pub async fn start_game(
    bot: Bot,
    msg: Message,
    state: Arc<AppState>,
    args: String,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id.0;

    // Validate chat whitelist
    if !state.dice_allowed_chats.contains(&chat_id) {
        reply!(bot, msg, "🎲 Dice game is not enabled in this chat.");
        return Ok(());
    }

    // Only in groups
    if msg.chat.is_private() {
        reply!(bot, msg, "🎲 Dice game can only be played in group chats.");
        return Ok(());
    }

    // One active game per chat
    for entry in state.dice_games.iter() {
        let g = entry.value();
        if g.chat_id == chat_id && (g.phase == GamePhase::Betting || g.phase == GamePhase::Rolling) {
            reply!(bot, msg, "🎲 A game is already active in this chat. Wait for it to finish.");
            return Ok(());
        }
    }

    // Parse args: "near 1" or "usd 5"
    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    if parts.len() != 2 {
        reply!(bot, msg, "Usage: /dice near 1 or /dice usd 5");
        return Ok(());
    }

    let token_key = match parts[0].to_lowercase().as_str() {
        "near" => "near",
        "usd" | "usdc" => "usdc",
        _ => {
            reply!(bot, msg, "Supported tokens: near, usd\nExample: /dice near 1");
            return Ok(());
        }
    };
    let token = game_token(&state, token_key);

    let amount_raw = match parse_amount(parts[1], token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            reply!(bot, msg, format!("Invalid amount. Example: /dice {token_key} 1"));
            return Ok(());
        }
    };

    // Validate min/max
    let (min_stake, max_stake) = stake_limits(&state, token_key);
    if amount_raw < min_stake {
        let min_display = format_amount(&min_stake.to_string(), token.decimals, token.display_dp);
        reply!(bot, msg, format!("Minimum stake: {}{min_display} {}", token.prefix, token.symbol));
        return Ok(());
    }
    if amount_raw > max_stake {
        let max_display = format_amount(&max_stake.to_string(), token.decimals, token.display_dp);
        reply!(bot, msg, format!("Maximum stake: {}{max_display} {}", token.prefix, token.symbol));
        return Ok(());
    }

    let sender = match &msg.from {
        Some(u) => u,
        None => return Ok(()),
    };

    let demo = state.dice_demo;
    let amount_str = amount_raw.to_string();
    let game_id = state.dice_next_id.fetch_add(1, Ordering::Relaxed);

    let (check_id, check_key) = if demo {
        (String::new(), String::new())
    } else {
        // Register wallet & check balance
        if let Err(e) = state.outlayer.register_wallet(sender.id.0).await {
            tracing::error!(user = sender.id.0, "register wallet: {e}");
            reply!(bot, msg, "Failed to set up wallet.");
            return Ok(());
        }

        let balance_str = match state.outlayer.get_balance(sender.id.0, &token.contract).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(user = sender.id.0, "get_balance: {e}");
                reply!(bot, msg, "Failed to check balance.");
                return Ok(());
            }
        };
        let balance: u128 = balance_str.parse().unwrap_or(0);
        if balance < amount_raw {
            let bal_fmt = format_amount(&balance.to_string(), token.decimals, token.display_dp);
            reply!(
                bot,
                msg,
                format!("Insufficient balance: {}{bal_fmt} {}", token.prefix, token.symbol)
            );
            return Ok(());
        }

        // Create payment check (escrow)
        match state
            .outlayer
            .create_payment_check(sender.id.0, &token.contract, &amount_str, &format!("dice:{game_id}"))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(user = sender.id.0, "create_payment_check: {e}");
                reply!(bot, msg, "Failed to lock stake. Try again.");
                return Ok(());
            }
        }
    };

    let now = now_ts();
    let betting_deadline = now + state.dice_betting_timeout;

    let game = DiceGame {
        game_id,
        chat_id,
        message_id: 0, // will be set after sending
        token_key: token_key.to_string(),
        min_stake_raw: amount_str.clone(),
        phase: GamePhase::Betting,
        players: vec![GamePlayer {
            user_id: sender.id.0,
            display_name: display_name(sender),
            stake_raw: amount_str,
            check_id,
            check_key,
            dice_value: None,
        }],
        created_at: now,
        betting_deadline,
        rolling_deadline: None,
        demo,
    };

    let (text, kb) = betting_message(&game, token, state.dice_betting_timeout);
    let sent = bot.send_message(msg.chat.id, &text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .reply_parameters(ReplyParameters::new(msg.id))
        .await?;

    let msg_id = sent.id.0;
    let mut game = game;
    game.message_id = msg_id;

    state.dice_msg_index.insert((chat_id, msg_id), game_id);
    state.dice_games.insert(game_id, game);
    persist_games(&state);

    spawn_betting_timer(bot, state, game_id, betting_deadline, now);

    tracing::info!(game_id, chat_id, "dice game started");
    Ok(())
}

// ── Join game ─────────────────────────────────────────────────────

pub async fn join_game(
    bot: Bot,
    msg: Message,
    state: Arc<AppState>,
    args: String,
    token: &TokenConfig,
) -> ResponseResult<()> {
    let reply = msg.reply_to_message().unwrap();
    let chat_id = msg.chat.id.0;
    let reply_msg_id = reply.id.0;

    let game_id = match state.dice_msg_index.get(&(chat_id, reply_msg_id)) {
        Some(id) => *id,
        None => return Ok(()),
    };

    let sender = match &msg.from {
        Some(u) => u,
        None => return Ok(()),
    };

    // Check token match (read-only, no race concern)
    let game_token_cfg = game_token(&state, &{
        let g = match state.dice_games.get(&game_id) {
            Some(g) => g,
            None => return Ok(()),
        };
        g.token_key.clone()
    });
    if token.contract != game_token_cfg.contract {
        let cmd = game_token_cfg.symbol.to_lowercase();
        reply!(
            bot,
            msg,
            format!(
                "🎲 This game is for {}. Use /{cmd} or /swap in DM first.",
                game_token_cfg.symbol
            )
        );
        return Ok(());
    }

    // Parse amount (before taking the lock)
    let amount_raw = match parse_amount(args.trim(), token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            let g = match state.dice_games.get(&game_id) {
                Some(g) => g,
                None => return Ok(()),
            };
            let stake_display = format_amount(&g.min_stake_raw, token.decimals, token.display_dp);
            reply!(
                bot,
                msg,
                format!("Invalid amount. Reply with /{} {stake_display}", token.symbol.to_lowercase())
            );
            return Ok(());
        }
    };

    // Read min_stake and demo flag
    let (min_stake, is_demo) = {
        let g = match state.dice_games.get(&game_id) {
            Some(g) => g,
            None => return Ok(()),
        };
        (g.min_stake_raw.parse::<u128>().unwrap_or(0), g.demo)
    };

    if amount_raw < min_stake {
        let stake_display = format_amount(&min_stake.to_string(), token.decimals, token.display_dp);
        reply!(
            bot,
            msg,
            format!("🎲 Minimum bet is {}{stake_display} {}", token.prefix, token.symbol)
        );
        return Ok(());
    }

    let stake_str = min_stake.to_string();
    let (check_id, check_key) = if is_demo {
        (String::new(), String::new())
    } else {
        // Register & check balance
        if let Err(e) = state.outlayer.register_wallet(sender.id.0).await {
            tracing::error!(user = sender.id.0, "register wallet: {e}");
            reply!(bot, msg, "Failed to set up wallet.");
            return Ok(());
        }

        let balance_str = match state.outlayer.get_balance(sender.id.0, &token.contract).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(user = sender.id.0, "get_balance: {e}");
                reply!(bot, msg, "Failed to check balance.");
                return Ok(());
            }
        };
        let balance: u128 = balance_str.parse().unwrap_or(0);
        if balance < min_stake {
            let bal_fmt = format_amount(&balance.to_string(), token.decimals, token.display_dp);
            reply!(
                bot,
                msg,
                format!("Insufficient balance: {}{bal_fmt} {}", token.prefix, token.symbol)
            );
            return Ok(());
        }

        match state
            .outlayer
            .create_payment_check(sender.id.0, &token.contract, &stake_str, &format!("dice:{game_id}"))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(user = sender.id.0, "create_payment_check: {e}");
                reply!(bot, msg, "Failed to lock stake. Try again.");
                return Ok(());
            }
        }
    };

    // Atomically check phase + already joined + push player
    let (text, kb) = {
        let mut game = match state.dice_games.get_mut(&game_id) {
            Some(g) => g,
            None => return Ok(()),
        };

        if game.phase != GamePhase::Betting {
            drop(game);
            // Refund if we already created the check
            if !is_demo {
                let _ = state.outlayer.reclaim_payment_check(sender.id.0, &check_id).await;
            }
            reply!(bot, msg, "🎲 Betting phase is over.");
            return Ok(());
        }

        if game.players.iter().any(|p| p.user_id == sender.id.0) {
            drop(game);
            if !is_demo {
                let _ = state.outlayer.reclaim_payment_check(sender.id.0, &check_id).await;
            }
            reply!(bot, msg, "🎲 You're already in this game!");
            return Ok(());
        }

        game.players.push(GamePlayer {
            user_id: sender.id.0,
            display_name: display_name(sender),
            stake_raw: stake_str,
            check_id,
            check_key,
            dice_value: None,
        });

        let remaining = game.betting_deadline.saturating_sub(now_ts());
        betting_message(&game, game_token_cfg, remaining)
    }; // guard dropped

    edit_game_msg(&bot, chat_id, reply_msg_id, &text, kb).await;

    persist_games(&state);

    let join_msg = if amount_raw > min_stake {
        let stake_display = format_amount(&min_stake.to_string(), token.decimals, token.display_dp);
        let stake_short = trim_zeros(&stake_display);
        format!(
            "🎲 {} joined the game! Bet: {}{stake_short} {} (change returned)",
            display_name(sender), token.prefix, token.symbol,
        )
    } else {
        format!("🎲 {} joined the game!", display_name(sender))
    };
    reply!(bot, msg, join_msg);
    tracing::info!(game_id, user = sender.id.0, "player joined dice game");
    Ok(())
}

// ── Handle dice roll ──────────────────────────────────────────────

pub async fn handle_dice_roll(
    bot: Bot,
    msg: Message,
    state: Arc<AppState>,
    user_id: u64,
    chat_id: i64,
    dice_value: u8,
) -> ResponseResult<()> {
    let game_id = match state.dice_player_index.get(&(chat_id, user_id)) {
        Some(id) => *id,
        None => {
            // Check if there's an active rolling game in this chat — tell non-participant
            let has_rolling = state.dice_games.iter().any(|e| {
                e.value().chat_id == chat_id && e.value().phase == GamePhase::Rolling
            });
            if has_rolling {
                let _ = bot.send_message(ChatId(chat_id), "🎲 You're not in this game!")
                    .reply_parameters(ReplyParameters::new(msg.id))
                    .await;
            }
            return Ok(());
        }
    };

    // Mutate under the guard, compute text + should_resolve, then drop guard before I/O
    let (text, kb, should_resolve, game_msg_id, player_name) = {
        let mut game = match state.dice_games.get_mut(&game_id) {
            Some(g) => g,
            None => return Ok(()),
        };

        if game.phase != GamePhase::Rolling {
            return Ok(());
        }

        let player = match game.players.iter_mut().find(|p| p.user_id == user_id) {
            Some(p) => p,
            None => return Ok(()),
        };

        // Double-roll detection
        if player.dice_value.is_some() {
            let name = player.display_name.clone();
            drop(game);
            return handle_double_roll(&bot, chat_id, msg.id, &name).await;
        }

        player.dice_value = Some(dice_value);
        let pname = player.display_name.clone();

        let token = game_token(&state, &game.token_key);
        let remaining = game.rolling_deadline.unwrap_or(0).saturating_sub(now_ts());
        let (text, kb) = rolling_message(&game, token, remaining);
        let should_resolve = game.players.iter().all(|p| p.dice_value.is_some());
        let mid = game.message_id;

        (text, kb, should_resolve, mid, pname)
    }; // guard dropped — now safe to do I/O

    edit_game_msg(&bot, chat_id, game_msg_id, &text, kb).await;

    // Send "rolled dice" first, then reveal the value after animation finishes
    let roll_msg = bot.send_message(ChatId(chat_id), format!("🎲 {player_name} rolled dice..."))
        .reply_parameters(ReplyParameters::new(msg.id))
        .await;

    persist_games(&state);

    // Wait for dice animation to finish, then reveal
    tokio::time::sleep(Duration::from_secs(5)).await;
    if let Ok(sent) = roll_msg {
        let _ = bot.edit_message_text(
            ChatId(chat_id),
            sent.id,
            format!("🎲 {player_name} rolled: {dice_value}"),
        ).await;
    }

    if should_resolve {
        let _ = bot.send_message(ChatId(chat_id), "🎲 All players rolled! Calculating results...")
            .reply_parameters(ReplyParameters::new(msg.id))
            .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        resolve_game(&bot, &state, game_id).await;
    }

    Ok(())
}

async fn handle_double_roll(
    bot: &Bot,
    chat_id: i64,
    dice_msg_id: teloxide::types::MessageId,
    player_name: &str,
) -> ResponseResult<()> {
    let _ = bot.delete_message(ChatId(chat_id), dice_msg_id).await;
    // Can't reply to deleted message, so just send
    let _ = bot.send_message(
        ChatId(chat_id),
        format!("🎲 {player_name} already rolled! Only the first roll counts."),
    )
    .await;
    Ok(())
}

// ── Timers ────────────────────────────────────────────────────────

pub fn spawn_betting_timer(bot: Bot, state: Arc<AppState>, game_id: GameId, deadline: u64, now: u64) {
    let remaining = deadline.saturating_sub(now);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(remaining)).await;
        handle_betting_timeout(&bot, &state, game_id).await;
    });
}

pub fn spawn_rolling_timer(bot: Bot, state: Arc<AppState>, game_id: GameId, deadline: u64, now: u64) {
    let remaining = deadline.saturating_sub(now);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(remaining)).await;
        handle_rolling_timeout(&bot, &state, game_id).await;
    });
}

async fn handle_betting_timeout(bot: &Bot, state: &Arc<AppState>, game_id: GameId) {
    // Atomically read phase and transition — prevents join race
    let (cancelled, game_snapshot) = {
        let mut g = match state.dice_games.get_mut(&game_id) {
            Some(g) => g,
            None => return,
        };
        if g.phase != GamePhase::Betting {
            return;
        }

        if g.players.len() < 2 {
            g.phase = GamePhase::Cancelled;
            (true, g.clone())
        } else {
            let now = now_ts();
            let rolling_deadline = now + state.dice_rolling_timeout;
            g.phase = GamePhase::Rolling;
            g.rolling_deadline = Some(rolling_deadline);

            // Build player index for dice detection
            for p in &g.players {
                state.dice_player_index.insert((g.chat_id, p.user_id), game_id);
            }

            (false, g.clone())
        }
    }; // guard dropped

    let token = game_token(state, &game_snapshot.token_key);

    if cancelled {
        // Refund the single player
        if !game_snapshot.demo {
            let player = &game_snapshot.players[0];
            let _ = state
                .outlayer
                .reclaim_payment_check(player.user_id, &player.check_id)
                .await;
        }

        let text = cancelled_message(&game_snapshot, token);
        let _ = bot
            .edit_message_text(
                ChatId(game_snapshot.chat_id),
                teloxide::types::MessageId(game_snapshot.message_id),
                &text,
            )
            .parse_mode(HTML)
            .await;
        let _ = bot.send_message(ChatId(game_snapshot.chat_id), &text)
            .parse_mode(HTML)
            .await;

        cleanup_game(state, &game_snapshot);
        persist_games(state);
        tracing::info!(game_id, "dice game cancelled (no joiners)");
    } else {
        let rolling_deadline = game_snapshot.rolling_deadline.unwrap();

        let (text, kb) = rolling_message(&game_snapshot, token, state.dice_rolling_timeout);
        edit_game_msg(bot, game_snapshot.chat_id, game_snapshot.message_id, &text, kb).await;

        persist_games(state);

        let players_mention: Vec<&str> = game_snapshot.players.iter()
            .map(|p| p.display_name.as_str())
            .collect();
        let _ = bot
            .send_message(
                ChatId(game_snapshot.chat_id),
                format!(
                    "🎲 Bets are closed! Roll your dice!\n{}\nYou have {} to roll.",
                    players_mention.join(" "),
                    format_duration(state.dice_rolling_timeout),
                ),
            )
            .await;

        spawn_rolling_timer(bot.clone(), state.clone(), game_id, rolling_deadline, now_ts());

        tracing::info!(game_id, "dice game moved to rolling phase");
    }
}

async fn handle_rolling_timeout(bot: &Bot, state: &Arc<AppState>, game_id: GameId) {
    resolve_game(bot, state, game_id).await;
}

// ── Resolve game ──────────────────────────────────────────────────

async fn resolve_game(bot: &Bot, state: &Arc<AppState>, game_id: GameId) {
    // Atomically claim ownership by setting phase to Finished — prevents double resolution
    let game = {
        let mut g = match state.dice_games.get_mut(&game_id) {
            Some(g) => g,
            None => return,
        };
        if g.phase != GamePhase::Rolling {
            return;
        }
        g.phase = GamePhase::Finished;
        g.clone()
    }; // guard dropped

    let token = game_token(state, &game.token_key);

    // Find max dice value
    let max_val = game
        .players
        .iter()
        .map(|p| p.dice_value.unwrap_or(0))
        .max()
        .unwrap_or(0);

    if max_val == 0 {
        // All players scored 0 — refund everyone
        if !game.demo {
            for p in &game.players {
                let _ = state
                    .outlayer
                    .reclaim_payment_check(p.user_id, &p.check_id)
                    .await;
            }
        }

        let text = all_zero_message(&game, token);
        let _ = bot
            .edit_message_text(
                ChatId(game.chat_id),
                teloxide::types::MessageId(game.message_id),
                &text,
            )
            .parse_mode(HTML)
            .await;
        let _ = bot.send_message(ChatId(game.chat_id), &text)
            .parse_mode(HTML)
            .await;

        cleanup_game(state, &game);
        persist_games(state);
        tracing::info!(game_id, "dice game finished: all zero, refunded");
        return;
    }

    // Winners
    let winners: Vec<&GamePlayer> = game
        .players
        .iter()
        .filter(|p| p.dice_value.unwrap_or(0) == max_val)
        .collect();

    let total_pot: u128 = game.players.iter().map(|p| p.stake_raw.parse::<u128>().unwrap_or(0)).sum();
    let fee_pct = state.dice_fee;
    let fee_amount = if fee_pct > 0 && game.players.len() >= 2 {
        total_pot * fee_pct as u128 / 100
    } else {
        0
    };
    let distributable = total_pot - fee_amount;
    let prize_each = distributable / winners.len() as u128;
    let prize_each_str = prize_each.to_string();

    // Distribute funds
    if !game.demo {
        // Claim all checks to first winner (collects full pot)
        let collector = winners[0];
        for p in &game.players {
            claim_with_retry(state, collector.user_id, &p.check_key, game_id).await;
        }

        // Redistribute prize_each to other winners
        for &winner in &winners[1..] {
            match state
                .outlayer
                .create_payment_check(
                    collector.user_id,
                    &token.contract,
                    &prize_each_str,
                    &format!("dice:{game_id}:split"),
                )
                .await
            {
                Ok((_cid, ckey)) => {
                    claim_with_retry(state, winner.user_id, &ckey, game_id).await;
                }
                Err(e) => {
                    tracing::error!(game_id, "split create check: {e}");
                }
            }
        }

        // Collect fee
        if fee_amount > 0 && state.dice_fee_account > 0 {
            let fee_str = fee_amount.to_string();
            match state
                .outlayer
                .create_payment_check(
                    collector.user_id,
                    &token.contract,
                    &fee_str,
                    &format!("dice:{game_id}:fee"),
                )
                .await
            {
                Ok((_cid, ckey)) => {
                    claim_with_retry(state, state.dice_fee_account, &ckey, game_id).await;
                    tracing::info!(game_id, fee = fee_str.as_str(), "dice fee collected");
                }
                Err(e) => {
                    tracing::error!(game_id, "fee create check: {e}");
                }
            }
        }
    }

    // Update original message + send new message with results
    let text = results_message(&game, token, &winners, &prize_each_str, fee_pct);
    let _ = bot
        .edit_message_text(
            ChatId(game.chat_id),
            teloxide::types::MessageId(game.message_id),
            &text,
        )
        .parse_mode(HTML)
        .await;
    let _ = bot.send_message(ChatId(game.chat_id), &text)
        .parse_mode(HTML)
        .await;

    cleanup_game(state, &game);
    persist_games(state);

    let winner_names: Vec<&str> = winners.iter().map(|w| w.display_name.as_str()).collect();
    tracing::info!(game_id, winners = ?winner_names, "dice game resolved");
}

// ── Callback: refresh timer ───────────────────────────────────────

pub async fn handle_refresh(bot: &Bot, state: &AppState, chat_id: i64, msg_id: i32, game_id: GameId) {
    let game = match state.dice_games.get(&game_id) {
        Some(g) => g.clone(),
        None => return,
    };

    let token = game_token(state, &game.token_key);
    let now = now_ts();

    match game.phase {
        GamePhase::Betting => {
            let remaining = game.betting_deadline.saturating_sub(now);
            let (text, kb) = betting_message(&game, token, remaining);
            edit_game_msg(bot, chat_id, msg_id, &text, kb).await;
        }
        GamePhase::Rolling => {
            let remaining = game.rolling_deadline.unwrap_or(0).saturating_sub(now);
            let (text, kb) = rolling_message(&game, token, remaining);
            edit_game_msg(bot, chat_id, msg_id, &text, kb).await;
        }
        _ => {}
    }
}

// ── Cleanup ───────────────────────────────────────────────────────

fn cleanup_game(state: &AppState, game: &DiceGame) {
    state.dice_msg_index.remove(&(game.chat_id, game.message_id));
    for p in &game.players {
        state.dice_player_index.remove(&(game.chat_id, p.user_id));
    }
    state.dice_games.remove(&game.game_id);
}
