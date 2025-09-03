use helix_common::api::builder_api::InclusionList;
use helix_types::BlsSignature;

pub(crate) struct SignedMessage {
    message: Message,
    /// Signature over SSZ hash-tree-root of message.
    signature: BlsSignature,
}

pub(crate) enum Message {
    InclusionList(InclusionListMessage),
}

pub(crate) struct InclusionListMessage {
    inclusion_list: InclusionList,
}
