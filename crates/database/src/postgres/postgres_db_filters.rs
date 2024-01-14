use helix_common::api::data_api::BidFilters;


#[derive(Clone, Debug)]
pub struct PgBidFilters(BidFilters);

impl From<&BidFilters> for PgBidFilters {
    fn from(value: &BidFilters) -> Self {
        PgBidFilters(value.clone())
    }
}

impl From<BidFilters> for PgBidFilters {
    fn from(value: BidFilters) -> Self {
        PgBidFilters(value)
    }
}

impl PgBidFilters {
    pub fn slot(&self) -> Option<i32> {
        self.0.slot.map(|slot| slot as i32)
    }

    pub fn cursor(&self) -> Option<i32> {
        self.0.cursor.map(|cursor| cursor as i32)
    }

    pub fn block_number(&self) -> Option<i32> {
        self.0.block_number.map(|block_number| block_number as i32)
    }

    pub fn proposer_pubkey(&self) -> Option<&[u8]> {
        self.0.proposer_pubkey.as_ref().map(|pubkey| pubkey.as_ref())
    }

    pub fn builder_pubkey(&self) -> Option<&[u8]> {
        self.0.builder_pubkey.as_ref().map(|pubkey| pubkey.as_ref())
    }

    pub fn block_hash(&self) -> Option<&[u8]> {
        self.0.block_hash.as_ref().map(|block_hash| block_hash.as_ref())
    }

    pub fn order(&self) -> Option<i32> {
        self.0.order_by.map(|order_by| order_by as i32)
    }

    pub fn limit(&self) -> Option<i64> {
        self.0.limit.map(|limit| limit as i64)
    }
}
