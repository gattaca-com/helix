use teloxide::{
    payloads::SendMessageSetters,
    prelude::Requester,
    types::{ChatId, ParseMode},
    Bot,
};
use tracing::error;

use crate::RelayConfig;

#[derive(Clone)]
pub enum AlertManager {
    Disabled,
    Telegram { bot: Bot, chat_id: ChatId },
}

impl AlertManager {
    pub fn from_relay_config(cfg: &RelayConfig) -> Self {
        match &cfg.alerts_config {
            Some(alerts) if !alerts.telegram_bot_token.is_empty() && alerts.chat_id != 0 => {
                AlertManager::Telegram {
                    bot: Bot::new(&alerts.telegram_bot_token),
                    chat_id: ChatId(alerts.chat_id),
                }
            }
            _ => AlertManager::Disabled,
        }
    }

    pub fn send(&self, message: &str) {
        match self {
            AlertManager::Disabled => {}
            AlertManager::Telegram { bot, chat_id } => {
                let bot = bot.clone();
                let chat_id = *chat_id;
                let msg = message.to_owned();

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
