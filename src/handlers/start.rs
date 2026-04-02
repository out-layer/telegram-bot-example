use std::sync::Arc;
use teloxide::prelude::*;

use super::{menu_view, HTML};
use crate::AppState;

pub async fn handle(bot: Bot, msg: Message, state: Arc<AppState>) -> ResponseResult<()> {
    if !msg.chat.is_private() {
        return Ok(());
    }

    let user_id = match &msg.from {
        Some(u) => u.id.0,
        None => return Ok(()),
    };

    state.conversations.remove(&user_id);

    let (text, kb) = menu_view(&state, user_id).await;

    bot.send_message(msg.chat.id, text)
        .parse_mode(HTML)
        .reply_markup(kb)
        .await?;

    Ok(())
}
