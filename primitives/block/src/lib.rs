#[macro_use]
extern crate log;

use thiserror::Error;

pub use block::*;
pub use fork_proof::*;
pub use macro_block::*;
pub use micro_block::*;
pub use multisig::*;
use nimiq_transaction::TransactionError;
pub use signed::*;
pub use skip_block::*;
pub use tendermint::*;

mod block;
mod fork_proof;
mod macro_block;
mod micro_block;
mod multisig;
mod signed;
mod skip_block;
mod tendermint;

/// Enum containing a variety of block error types.
#[derive(Error, Debug, PartialEq, Eq)]
pub enum BlockError {
    #[error("Unsupported version")]
    UnsupportedVersion,
    #[error("Extra data too large")]
    ExtraDataTooLarge,
    #[error("Block is from the future")]
    FromTheFuture,
    #[error("Block size exceeded")]
    SizeExceeded,

    #[error("Body hash mismatch")]
    BodyHashMismatch,
    #[error("Accounts hash mismatch")]
    AccountsHashMismatch,

    #[error("Missing justification")]
    NoJustification,
    #[error("Missing skip block proof")]
    NoSkipBlockProof,
    #[error("Missing body")]
    MissingBody,

    #[error("Invalid fork proof")]
    InvalidForkProof,
    #[error("Duplicate fork proof")]
    DuplicateForkProof,
    #[error("Fork proofs incorrectly ordered")]
    ForkProofsNotOrdered,

    #[error("Duplicate transaction in block")]
    DuplicateTransaction,
    #[error("Invalid transaction in block: {}", _0)]
    InvalidTransaction(#[from] TransactionError),
    #[error("Expired transaction in block")]
    ExpiredTransaction,
    #[error("Transactions execution result mismatch")]
    TransactionExecutionMismatch,

    #[error("Duplicate receipt in block")]
    DuplicateReceipt,
    #[error("Invalid receipt in block")]
    InvalidReceipt,
    #[error("Receipts incorrectly ordered")]
    ReceiptsNotOrdered,

    #[error("Justification is invalid")]
    InvalidJustification,
    #[error("Skip block proof is invalid")]
    InvalidSkipBlockProof,
    #[error("Contains an invalid seed")]
    InvalidSeed,
    #[error("Invalid history root")]
    InvalidHistoryRoot,
    #[error("Incorrect validators")]
    InvalidValidators,
    #[error("Incorrect PK Tree root")]
    InvalidPkTreeRoot,
    #[error("Invalid skip block timestamp")]
    InvalidSkipBlockTimestamp,
    #[error("Skip block contains a non empty body")]
    InvalidSkipBlockBody,
}
