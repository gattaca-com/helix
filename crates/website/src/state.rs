use std::sync::Arc;

use helix_common::{chain_info::ChainInfo, WebsiteConfig};
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use helix_housekeeper::CurrentSlotInfo;
use parking_lot::RwLock;

use crate::templates::IndexTemplate;

#[derive(Clone)]
pub struct AppState {
    pub db_pool: Arc<PostgresDatabaseService>,
    pub chain_info: Arc<ChainInfo>,
    pub website_config: WebsiteConfig,
    pub cached_templates: Arc<RwLock<CachedTemplates>>,
    pub current_slot_info: CurrentSlotInfo,
}

pub struct CachedTemplates {
    pub default: IndexTemplate,
    pub by_value_desc: IndexTemplate,
    pub by_value_asc: IndexTemplate,
}
