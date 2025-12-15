use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    prelude::Requester,
    types::{ChatId, ParseMode},
};
use tracing::error;

use crate::RelayConfig;

#[derive(Clone)]
pub enum AlertManager {
    Disabled,
    Telegram { bot: Bot, chat_ids: Vec<ChatId> },
}

impl AlertManager {
    pub fn from_relay_config(cfg: &RelayConfig) -> Self {
        match &cfg.alerts_config {
            Some(alerts) if !alerts.telegram_bot_token.is_empty() && !alerts.chat_ids.is_empty() => {
                AlertManager::Telegram {
                    bot: Bot::new(&alerts.telegram_bot_token),
                    chat_ids: alerts.chat_ids.clone(),
                }
            }
            _ => AlertManager::Disabled,
        }
    }

    pub fn send(&self, message: &str) {
        match self {
            AlertManager::Disabled => {}
            AlertManager::Telegram { bot, chat_ids } => {
                let bot = bot.clone();
                let msg = message.to_owned();

                for chat_id in chat_ids {
                    let bot = bot.clone();
                    let chat_id = *chat_id;
                    let msg = msg.clone();

                    tokio::spawn(async move {
                        if let Err(e) =
                            bot.send_message(chat_id, msg).parse_mode(ParseMode::MarkdownV2).await
                        {
                            error!("alert send error: {}", e);
                        }
                    });
                }
            }
        }
    }
}
