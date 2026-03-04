// references position in SharedVector<InternalBidSubmission>
#[derive(Debug, Clone, Copy)]
pub struct NewBidSubmissionIx {
    pub ix: usize,
}

// references position in SharedVector<SubmissionResultWithRef>
#[derive(Debug, Clone, Copy)]
pub struct SubmissionResultIx {
    pub ix: usize,
}

// references position in SharedVector<BidSubmission>
#[derive(Debug, Clone, Copy)]
pub struct DecodedSubmission {
    pub ix: usize,
}
