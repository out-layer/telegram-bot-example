use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::ReplyParameters;

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

#[derive(Serialize, Deserialize, Default)]
pub struct DiceGameStore {
    pub games: Vec<DiceGame>,
    pub next_id: GameId,
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
        .map(|r| r.value().clone())
        .collect();
    let store = DiceGameStore {
        games,
        next_id: state.dice_next_id.load(Ordering::Relaxed),
    };
    let tmp = format!("{}.tmp", state.dice_games_file);
    if let Ok(data) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(&tmp, &data);
        let _ = std::fs::rename(&tmp, &state.dice_games_file);
    }
}

pub fn restore_games(state: &Arc<AppState>, bot: &Bot) {
    let store = load_games(&state.dice_games_file);
    state.dice_next_id.store(store.next_id, Ordering::Relaxed);
    let now = now_ts();

    for game in store.games {
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
        .unwrap_or_else(|| user.first_name.clone())
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

macro_rules! reply {
    ($bot:expr, $msg:expr, $text:expr) => {
        $bot.send_message($msg.chat.id, $text)
            .reply_parameters(ReplyParameters::new($msg.id))
            .await?
    };
}

// ── Message formatting ────────────────────────────────────────────

fn betting_message(game: &DiceGame, token: &TokenConfig, remaining_secs: u64) -> String {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let players_list: Vec<&str> = game.players.iter().map(|p| p.display_name.as_str()).collect();
    let mins = remaining_secs / 60;
    let secs = remaining_secs % 60;
    let cmd = token.symbol.to_lowercase();
    let demo_tag = if game.demo { " [DEMO]" } else { "" };
    format!(
        "🎲 <b>Dice Game{demo_tag} — {prefix}{stake_display} {symbol}</b>\n\n\
         Bet: {prefix}{stake_display} {symbol}\n\
         Players ({count}): {players}\n\n\
         ⏳ Waiting for players... ({mins}:{secs:02})\n\
         Reply <code>/{cmd} {stake_display}</code> to join!",
        prefix = token.prefix,
        symbol = token.symbol,
        count = game.players.len(),
        players = players_list.join(", "),
    )
}

fn rolling_message(game: &DiceGame, token: &TokenConfig, remaining_secs: u64) -> String {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let mins = remaining_secs / 60;
    let secs = remaining_secs % 60;
    let demo_tag = if game.demo { " [DEMO]" } else { "" };

    let mut lines = Vec::new();
    for p in &game.players {
        let status = match p.dice_value {
            Some(v) => format!("🎲 {v}"),
            None => "⏳".to_string(),
        };
        lines.push(format!("{} — {status}", p.display_name));
    }

    format!(
        "🎲 <b>Dice Game{demo_tag} — Roll!</b>\n\
         Bet: {prefix}{stake_display} {symbol}\n\n\
         {players}\n\n\
         Send 🎲 to roll! ({mins}:{secs:02})",
        prefix = token.prefix,
        symbol = token.symbol,
        players = lines.join("\n"),
    )
}

fn results_message(game: &DiceGame, token: &TokenConfig, winners: &[&GamePlayer], prize_each: &str) -> String {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let prize_display = format_amount(prize_each, token.decimals, token.display_dp);
    let demo_tag = if game.demo { " [DEMO]" } else { "" };

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

    format!(
        "🎲 <b>Dice Game{demo_tag} — Results!</b>\n\
         Bet: {prefix}{stake_display} {symbol}\n\n\
         {players}\n\n\
         {winner_text}",
        prefix = token.prefix,
        symbol = token.symbol,
        players = lines.join("\n"),
    )
}

fn cancelled_message(game: &DiceGame, token: &TokenConfig) -> String {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    let creator = &game.players[0].display_name;
    format!(
        "🎲 <b>Dice Game — Cancelled</b>\n\n\
         No one joined. {prefix}{stake_display} {symbol} returned to {creator}.",
        prefix = token.prefix,
        symbol = token.symbol,
    )
}

fn all_zero_message(game: &DiceGame, token: &TokenConfig) -> String {
    let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
    format!(
        "🎲 <b>Dice Game — No Winners</b>\n\
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

    let text = betting_message(&game, token, state.dice_betting_timeout);
    let sent = bot.send_message(msg.chat.id, &text)
        .parse_mode(HTML)
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

    // Read game state
    let game = match state.dice_games.get(&game_id) {
        Some(g) => g.clone(),
        None => return Ok(()),
    };

    if game.phase != GamePhase::Betting {
        reply!(bot, msg, "🎲 Betting phase is over.");
        return Ok(());
    }

    // Check token match
    let game_token_cfg = game_token(&state, &game.token_key);
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

    // Already in game?
    if game.players.iter().any(|p| p.user_id == sender.id.0) {
        reply!(bot, msg, "🎲 You're already in this game!");
        return Ok(());
    }

    // Parse amount
    let amount_raw = match parse_amount(args.trim(), token.decimals) {
        Some(a) if a > 0 => a,
        _ => {
            let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
            reply!(
                bot,
                msg,
                format!("Invalid amount. Reply with /{} {stake_display}", token.symbol.to_lowercase())
            );
            return Ok(());
        }
    };

    let min_stake: u128 = game.min_stake_raw.parse().unwrap_or(0);
    if amount_raw < min_stake {
        let stake_display = format_amount(&game.min_stake_raw, token.decimals, token.display_dp);
        reply!(
            bot,
            msg,
            format!("🎲 Minimum bet is {}{stake_display} {}", token.prefix, token.symbol)
        );
        return Ok(());
    }

    let stake_str = min_stake.to_string();
    let (check_id, check_key) = if game.demo {
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

    // Update game
    let now = now_ts();
    {
        let mut game = state.dice_games.get_mut(&game_id).unwrap();
        game.players.push(GamePlayer {
            user_id: sender.id.0,
            display_name: display_name(sender),
            stake_raw: stake_str,
            check_id,
            check_key,
            dice_value: None,
        });

        // Update announcement
        let remaining = game.betting_deadline.saturating_sub(now);
        let text = betting_message(&game, game_token_cfg, remaining);
        let _ = bot
            .edit_message_text(ChatId(chat_id), teloxide::types::MessageId(game.message_id), text)
            .parse_mode(HTML)
            .await;
    }

    persist_games(&state);

    reply!(bot, msg, format!("🎲 {} joined the game!", display_name(sender)));
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
        None => return Ok(()), // Not a game participant
    };

    let should_resolve;

    {
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
            // Try to delete the duplicate dice message
            let _ = bot.delete_message(ChatId(chat_id), msg.id).await;
            bot.send_message(
                ChatId(chat_id),
                format!("🎲 {} already rolled! Only the first roll counts.", player.display_name),
            )
            .await?;
            return Ok(());
        }

        player.dice_value = Some(dice_value);

        // Update game message
        let token = game_token(&state, &game.token_key);
        let remaining = game.rolling_deadline.unwrap_or(0).saturating_sub(now_ts());
        let text = rolling_message(&game, token, remaining);
        let _ = bot
            .edit_message_text(ChatId(chat_id), teloxide::types::MessageId(game.message_id), text)
            .parse_mode(HTML)
            .await;

        // Check if all rolled
        should_resolve = game.players.iter().all(|p| p.dice_value.is_some());
    }

    persist_games(&state);

    if should_resolve {
        resolve_game(&bot, &state, game_id).await;
    }

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
    let game = match state.dice_games.get(&game_id) {
        Some(g) => g.clone(),
        None => return,
    };
    if game.phase != GamePhase::Betting {
        return;
    }

    let token = game_token(state, &game.token_key);

    if game.players.len() < 2 {
        // Cancel — refund the single player
        if !game.demo {
            let player = &game.players[0];
            let _ = state
                .outlayer
                .reclaim_payment_check(player.user_id, &player.check_id)
                .await;
        }

        let text = cancelled_message(&game, token);
        let _ = bot
            .edit_message_text(
                ChatId(game.chat_id),
                teloxide::types::MessageId(game.message_id),
                text,
            )
            .parse_mode(HTML)
            .await;

        cleanup_game(state, &game);
        if let Some(mut g) = state.dice_games.get_mut(&game_id) {
            g.phase = GamePhase::Cancelled;
        }
        persist_games(state);
        tracing::info!(game_id, "dice game cancelled (no joiners)");
        return;
    }

    // Transition to rolling phase
    let now = now_ts();
    let rolling_deadline = now + state.dice_rolling_timeout;

    {
        let mut g = state.dice_games.get_mut(&game_id).unwrap();
        g.phase = GamePhase::Rolling;
        g.rolling_deadline = Some(rolling_deadline);

        // Build player index for dice detection
        for p in &g.players {
            state.dice_player_index.insert((g.chat_id, p.user_id), game_id);
        }

        let text = rolling_message(&g, token, state.dice_rolling_timeout);
        let _ = bot
            .edit_message_text(
                ChatId(g.chat_id),
                teloxide::types::MessageId(g.message_id),
                text,
            )
            .parse_mode(HTML)
            .await;
    }

    persist_games(state);

    // Notify in chat
    let _ = bot
        .send_message(
            ChatId(game.chat_id),
            "🎲 Bets are closed! Roll your dice!",
        )
        .await;

    spawn_rolling_timer(bot.clone(), state.clone(), game_id, rolling_deadline, now);

    tracing::info!(game_id, "dice game moved to rolling phase");
}

async fn handle_rolling_timeout(bot: &Bot, state: &Arc<AppState>, game_id: GameId) {
    let game = match state.dice_games.get(&game_id) {
        Some(g) => g.clone(),
        None => return,
    };
    if game.phase != GamePhase::Rolling {
        return;
    }

    resolve_game(bot, state, game_id).await;
}

// ── Resolve game ──────────────────────────────────────────────────

async fn resolve_game(bot: &Bot, state: &Arc<AppState>, game_id: GameId) {
    let game = match state.dice_games.get(&game_id) {
        Some(g) => g.clone(),
        None => return,
    };

    if game.phase != GamePhase::Rolling {
        return;
    }

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
                text,
            )
            .parse_mode(HTML)
            .await;

        cleanup_game(state, &game);
        if let Some(mut g) = state.dice_games.get_mut(&game_id) {
            g.phase = GamePhase::Finished;
        }
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
    let prize_each = total_pot / winners.len() as u128;
    let prize_each_str = prize_each.to_string();

    // Distribute funds
    if game.demo {
        // Demo mode: no actual transfers
    } else if winners.len() == 1 {
        // Single winner — claim all checks to winner
        let winner = winners[0];
        for p in &game.players {
            for attempt in 0..3u32 {
                match state
                    .outlayer
                    .claim_payment_check(winner.user_id, &p.check_key)
                    .await
                {
                    Ok(_) => break,
                    Err(e) => {
                        tracing::warn!(game_id, attempt, "claim check: {e}");
                        if attempt < 2 {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    } else {
        // Multiple winners — claim all to first winner, then redistribute
        let first_winner = winners[0];
        for p in &game.players {
            for attempt in 0..3u32 {
                match state
                    .outlayer
                    .claim_payment_check(first_winner.user_id, &p.check_key)
                    .await
                {
                    Ok(_) => break,
                    Err(e) => {
                        tracing::warn!(game_id, attempt, "claim to first winner: {e}");
                        if attempt < 2 {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }

        // Redistribute to other winners
        for &winner in &winners[1..] {
            match state
                .outlayer
                .create_payment_check(
                    first_winner.user_id,
                    &token.contract,
                    &prize_each_str,
                    &format!("dice:{game_id}:split"),
                )
                .await
            {
                Ok((_cid, ckey)) => {
                    for attempt in 0..3u32 {
                        match state
                            .outlayer
                            .claim_payment_check(winner.user_id, &ckey)
                            .await
                        {
                            Ok(_) => break,
                            Err(e) => {
                                tracing::warn!(game_id, attempt, "split claim: {e}");
                                if attempt < 2 {
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(game_id, "split create check: {e}");
                }
            }
        }
    }

    // Update message
    let text = results_message(&game, token, &winners, &prize_each_str);
    let _ = bot
        .edit_message_text(
            ChatId(game.chat_id),
            teloxide::types::MessageId(game.message_id),
            text,
        )
        .parse_mode(HTML)
        .await;

    cleanup_game(state, &game);
    if let Some(mut g) = state.dice_games.get_mut(&game_id) {
        g.phase = GamePhase::Finished;
    }
    persist_games(state);

    let winner_names: Vec<&str> = winners.iter().map(|w| w.display_name.as_str()).collect();
    tracing::info!(game_id, winners = ?winner_names, "dice game resolved");
}

// ── Cleanup ───────────────────────────────────────────────────────

fn cleanup_game(state: &AppState, game: &DiceGame) {
    state.dice_msg_index.remove(&(game.chat_id, game.message_id));
    for p in &game.players {
        state.dice_player_index.remove(&(game.chat_id, p.user_id));
    }
    // Keep game in dice_games with Finished/Cancelled phase for persistence
    // It will be cleaned up on next restart (only active games are restored)
}
