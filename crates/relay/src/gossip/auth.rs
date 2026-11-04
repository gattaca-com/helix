use subtle::ConstantTimeEq;
use tonic::{
    Request, Status,
    metadata::{Ascii, MetadataValue},
};

pub(super) const GOSSIP_TOKEN_ENV_VAR: &str = "GOSSIP_TOKEN";
const GOSSIP_TOKEN_HEADER: &str = "x-gossip-token";

/// Shared secret that relays send with every gossip request. The gossip server rejects
/// requests that do not carry it.
#[derive(Clone)]
pub struct GossipToken(MetadataValue<Ascii>);

impl GossipToken {
    pub fn from_env() -> Option<Self> {
        std::env::var(GOSSIP_TOKEN_ENV_VAR).ok().map(Self::new)
    }

    fn new(token: String) -> Self {
        assert!(!token.is_empty(), "{GOSSIP_TOKEN_ENV_VAR} should not be empty");
        Self(
            MetadataValue::try_from(token)
                .unwrap_or_else(|_| panic!("{GOSSIP_TOKEN_ENV_VAR} should be printable ASCII")),
        )
    }

    pub fn attach<T>(&self, request: &mut Request<T>) {
        request.metadata_mut().insert(GOSSIP_TOKEN_HEADER, self.0.clone());
    }

    pub fn verify<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let valid = request
            .metadata()
            .get(GOSSIP_TOKEN_HEADER)
            .is_some_and(|token| token.as_bytes().ct_eq(self.0.as_bytes()).into());

        if valid { Ok(()) } else { Err(Status::unauthenticated("invalid gossip token")) }
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    #[test]
    fn verify_accepts_only_matching_token() {
        let token = GossipToken::new("secret".to_string());

        let mut signed = Request::new(());
        token.attach(&mut signed);
        assert!(token.verify(&signed).is_ok());

        let mut wrong = Request::new(());
        GossipToken::new("other".to_string()).attach(&mut wrong);
        assert_eq!(token.verify(&wrong).unwrap_err().code(), Code::Unauthenticated);

        let missing = Request::new(());
        assert_eq!(token.verify(&missing).unwrap_err().code(), Code::Unauthenticated);
    }
}
