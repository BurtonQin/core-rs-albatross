use std::error::Error;
use std::ops::Deref;

use nimiq_account::BlockLog;
use parking_lot::{RwLockUpgradableReadGuard, RwLockWriteGuard};

use nimiq_block::{Block, ForkProof};
use nimiq_database::WriteTransaction;
use nimiq_hash::{Blake2bHash, Hash};
use nimiq_primitives::policy;

use crate::blockchain_state::BlockchainState;
use crate::chain_info::ChainInfo;
use crate::chain_store::MAX_EPOCHS_STORED;
use crate::{
    AbstractBlockchain, Blockchain, BlockchainEvent, ChainOrdering, ForkEvent, PushError,
    PushResult,
};

/// Implements methods to push blocks into the chain. This is used when the node has already synced
/// and is just receiving newly produced blocks. It is also used for the final phase of syncing,
/// when the node is just receiving micro blocks.
impl Blockchain {
    /// Private function to push a block.
    /// Set the trusted flag to true to skip VRF and signature verifications: when the source of the
    /// block can be trusted.
    fn do_push(
        this: RwLockUpgradableReadGuard<Self>,
        block: Block,
        trusted: bool,
    ) -> Result<PushResult, PushError> {
        // Ignore all blocks that precede (or are at the same height) as the most recent accepted
        // macro block.
        let last_macro_block = policy::last_macro_block(this.block_number());
        if block.block_number() <= last_macro_block {
            debug!(
                block_no = block.block_number(),
                reason = "we have already finalized an earlier macro block",
                last_macro_block_no = last_macro_block,
                "Ignoring block",
            );
            return Ok(PushResult::Ignored);
        }

        // TODO: We might want to pass this as argument to this method.
        let read_txn = this.read_transaction();

        // Check if we already know this block.
        if this
            .chain_store
            .get_chain_info(&block.hash(), false, Some(&read_txn))
            .is_some()
        {
            return Ok(PushResult::Known);
        }

        // Check if we have this block's parent.
        let prev_info = this
            .chain_store
            .get_chain_info(block.parent_hash(), false, Some(&read_txn))
            .ok_or_else(|| {
                warn!(
                    %block,
                    reason = "parent block is unknown",
                    parent_block_hash = %block.parent_hash(),
                    "Rejecting block",
                );
                PushError::Orphan
            })?;

        // Get the intended block proposer.
        let offset = if let Block::Macro(macro_block) = &block {
            macro_block.round()
        } else {
            // Skip and micro block offset is block number
            block.block_number()
        };
        let proposer_slot = this
            .get_proposer_at(
                block.block_number(),
                offset,
                prev_info.head.seed().entropy(),
                Some(&read_txn),
            )
            .ok_or_else(|| {
                warn!(%block, reason = "failed to determine block proposer", "Rejecting block");
                PushError::Orphan
            })?;

        // Check the header.
        if let Err(e) = Blockchain::verify_block_header(
            this.deref(),
            &block.header(),
            &proposer_slot.validator.signing_key,
            Some(&read_txn),
            !trusted,
            block.is_skip(),
        ) {
            warn!(%block, reason = "bad header", "Rejecting block");
            return Err(e);
        }

        // Check the justification.
        if let Err(e) = Blockchain::verify_block_justification(
            &*this,
            &block,
            &proposer_slot.validator.signing_key,
            !trusted,
        ) {
            warn!(%block, reason = "bad justification", "Rejecting block");
            return Err(e);
        }

        // Check the body.
        if let Err(e) = this.verify_block_body(
            &block.header(),
            &block.body(),
            Some(&read_txn),
            block.is_skip(),
            !trusted,
        ) {
            warn!(%block, reason = "bad body", "Rejecting block");
            return Err(e);
        }

        // Detect forks in micro blocks other than skip block
        if !block.is_skip() {
            if let Block::Micro(micro_block) = &block {
                // Check if there are two blocks in the same slot and with the same height. Since we already
                // verified the validator for the current slot, this is enough to check for fork proofs.
                // Note: We don't verify the justifications for the other blocks here, since they had to
                // already be verified in order to be added to the blockchain.
                // Count the micro blocks after the last macro block.
                let mut micro_blocks: Vec<Block> =
                    this.chain_store
                        .get_blocks_at(block.block_number(), false, Some(&read_txn));

                micro_blocks.retain(|block| block.is_micro() && !block.is_skip());

                // Get the micro header from the block
                let micro_header1 = &micro_block.header;

                // Get the justification for the block. We assume that the
                // validator's signature is valid.
                let justification1 = match micro_block
                    .justification
                    .clone()
                    .expect("Missing justification!")
                {
                    nimiq_block::MicroJustification::Micro(signature) => signature,
                    nimiq_block::MicroJustification::Skip(_) => {
                        unreachable!("Skip blocks are already filtered")
                    }
                };

                for micro_block in micro_blocks.drain(..).map(|block| block.unwrap_micro()) {
                    // If there's another micro block set to this block height, which also has the same
                    // VrfSeed entropy we notify the fork event.
                    if block.seed().entropy() == micro_block.header.seed.entropy() {
                        let micro_header2 = micro_block.header;
                        let justification2 =
                            match micro_block.justification.expect("Missing justification!") {
                                nimiq_block::MicroJustification::Micro(signature) => signature,
                                nimiq_block::MicroJustification::Skip(_) => {
                                    unreachable!("Skip blocks are already filtered")
                                }
                            };

                        let proof = ForkProof {
                            header1: micro_header1.clone(),
                            header2: micro_header2,
                            justification1: justification1.clone(),
                            justification2,
                            prev_vrf_seed: prev_info.head.seed().clone(),
                        };

                        this.fork_notifier.notify(ForkEvent::Detected(proof));
                    }
                }
            }
        }

        // Calculate chain ordering.
        let chain_order =
            ChainOrdering::order_chains(this.deref(), &block, &prev_info, Some(&read_txn));

        read_txn.close();

        let chain_info = ChainInfo::from_block(block, &prev_info);

        // Extend, rebranch or just store the block depending on the chain ordering.
        let result = match chain_order {
            ChainOrdering::Extend => {
                return Blockchain::extend(this, chain_info.head.hash(), chain_info, prev_info);
            }
            ChainOrdering::Superior => {
                return Blockchain::rebranch(this, chain_info.head.hash(), chain_info);
            }
            ChainOrdering::Inferior => {
                debug!(block = %chain_info.head, "Storing block - on inferior chain");
                PushResult::Ignored
            }
            ChainOrdering::Unknown => {
                debug!(block = %chain_info.head, "Storing block - on fork");
                PushResult::Forked
            }
        };

        let mut txn = this.write_transaction();
        this.chain_store
            .put_chain_info(&mut txn, &chain_info.head.hash(), &chain_info, true);
        txn.commit();

        Ok(result)
    }

    // To retain the option of having already taken a lock before this call the self was exchanged.
    // This is a bit ugly but since push does only really need &mut self briefly at the end for the actual write
    // while needing &self for the majority it made sense to use upgradable read instead of self.
    // Note that there can always only ever be at most one RwLockUpgradableRead thus the push calls are also
    // sequentialized by it.
    /// Pushes a block into the chain.
    pub fn push(
        this: RwLockUpgradableReadGuard<Self>,
        block: Block,
    ) -> Result<PushResult, PushError> {
        Self::push_wrapperfn(this, block, false)
    }

    // To retain the option of having already taken a lock before this call the self was exchanged.
    // This is a bit ugly but since push does only really need &mut self briefly at the end for the actual write
    // while needing &self for the majority it made sense to use upgradable read instead of self.
    // Note that there can always only ever be at most one RwLockUpgradableRead thus the push calls are also
    // sequentialized by it.
    /// Pushes a block into the chain.
    /// The trusted version of the push function will skip some verifications that can only be skipped if
    /// the source is trusted. This is the case of a validator pushing its own blocks
    pub fn trusted_push(
        this: RwLockUpgradableReadGuard<Self>,
        block: Block,
    ) -> Result<PushResult, PushError> {
        Self::push_wrapperfn(this, block, true)
    }

    fn push_wrapperfn(
        this: RwLockUpgradableReadGuard<Self>,
        block: Block,
        trust: bool,
    ) -> Result<PushResult, PushError> {
        #[cfg(not(feature = "metrics"))]
        {
            Self::do_push(this, block, trust)
        }
        #[cfg(feature = "metrics")]
        {
            let metrics = this.metrics.clone();
            let res = Self::do_push(this, block, trust);
            metrics.note_push_result(&res);
            res
        }
    }

    /// Extends the current main chain.
    fn extend(
        this: RwLockUpgradableReadGuard<Blockchain>,
        block_hash: Blake2bHash,
        mut chain_info: ChainInfo,
        mut prev_info: ChainInfo,
    ) -> Result<PushResult, PushError> {
        let mut txn = this.write_transaction();

        let block_number = this.block_number() + 1;
        let is_macro_block = policy::is_macro_block_at(block_number);
        let is_election_block = policy::is_election_block_at(block_number);

        let block_info = this.check_and_commit(&this.state, &chain_info.head, &mut txn);
        let block_log = match block_info {
            Ok(block_info) => block_info,
            Err(e) => {
                txn.abort();
                return Err(e);
            }
        };

        chain_info.on_main_chain = true;
        prev_info.main_chain_successor = Some(chain_info.head.hash());

        this.chain_store
            .put_chain_info(&mut txn, &block_hash, &chain_info, true);
        this.chain_store
            .put_chain_info(&mut txn, chain_info.head.parent_hash(), &prev_info, false);
        this.chain_store.set_head(&mut txn, &block_hash);

        if is_election_block {
            this.chain_store.prune_epoch(
                policy::epoch_at(block_number).saturating_sub(MAX_EPOCHS_STORED),
                &mut txn,
            );
        }

        txn.commit();

        // Upgrade the lock as late as possible.
        let mut this = RwLockUpgradableReadGuard::upgrade_untimed(this);

        if let Block::Macro(ref macro_block) = chain_info.head {
            this.state.macro_info = chain_info.clone();
            this.state.macro_head_hash = block_hash.clone();

            if is_election_block {
                this.state.election_head = macro_block.clone();
                this.state.election_head_hash = block_hash.clone();

                let old_slots = this.state.current_slots.take().unwrap();
                this.state.previous_slots.replace(old_slots);

                let new_slots = macro_block.get_validators().unwrap();
                this.state.current_slots.replace(new_slots);
            }
        }

        this.state.main_chain = chain_info;
        this.state.head_hash = block_hash.clone();

        // Downgrade the lock again as the notify listeners might want to acquire read access themselves.
        let this = RwLockWriteGuard::downgrade_to_upgradable(this);

        let num_transactions = this.state.main_chain.head.num_transactions();
        #[cfg(feature = "metrics")]
        this.metrics.note_extend(num_transactions);
        debug!(
            block = %this.state.main_chain.head,
            num_transactions,
            kind = "extend",
            "Accepted block",
        );

        if is_election_block {
            this.notifier
                .notify(BlockchainEvent::EpochFinalized(block_hash));
        } else if is_macro_block {
            this.notifier.notify(BlockchainEvent::Finalized(block_hash));
        } else {
            this.notifier.notify(BlockchainEvent::Extended(block_hash));
        }

        this.log_notifier.notify(block_log);

        Ok(PushResult::Extended)
    }

    /// Rebranches the current main chain.
    fn rebranch(
        this: RwLockUpgradableReadGuard<Blockchain>,
        block_hash: Blake2bHash,
        chain_info: ChainInfo,
    ) -> Result<PushResult, PushError> {
        let target_block = chain_info.head.header();
        debug!(block = %target_block, "Rebranching");

        // Find the common ancestor between our current main chain and the fork chain.
        // Walk up the fork chain until we find a block that is part of the main chain.
        // Store the chain along the way.
        let read_txn = this.read_transaction();

        let mut fork_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut current: (Blake2bHash, ChainInfo) = (block_hash, chain_info);

        while !current.1.on_main_chain {
            let prev_hash = current.1.head.parent_hash().clone();

            let prev_info = this
                .chain_store
                .get_chain_info(&prev_hash, true, Some(&read_txn))
                .expect("Corrupted store: Failed to find fork predecessor while rebranching");

            fork_chain.push(current);

            current = (prev_hash, prev_info);
        }
        read_txn.close();

        debug!(
            block = %target_block,
            common_ancestor = %current.1.head,
            no_blocks_up = fork_chain.len(),
            "Found common ancestor",
        );

        // Revert AccountsTree & TransactionCache to the common ancestor state.
        let mut revert_chain: Vec<(Blake2bHash, ChainInfo)> = vec![];
        let mut ancestor = current;

        // Check if ancestor is in current batch.
        if ancestor.1.head.block_number() < this.state.macro_info.head.block_number() {
            warn!(
                block = %target_block,
                reason = "ancestor block already finalized",
                ancestor_block = %ancestor.1.head,
                "Rejecting block",
            );
            return Err(PushError::InvalidFork);
        }

        let mut write_txn = this.write_transaction();

        current = (this.state.head_hash.clone(), this.state.main_chain.clone());
        let mut block_logs = Vec::new();

        while current.0 != ancestor.0 {
            let block = current.1.head.clone();
            if block.is_macro() {
                panic!("Trying to rebranch across macro block");
            }

            let prev_hash = block.parent_hash().clone();

            let prev_info = this
                .chain_store
                .get_chain_info(&prev_hash, true, Some(&write_txn))
                .expect("Corrupted store: Failed to find main chain predecessor while rebranching");

            block_logs.push(this.revert_accounts(&this.state.accounts, &mut write_txn, &block)?);

            assert_eq!(
                prev_info.head.state_root(),
                &this.state.accounts.get_root(Some(&write_txn)),
                "Failed to revert main chain while rebranching - inconsistent state"
            );

            revert_chain.push(current);

            current = (prev_hash, prev_info);
        }

        // Push each fork block.

        let mut fork_iter = fork_chain.iter().rev();

        while let Some(fork_block) = fork_iter.next() {
            let block_log = this.check_and_commit(&this.state, &fork_block.1.head, &mut write_txn);
            let block_log = match block_log {
                Ok(block_info) => block_info,
                Err(e) => {
                    warn!(
                        block = %target_block,
                        reason = "failed to apply for block while rebranching",
                        fork_block = %fork_block.1.head,
                        error = &e as &dyn Error,
                        "Rejecting block",
                    );
                    write_txn.abort();

                    // Delete invalid fork blocks from store.
                    let mut write_txn = this.write_transaction();
                    for block in vec![fork_block].into_iter().chain(fork_iter) {
                        this.chain_store.remove_chain_info(
                            &mut write_txn,
                            &block.0,
                            fork_block.1.head.block_number(),
                        )
                    }
                    write_txn.commit();

                    return Err(PushError::InvalidFork);
                }
            };
            block_logs.push(block_log);
        }

        // Unset onMainChain flag / mainChainSuccessor on the current main chain up to (excluding) the common ancestor.
        for reverted_block in revert_chain.iter_mut() {
            reverted_block.1.on_main_chain = false;
            reverted_block.1.main_chain_successor = None;

            this.chain_store.put_chain_info(
                &mut write_txn,
                &reverted_block.0,
                &reverted_block.1,
                false,
            );
        }

        // Update the mainChainSuccessor of the common ancestor block.
        ancestor.1.main_chain_successor = Some(fork_chain.last().unwrap().0.clone());
        this.chain_store
            .put_chain_info(&mut write_txn, &ancestor.0, &ancestor.1, false);

        // Set onMainChain flag / mainChainSuccessor on the fork.
        for i in (0..fork_chain.len()).rev() {
            let main_chain_successor = if i > 0 {
                Some(fork_chain[i - 1].0.clone())
            } else {
                None
            };

            let fork_block = &mut fork_chain[i];
            fork_block.1.on_main_chain = true;
            fork_block.1.main_chain_successor = main_chain_successor;

            // Include the body of the new block (at position 0).
            this.chain_store
                .put_chain_info(&mut write_txn, &fork_block.0, &fork_block.1, i == 0);
        }

        // Commit transaction & update head.
        let new_head_hash = &fork_chain[0].0;
        let new_head_info = &fork_chain[0].1;
        this.chain_store.set_head(&mut write_txn, new_head_hash);
        write_txn.commit();

        // Upgrade the lock as late as possible.
        let mut this = RwLockUpgradableReadGuard::upgrade(this);

        if let Block::Macro(ref macro_block) = new_head_info.head {
            this.state.macro_info = new_head_info.clone();
            this.state.macro_head_hash = new_head_hash.clone();

            if policy::is_election_block_at(new_head_info.head.block_number()) {
                this.state.election_head = macro_block.clone();
                this.state.election_head_hash = new_head_hash.clone();

                let old_slots = this.state.current_slots.take().unwrap();
                this.state.previous_slots.replace(old_slots);

                let new_slots = macro_block.get_validators().unwrap();
                this.state.current_slots.replace(new_slots);
            }
        }

        this.state.main_chain = new_head_info.clone();
        this.state.head_hash = new_head_hash.clone();

        // Downgrade the lock again as the notified listeners might want to acquire read themselves.
        let this = RwLockWriteGuard::downgrade_to_upgradable(this);

        let mut reverted_blocks = Vec::with_capacity(revert_chain.len());
        for (hash, chain_info) in revert_chain.into_iter().rev() {
            debug!(
                block = %chain_info.head,
                num_transactions = chain_info.head.num_transactions(),
                "Reverted block",
            );
            reverted_blocks.push((hash, chain_info.head));
        }

        let mut adopted_blocks = Vec::with_capacity(fork_chain.len());
        for (hash, chain_info) in fork_chain.into_iter().rev() {
            debug!(
                block = %chain_info.head,
                num_transactions = chain_info.head.num_transactions(),
                kind = "rebranch",
                "Accepted block",
            );
            adopted_blocks.push((hash, chain_info.head));
        }

        debug!(
            block = %this.state.main_chain.head,
            num_reverted_blocks = reverted_blocks.len(),
            num_adopted_blocks = adopted_blocks.len(),
            "Rebranched",
        );
        #[cfg(feature = "metrics")]
        this.metrics
            .note_rebranch(&reverted_blocks, &adopted_blocks);

        let event = BlockchainEvent::Rebranched(reverted_blocks, adopted_blocks);
        this.notifier.notify(event);

        this.log_notifier.notify_vec(block_logs);

        Ok(PushResult::Rebranched)
    }

    fn check_and_commit(
        &self,
        state: &BlockchainState,
        block: &Block,
        txn: &mut WriteTransaction,
    ) -> Result<BlockLog, PushError> {
        // Check transactions against replay attacks. This is only necessary for micro blocks.
        if block.is_micro() {
            let transactions = block.transactions();

            if let Some(tx_vec) = transactions {
                for transaction in tx_vec {
                    let tx_hash = transaction.get_raw_transaction().hash();
                    if self.contains_tx_in_validity_window(&tx_hash, Some(txn)) {
                        warn!(
                            %block,
                            reason = "transaction already included",
                            transaction_hash = %tx_hash,
                            "Rejecting block",
                        );
                        return Err(PushError::DuplicateTransaction);
                    }
                }
            }
        }

        // Commit block to AccountsTree.
        let block_log = self.commit_accounts(state, block, txn);
        if let Err(e) = block_log {
            warn!(%block, reason = "commit failed", error = &e as &dyn Error, "Rejecting block");
            #[cfg(feature = "metrics")]
            self.metrics.note_invalid_block();
            return Err(e);
        }

        // Verify the state against the block.
        if let Err(e) = self.verify_block_state(state, block, Some(txn)) {
            warn!(%block, reason = "bad state", error = &e as &dyn Error, "Rejecting block");
            return Err(e);
        }

        block_log
    }
}
