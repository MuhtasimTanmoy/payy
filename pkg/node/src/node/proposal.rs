use std::{sync::Arc, time::Instant};

use doomslug::ApprovalValidated;
use primitives::hash::CryptoHash;
use smirk::Element;
use tracing::{info, instrument, warn};

use crate::{
    block::{Block, BlockContent, BlockHeader, BlockState}, network::NetworkEvent, node::block_format::BlockMetadata, types::BlockHeight, BlockFormat, Error, Mode, NodeShared, Result
};

impl NodeShared {
    #[instrument(skip(self))]
   pub(crate) fn commit_proposal(&self, block: Block) -> Result<()> {
        let state = &block.content.state;
        let height = block.content.header.height;

        // Commit proposal
        info!(counter.commit_height = ?height, "Commit");

        // Update the last_commit time
        let commit_time = chrono::Utc::now();
        self.state.lock().last_commit = Some(Instant::now());

        // Get a list of keys to remove from the mempool
        let keys = state
            .txns
            .iter()
            .map(|txn| {
                let hash = txn.hash();
                Ok(hash)
            })
            .collect::<Result<Vec<_>>>()?;

        for txn in &block.content.state.txns {
            info!(
                hash = format!("0x{}", txn.hash()),
                recent_root = format!("0x{:x}", txn.recent_root),
                mb_hash = format!("0x{:x}", txn.mb_hash),
                mb_value = format!("0x{:x}", txn.mb_value),
                input_leaves = ?txn.input_leaves.iter().map(|l| format!("0x{l:x}")).collect::<Vec<_>>(),
                output_leaves = ?txn.output_leaves.iter().map(|l| format!("0x{l:x}")).collect::<Vec<_>>(),
                "Committing transaction"
            )
        }

        // Validate leaves before commit
        let leaves = state
            .txns
            .iter()
            .flat_map(|txn| txn.leaves())
            .filter(|e| *e != Element::ZERO);

        let skip_validation = self.config.bad_blocks.contains(&height);
        {
            for leaf in leaves {
                if !skip_validation && self.notes_tree.read().tree().contains_element(&leaf) {
                    panic!("Double-spend detected. This should never happen, this should have been caught before commit");
                }
            }
        }

        // The order of these operations is important.
        // If we exit after commiting to block store,
        // but before commiting to notes tree, we
        // can detect it by checking the previous block's root hash.
        self.block_store.set(
            &BlockFormat::V2(block.clone(), BlockMetadata {
                timestamp_unix_s: Some(commit_time.timestamp() as u64)
            }),
        )?;

        Self::apply_block_to_tree(&mut self.notes_tree.write(), state, height, skip_validation)?;

        let block = Arc::new(block);

        // Commit changes in mempool (releasing unused txns and removing used ones). This will
        // also release all requests that were waiting for these txns to be committed.
        self.mempool
            .commit(height, keys.iter().map(|k| (k, Ok(Arc::clone(&block)))).collect());

        // Notify any commit listeners
        let listeners = &mut self.state.lock().listeners;
        listeners.retain(|tx| tx.send(Arc::clone(&block)).is_ok());

        Ok(())
    } 

   #[instrument(skip(self))]
   pub fn receive_proposal(&self, block: Block) -> Result<()> {
        if self.config.mode == Mode::Validator {
            panic!("This function should not be called by the validator");
        }

        let manifest_height = block.content.header.height;
        let keys = &block
            .content
            .state
            .txns
            .iter()
            .map(|t| t.hash())
            .collect::<Vec<_>>();

        // self.solid.receive_proposal(block)?;
        self.block_cache.lock().insert(block);

        // TODO: Check if we need to do anything else now we have this proposal

        // Only add the the mempool if Solid did not report any errors,
        // otherwise a malicious node could spam us with invalid proposals,
        // preventing specific txns from ever being processed
        self.mempool.lease_txns(manifest_height, keys);

        // Notify the worker

        Ok(())
    } 


    #[instrument(skip_all, fields(height))]
    pub(crate) async fn create_proposal(
        &self,
        last_block_hash: CryptoHash,
        height: BlockHeight,
        accepts: Vec<ApprovalValidated>,
    ) -> Result<Block> {
        // Commit proposal
        info!(?height, "Propose");

        let txns = {
            // Get a list of txns
            let utxos = self
                .mempool
                .lease_batch(height, self.config.block_txns_count);

            for (_, txn) in &utxos {
                if let Err(err) = self.validate_transaction(txn).await {
                    let txn_hash = txn.hash();
                    // commit releases the other keys in the lease too
                    self.mempool.commit(height, vec![(&txn_hash, Err(err))]);

                    // If any of the transactions fail validation,
                    // return early and try other transactions in a new proposal.
                    return Err(Error::InvalidTransaction { txn: txn_hash });
                }
            }

            utxos.into_iter().map(|(_, utxo)| utxo).collect::<Vec<_>>()
        };

        let leaves = txns.iter().flat_map(|utxo| utxo.leaves()).filter(|leaf| leaf != &Element::ZERO).collect::<Vec<_>>();

        let new_root_hash = match leaves.is_empty() {
            true => {
                // Root is unchanged
                self.notes_tree.read().tree().root_hash()
            },
            false => {
                self.notes_tree.read().tree().root_hash_with(&leaves)
            }
        };

        let block_content = BlockContent {
            header: BlockHeader {
                height,
                last_block_hash,
                epoch_id: 0,
                last_final_block_hash: last_block_hash,
                approvals: accepts.into_iter().map(|a| a.signature).collect(),
            },
            state: BlockState::new(new_root_hash, txns),
        };

        // Create a signed block
        let block = block_content.to_block(&self.local_peer);

        let validate_res = self.validate_block(&block);
        match &validate_res {
            Ok(()) => {}
            Err(Error::LeafAlreadyInsertedInTheSameBlock {
                inserted_leaf,
                txn_hash,
                failing_txn_hash,
            }) => {
                let inserted_leaf = *inserted_leaf;
                let txn_hash = *txn_hash;
                let failing_txn_hash = *failing_txn_hash;

                self.mempool.commit(
                    height,
                    vec![(
                        &failing_txn_hash,
                        Err(Error::LeafAlreadyInsertedInTheSameBlock {
                            inserted_leaf,
                            txn_hash,
                            failing_txn_hash,
                        }),
                    )],
                );
            }
            Err(Error::NoteAlreadySpent {
                spent_note,
                failing_txn_hash,
            }) => {
                let spent_note = *spent_note;
                let failing_txn_hash = *failing_txn_hash;

                self.mempool.commit(
                    height,
                    vec![(
                        &failing_txn_hash,
                        Err(Error::NoteAlreadySpent {
                            spent_note,
                            failing_txn_hash,
                        }),
                    )],
                );
            }
            Err(Error::UtxoRootIsNotRecentEnough { utxo_recent_root, recent_roots, txn_hash }) => {
                let utxo_recent_root = *utxo_recent_root;
                let recent_roots = recent_roots.clone();
                let txn_hash = *txn_hash;

                self.mempool.commit(
                    height,
                    vec![(
                        &txn_hash,
                        Err(Error::UtxoRootIsNotRecentEnough {
                            utxo_recent_root,
                            recent_roots,
                            txn_hash,
                        }),
                    )],
                );
            }
            Err(_err) => {
                // One of the transactions failed, but we don't know which one.
                // Send the same error to each of them.
                let txn_iter = block.content.state.txns.iter();
                let txn_errors = txn_iter
                    .clone()
                    .map(|_tx| 
                        // This is not optimal, but it's required because Error is not clone-able.
                        self.validate_block(&block)
                    )
                    .collect::<Vec<_>>();
                let txn_keys = txn_iter
                    .map(|tx| tx.hash())
                    .collect::<Vec<_>>();

                self.mempool.commit(
                    height,
                    txn_errors
                        .into_iter()
                        .enumerate()
                        .map(|(i, err)| (&txn_keys[i], err.map(|_| unreachable!("We know the result is Err in this match branch"))))
                        .collect::<Vec<_>>(),
                );
            }
        }
        validate_res?;

        // Commit locally
        self.commit_proposal(block.clone())?;

        // Add our newly minted block to the block store
        self.block_cache.lock().insert(block.clone());
        self.block_cache.lock().confirm(height);

        // Send proposal to peers
        self.send_all(NetworkEvent::Block(block.clone())).await;

        Ok(block)
    } 
}
