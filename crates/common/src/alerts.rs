use std::{collections::HashMap, sync::Arc};

use alloy_primitives::B256;
use dashmap::DashMap;
use helix_types::BlsPublicKeyBytes;
use teloxide::{
    Bot,
    payloads::SendMessageSetters,
    prelude::Requester,
    types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tracing::error;
use uuid::Uuid;

use crate::RelayConfig;

#[derive(Clone, Default)]
pub struct PromotionTokenCache(Arc<DashMap<String, BlsPublicKeyBytes>>);

impl PromotionTokenCache {
    pub fn insert(&self, pubkey: BlsPublicKeyBytes) -> String {
        let token = Uuid::new_v4().to_string();
        self.0.insert(token.clone(), pubkey);
        token
    }

    // Consumes token (one-shot — can't reuse)
    pub fn consume(&self, token: &str) -> Option<BlsPublicKeyBytes> {
        self.0.remove(token).map(|(_, pubkey)| pubkey)
    }
}

#[derive(Clone)]
pub enum AlertManager {
    Disabled,
    Telegram {
        bot: Bot,
        merged_blocks_chat_ids: Vec<ChatId>,
        demotion_chat_id: Option<ChatId>,
        builder_demotion_chat_ids: HashMap<String, ChatId>,
        promotion_tokens: PromotionTokenCache,
        relay_url: String,
    },
}

impl AlertManager {
    pub fn from_relay_config(cfg: &RelayConfig) -> Self {
        match &cfg.alerts_config {
            Some(alerts) if !alerts.telegram_bot_token.is_empty() => AlertManager::Telegram {
                bot: Bot::new(&alerts.telegram_bot_token),
                merged_blocks_chat_ids: alerts.merged_blocks_chat_ids.clone(),
                demotion_chat_id: alerts.demotion_chat_id,
                builder_demotion_chat_ids: alerts.builder_demotion_chat_ids.clone(),
                promotion_tokens: PromotionTokenCache::default(),
                relay_url: alerts.relay_url.clone(),
            },
            _ => AlertManager::Disabled,
        }
    }

    pub fn send_merged_block(&self, message: &str) {
        let AlertManager::Telegram { bot, merged_blocks_chat_ids, .. } = self else { return };

        for chat_id in merged_blocks_chat_ids {
            let bot = bot.clone();
            let chat_id = *chat_id;
            let msg = message.to_owned();

            crate::spawn_tracked!(async move {
                if let Err(e) =
                    bot.send_message(chat_id, msg).parse_mode(ParseMode::MarkdownV2).await
                {
                    error!("alert send error: {}", e);
                }
            });
        }
    }

    pub fn send_promotion(&self, message: &str, builder_id: &str) {
        let AlertManager::Telegram { bot, demotion_chat_id, builder_demotion_chat_ids, .. } = self
        else {
            return;
        };

        for chat_id in demotion_targets(*demotion_chat_id, builder_demotion_chat_ids, builder_id) {
            let bot = bot.clone();
            let msg = message.to_owned();

            crate::spawn_tracked!(async move {
                if let Err(e) =
                    bot.send_message(chat_id, msg).parse_mode(ParseMode::MarkdownV2).await
                {
                    error!("promotion alert send error: {e}");
                }
            });
        }
    }

    pub fn send_demotion(&self, message: &str, token: &str, builder_id: &str) {
        let AlertManager::Telegram {
            bot,
            demotion_chat_id,
            builder_demotion_chat_ids,
            relay_url,
            ..
        } = self
        else {
            return;
        };

        let url = format!("{relay_url}/relay/v2/builder/promote?token={token}");

        for chat_id in demotion_targets(*demotion_chat_id, builder_demotion_chat_ids, builder_id) {
            let bot = bot.clone();
            let msg = message.to_owned();
            let url = url.clone();

            crate::spawn_tracked!(async move {
                let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::url(
                    "🔁 Repromote",
                    url.parse().expect("valid url"),
                )]]);
                if let Err(e) = bot
                    .send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboard)
                    .await
                {
                    error!("demotion alert send error: {e}");
                }
            });
        }
    }

    pub fn generate_token(&self, pubkey: BlsPublicKeyBytes) -> String {
        match self {
            AlertManager::Telegram { promotion_tokens, .. } => promotion_tokens.insert(pubkey),
            AlertManager::Disabled => String::new(),
        }
    }

    pub fn consume_promotion_token(&self, token: &str) -> Option<BlsPublicKeyBytes> {
        match self {
            AlertManager::Telegram { promotion_tokens, .. } => promotion_tokens.consume(token),
            AlertManager::Disabled => None,
        }
    }
}

// shared demotion channel + builder-specific channel if configured
fn demotion_targets(
    demotion_chat_id: Option<ChatId>,
    builder_demotion_chat_ids: &HashMap<String, ChatId>,
    builder_id: &str,
) -> impl Iterator<Item = ChatId> {
    let builder_chat = builder_demotion_chat_ids
        .get(builder_id)
        .copied()
        .filter(|id| Some(*id) != demotion_chat_id);
    demotion_chat_id.into_iter().chain(builder_chat)
}

pub fn format_demotion_alert(
    slot: u64,
    network: &str,
    region: &str,
    builder_pubkey: &BlsPublicKeyBytes,
    builder_id: &str,
    block_hash: &B256,
    reason: &str,
) -> String {
    format!(
        "⚠️ *Builder Demoted*\n\
            \n\
            *Link:* https://beaconcha\\.in/slot/{slot}\n\
            *Slot:* `{slot}`\n\
            *Network:* `{network}`\n\
            *Region:* `{region}`\n\
            *Builder Pubkey:* `{builder_pubkey}`\n\
            *Builder ID:* `{builder_id}`\n\
            *Block Hash:* `{block_hash}`\n\
            *Reason:* `{reason}`\n",
        slot = slot,
        network = network,
        region = region,
        builder_pubkey = builder_pubkey,
        builder_id = builder_id,
        block_hash = block_hash,
        reason = reason,
    )
}
