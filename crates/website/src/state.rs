use crate::templates::IndexTemplate;
use helix_common::chain_info::ChainInfo;
use helix_common::WebsiteConfig;
use helix_database::postgres::postgres_db_service::PostgresDatabaseService;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub db_pool: Arc<PostgresDatabaseService>,
    pub chain_info: Arc<ChainInfo>,
    pub website_config: WebsiteConfig,
    pub cached_templates: Arc<RwLock<CachedTemplates>>,
    pub latest_slot: Arc<RwLock<u64>>,
}

pub struct CachedTemplates {
    pub default: IndexTemplate,
    pub by_value_desc: IndexTemplate,
    pub by_value_asc: IndexTemplate,
}
