use account::Inherent;
use accounts::Accounts;
use block::{Block, BlockError, MicroBlock, ViewChanges};
#[cfg(feature = "metrics")]
use blockchain_base::chain_metrics::BlockchainMetrics;
use database::WriteTransaction;

use crate::blockchain_state::BlockchainState;
use crate::chain_info::ChainInfo;
use crate::{Blockchain, PushError};

// complicated stuff
impl Blockchain {
    pub(crate) fn commit_accounts(
        &self,
        state: &BlockchainState,
        first_view_number: u32,
        txn: &mut WriteTransaction,
        chain_info: &ChainInfo,
    ) -> Result<(), PushError> {
        let block = &chain_info.head;
        let accounts = &state.accounts;

        match block {
            Block::Macro(ref macro_block) => {
                let mut inherents: Vec<Inherent> = vec![];
                if macro_block.is_election_block() {
                    // On election the previous epoch needs to be finalized.
                    // We can rely on `state` here, since we cannot revert macro blocks.
                    inherents.append(&mut self.finalize_previous_epoch(state, chain_info));
                }

                // Add slashes for view changes.
                let view_changes = ViewChanges::new(
                    macro_block.header.block_number,
                    first_view_number,
                    macro_block.header.view_number,
                );
                inherents.append(&mut self.create_slash_inherents(&[], &view_changes, Some(txn)));

                // Commit block to AccountsTree.
                let receipts =
                    accounts.commit(txn, &[], &inherents, macro_block.header.block_number);

                // macro blocks are final and receipts for the previous batch are no longer necessary
                // as rebranching across this block is not possible
                self.chain_store.clear_receipts(txn);
                if let Err(e) = receipts {
                    return Err(PushError::AccountsError(e));
                }
            }
            Block::Micro(ref micro_block) => {
                let extrinsics = micro_block.body.as_ref().unwrap();
                let view_changes = ViewChanges::new(
                    micro_block.header.block_number,
                    first_view_number,
                    micro_block.header.view_number,
                );
                let inherents =
                    self.create_slash_inherents(&extrinsics.fork_proofs, &view_changes, Some(txn));

                // Commit block to AccountsTree.
                let receipts = accounts.commit(
                    txn,
                    &extrinsics.transactions,
                    &inherents,
                    micro_block.header.block_number,
                );
                if let Err(e) = receipts {
                    return Err(PushError::AccountsError(e));
                }

                // Store receipts.
                let receipts = receipts.unwrap();
                self.chain_store
                    .put_receipts(txn, micro_block.header.block_number, &receipts);
            }
        }

        // Verify accounts hash.
        let accounts_hash = accounts.hash(Some(&txn));
        trace!("Block state root: {}", block.state_root());
        trace!("Accounts hash:    {}", accounts_hash);
        if block.state_root() != &accounts_hash {
            return Err(PushError::InvalidBlock(BlockError::AccountsHashMismatch));
        }

        Ok(())
    }

    pub(crate) fn revert_accounts(
        &self,
        accounts: &Accounts,
        txn: &mut WriteTransaction,
        micro_block: &MicroBlock,
        prev_view_number: u32,
    ) -> Result<(), PushError> {
        assert_eq!(
            micro_block.header.state_root,
            accounts.hash(Some(&txn)),
            "Failed to revert - inconsistent state"
        );

        let extrinsics = micro_block.body.as_ref().unwrap();
        let view_changes = ViewChanges::new(
            micro_block.header.block_number,
            prev_view_number,
            micro_block.header.view_number,
        );
        let inherents =
            self.create_slash_inherents(&extrinsics.fork_proofs, &view_changes, Some(txn));
        let receipts = self
            .chain_store
            .get_receipts(micro_block.header.block_number, Some(txn))
            .expect("Failed to revert - missing receipts");

        if let Err(e) = accounts.revert(
            txn,
            &extrinsics.transactions,
            &inherents,
            micro_block.header.block_number,
            &receipts,
        ) {
            panic!("Failed to revert - {}", e);
        }

        Ok(())
    }
}