/// KCP protocol errors
#[cfg_attr(test, derive(Debug))]
pub enum Error {
    ConvInconsistent,
    InvalidSegmentSize,
    InvalidSegmentDataSize,
    NeedUpdate,
    RecvQueueEmpty,
    ExpectingFragment,
    UnsupportedCmd,
    UserBufTooBig,
    UserBufTooSmall,
}
