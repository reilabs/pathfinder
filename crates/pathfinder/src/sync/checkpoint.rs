#![allow(dead_code, unused_variables)]
use anyhow::Context;
use futures::StreamExt;
use futures::TryStreamExt;
use p2p::client::peer_agnostic::EventsForBlockByTransaction;
use p2p::client::{conv::TryFromDto, peer_agnostic::Client as P2PClient};
use p2p::PeerData;
use p2p_proto::{
    common::{BlockNumberOrHash, Direction, Iteration},
    receipt::{ReceiptsRequest, ReceiptsResponse},
    transaction::{TransactionsRequest, TransactionsResponse},
};
use pathfinder_common::{
    receipt::Receipt, transaction::Transaction, BlockHash, BlockHeader, BlockNumber,
    TransactionIndex,
};
use pathfinder_ethereum::EthereumStateUpdate;
use pathfinder_storage::Storage;
use primitive_types::H160;
use tokio::task::spawn_blocking;

use crate::state::block_hash::{
    calculate_transaction_commitment, TransactionCommitmentFinalHashType,
};

use crate::sync::class_definitions;
use crate::sync::error::SyncError;
use crate::sync::events;
use crate::sync::headers;
use crate::sync::receipts;
use crate::sync::state_updates;
use crate::sync::transactions;

/// Provides P2P sync capability for blocks secured by L1.
#[derive(Clone)]
pub struct Sync {
    pub storage: Storage,
    pub p2p: P2PClient,
    // TODO: merge these two inside the client.
    pub eth_client: pathfinder_ethereum::EthereumClient,
    pub eth_address: H160,
}

impl Sync {
    pub fn new(
        storage: Storage,
        p2p: P2PClient,
        ethereum: (pathfinder_ethereum::EthereumClient, H160),
    ) -> Self {
        Self {
            storage,
            p2p,
            eth_client: ethereum.0,
            eth_address: ethereum.1,
        }
    }

    /// Syncs using p2p until the given Ethereum checkpoint.
    pub async fn run(&self, checkpoint: EthereumStateUpdate) -> Result<(), SyncError> {
        use pathfinder_ethereum::EthereumApi;

        let local_state = LocalState::from_db(self.storage.clone(), checkpoint.clone())
            .await
            .context("Querying local state")?;

        // Ensure our local state is consistent with the L1 checkpoint.
        CheckpointAnalysis::analyse(&local_state, &checkpoint)
            .handle(self.storage.clone())
            .await
            .context("Analysing local storage against L1 checkpoint")?;

        // Persist checkpoint as new L1 anchor. This must be done first to protect against an interrupted sync process.
        // Subsequent syncs will use this value to rollback against. Persisting it later would result in more data than
        // necessary being rolled back (potentially all data if the header sync process is frequently interrupted), so this
        // ensures sync will progress even under bad conditions.
        let anchor = checkpoint;
        persist_anchor(self.storage.clone(), anchor.clone())
            .await
            .context("Persisting new Ethereum anchor")?;

        let head = anchor.block_number;

        // Sync missing headers in reverse chronological order, from the new anchor to genesis.
        self.sync_headers(anchor).await?;

        // Sync the rest of the data in chronological order.
        self.sync_transactions(head).await?;
        self.sync_state_updates(head).await?;
        self.sync_class_definitions(head).await?;
        self.sync_events(head).await?;

        Ok(())
    }

    /// Syncs all headers in reverse chronological order, from the anchor point
    /// back to genesis. Fills in any gaps left by previous header syncs.
    ///
    /// As sync goes backwards from a known L1 anchor block, this method can
    /// guarantee that all sync'd headers are secured by L1.
    ///
    /// No guarantees are made about any headers newer than the anchor.
    async fn sync_headers(&self, anchor: EthereumStateUpdate) -> Result<(), SyncError> {
        while let Some(gap) =
            headers::next_gap(self.storage.clone(), anchor.block_number, anchor.block_hash)
                .await
                .context("Finding next gap in header chain")?
        {
            // TODO: create a tracing scope for this gap start, stop.

            tracing::info!("Syncing headers");

            // TODO: consider .inspect_ok(tracing::trace!) for each stage.
            self.p2p
                .clone()
                // TODO: consider buffering in the client to reduce request latency.
                .header_stream(gap.head, gap.tail, true)
                .scan((gap.head, gap.head_hash, false), headers::check_continuity)
                // TODO: rayon scope this.
                .and_then(headers::verify)
                // chunk so that persisting to storage can be batched.
                .try_chunks(1024)
                // TODO: Pull out remaining data from try_chunks error.
                //       try_chunks::Error is a tuple of Err(data, error) so we
                //       should re-stream that as Ok(data), Err(error). Right now
                //       we just map to Err(error).
                .map_err(|e| e.1)
                .and_then(|x| headers::persist(x, self.storage.clone()))
                .inspect_ok(|x| tracing::info!(tail=%x.data.header.number, "Header chunk synced"))
                // Drive stream to completion.
                .try_fold((), |_state, _x| std::future::ready(Ok(())))
                .await?;
        }

        Ok(())
    }

    async fn sync_transactions(&self, stop: BlockNumber) -> Result<(), SyncError> {
        if let Some(start) = transactions::next_missing(self.storage.clone(), stop)
            .await
            .context("Finding next block with missing transaction(s)")?
        {
            self.p2p
                .clone()
                .transactions_stream(
                    start,
                    stop,
                    transactions::counts_stream(self.storage.clone(), start, stop),
                )
                .map_err(Into::into)
                .and_then(|x| async { Ok(x) })
                .and_then(|x| transactions::verify_commitment(x, self.storage.clone()))
                .try_chunks(100)
                .map_err(|e| e.1)
                .and_then(|x| transactions::persist(self.storage.clone(), x))
                .inspect_ok(|x| tracing::info!(tail=%x, "Transactions chunk synced"))
                // Drive stream to completion.
                .try_fold((), |_, _| std::future::ready(Ok(())))
                .await?;
        }

        Ok(())
    }

    async fn sync_receipts(&self) -> anyhow::Result<()> {
        let (first_block, last_block) = spawn_blocking({
            let storage = self.storage.clone();
            move || -> anyhow::Result<(Option<BlockNumber>, Option<BlockNumber>)> {
                let mut db = storage
                    .connection()
                    .context("Creating database connection")?;
                let db = db.transaction().context("Creating database transaction")?;
                let first_block = db
                    .first_block_without_receipts()
                    .context("Querying first block without receipts")?;
                let last_block = db
                    .block_id(pathfinder_storage::BlockId::Latest)
                    .context("Querying latest block without receipts")?
                    .map(|(block_number, _)| block_number);
                Ok((first_block, last_block))
            }
        })
        .await
        .context("Joining blocking task")??;

        let Some(first_block) = first_block else {
            return Ok(());
        };
        let last_block = last_block.context("Last block not found but first block found")?;

        let mut curr_block = headers::query(self.storage.clone(), first_block)
            .await?
            .ok_or_else(|| anyhow::anyhow!("First block not found"))?;

        // Loop which refreshes peer set once we exhaust it.
        loop {
            let peers = self
                .p2p
                .get_update_peers_with_transaction_sync_capability()
                .await;

            // Attempt each peer.
            'next_peer: for peer in peers {
                let request = ReceiptsRequest {
                    iteration: Iteration {
                        start: BlockNumberOrHash::Number(curr_block.number.get()),
                        direction: Direction::Forward,
                        limit: last_block.get() - curr_block.number.get() + 1,
                        step: 1.into(),
                    },
                };

                let mut responses = match self.p2p.send_receipts_sync_request(peer, request).await {
                    Ok(x) => x,
                    Err(error) => {
                        // Failed to establish connection, try next peer.
                        tracing::debug!(%peer, reason=%error, "Receipts request failed");
                        continue 'next_peer;
                    }
                };

                let mut receipts = Vec::new();
                while let Some(receipt) = responses.next().await {
                    match receipt {
                        ReceiptsResponse::Receipt(receipt) => {
                            match Receipt::try_from_dto((
                                receipt,
                                TransactionIndex::new_or_panic(receipts.len().try_into().unwrap()),
                            )) {
                                Ok(receipt) if receipts.len() < curr_block.transaction_count => {
                                    receipts.push(receipt)
                                }
                                Ok(receipt) => {
                                    receipts::persist(
                                        self.storage.clone(),
                                        curr_block.clone(),
                                        receipts.clone(),
                                    )
                                    .await
                                    .context("Inserting receipts")?;
                                    if curr_block.number == last_block {
                                        return Ok(());
                                    }
                                    curr_block =
                                        headers::query(self.storage.clone(), curr_block.number + 1)
                                            .await?
                                            .ok_or_else(|| {
                                                anyhow::anyhow!("Next block not found")
                                            })?;
                                    receipts.clear();
                                    receipts.push(receipt);
                                }
                                Err(error) => {
                                    tracing::debug!(%peer, %error, "Receipt stream returned unexpected DTO");
                                    continue 'next_peer;
                                }
                            }
                        }
                        ReceiptsResponse::Fin if curr_block.number == last_block => {
                            if receipts.len() != curr_block.transaction_count {
                                tracing::debug!(
                                    "Invalid receipts for block {}, trying next peer",
                                    curr_block.number
                                );
                                continue 'next_peer;
                            }
                            receipts::persist(self.storage.clone(), curr_block, receipts)
                                .await
                                .context("Inserting receipts")?;
                            return Ok(());
                        }
                        ReceiptsResponse::Fin => {
                            tracing::debug!(%peer, "Unexpected receipts stream Fin");
                            continue 'next_peer;
                        }
                    };
                }
            }
        }
    }

    async fn sync_state_updates(&self, stop: BlockNumber) -> Result<(), SyncError> {
        if let Some(start) = state_updates::next_missing(self.storage.clone(), stop)
            .await
            .context("Finding next missing state update")?
        {
            self.p2p
                .clone()
                .contract_updates_stream(
                    start,
                    stop,
                    state_updates::contract_update_counts_stream(self.storage.clone(), start, stop),
                )
                .map_err(Into::into)
                .and_then(state_updates::verify_signature)
                .try_chunks(100)
                .map_err(|e| e.1)
                // Persist state updates (without: state commitments and declared classes)
                .and_then(|x| state_updates::persist(self.storage.clone(), x))
                .inspect_ok(|x| tracing::info!(tail=%x, "State update chunk synced"))
                // Drive stream to completion.
                .try_fold((), |_, _| std::future::ready(Ok(())))
                .await?;
        }

        Ok(())
    }

    async fn sync_class_definitions(&self, stop: BlockNumber) -> Result<(), SyncError> {
        if let Some(start) = class_definitions::next_missing(self.storage.clone(), stop)
            .await
            .context("Finding next block with missing class definition(s)")?
        {
            self.p2p
                .clone()
                .class_definitions_stream(
                    start,
                    stop,
                    class_definitions::declared_class_counts_stream(
                        self.storage.clone(),
                        start,
                        stop,
                    ),
                )
                .map_err(Into::into)
                .map_ok(class_definitions::verify_layout)
                .and_then(std::future::ready)
                .and_then(class_definitions::verify_hash)
                .try_chunks(1000)
                .map_err(|e| e.1)
                .and_then(|x| class_definitions::persist(self.storage.clone(), x))
                .inspect_ok(|x| tracing::info!(tail=%x, "Class definitions chunk synced"))
                // Drive stream to completion.
                .try_fold((), |_, _| std::future::ready(Ok(())))
                .await?;
        }

        Ok(())
    }

    async fn sync_events(&self, stop: BlockNumber) -> Result<(), SyncError> {
        let Some(start) = events::next_missing(self.storage.clone(), stop)
            .await
            .context("Finding next block with missing events")?
        else {
            return Ok(());
        };

        let event_stream = self.p2p.clone().events_stream(
            start,
            stop,
            events::counts_stream(self.storage.clone(), start, stop),
        );

        handle_events_stream(event_stream, self.storage.clone()).await?;

        Ok(())
    }
}

async fn handle_events_stream(
    event_stream: impl futures::Stream<Item = anyhow::Result<PeerData<EventsForBlockByTransaction>>>,
    storage: Storage,
) -> Result<(), SyncError> {
    event_stream
        .map_err(Into::into)
        .and_then(|x| events::verify_commitment(x, storage.clone()))
        .try_chunks(100)
        .map_err(|e| e.1)
        .and_then(|x| events::persist(storage.clone(), x))
        .inspect_ok(|x| tracing::info!(tail=%x, "Events chunk synced"))
        // Drive stream to completion.
        .try_fold((), |_, _| std::future::ready(Ok(())))
        .await?;
    Ok(())
}

async fn check_transactions(
    block: &BlockHeader,
    transactions: &[Transaction],
) -> anyhow::Result<bool> {
    if transactions.len() != block.transaction_count {
        return Ok(false);
    }
    let transaction_final_hash_type =
        TransactionCommitmentFinalHashType::for_version(&block.starknet_version);
    let transaction_commitment = spawn_blocking({
        let transactions = transactions.to_vec();
        move || {
            calculate_transaction_commitment(&transactions, transaction_final_hash_type)
                .map_err(anyhow::Error::from)
        }
    })
    .await
    .context("Joining blocking task")?
    .context("Calculating transaction commitment")?;
    Ok(transaction_commitment == block.transaction_commitment)
}

/// Performs [analysis](Self::analyse) of the [LocalState] by comparing it with a given L1 checkpoint,
/// and [handles](Self::handle) the result.
enum CheckpointAnalysis {
    /// The checkpoint hash does not match the local L1 anchor, indicating an inconsistency with the Ethereum source
    /// with the one used by the previous sync.
    HashMismatchWithAnchor {
        block: BlockNumber,
        checkpoint: BlockHash,
        anchor: BlockHash,
    },
    /// The checkpoint is older than the local anchor, indicating an inconsistency in the Ethereum source between
    /// this sync and the previous sync.
    PredatesAnchor {
        checkpoint: BlockNumber,
        anchor: BlockNumber,
    },
    /// The checkpoint exceeds the local chain. As such, the local chain should be rolled back to its anchor as we
    /// cannot be confident in any of the local data not verified by L1.
    ExceedsLocalChain {
        local: BlockNumber,
        checkpoint: BlockNumber,
        anchor: Option<BlockNumber>,
    },
    /// The checkpoint hash does not match the local chain data. The local chain should be rolled back to its anchor.
    HashMismatchWithLocalChain {
        block: BlockNumber,
        local: BlockHash,
        checkpoint: BlockHash,
        anchor: Option<BlockNumber>,
    },
    /// Local data is consistent with the checkpoint, no action required.
    Consistent,
}

impl CheckpointAnalysis {
    /// Analyse [LocalState] by checking it for consistency against the given L1 checkpoint.
    ///
    /// For more information on the potential inconsistencies see the [CheckpointAnalysis] variants.
    fn analyse(local_state: &LocalState, checkpoint: &EthereumStateUpdate) -> CheckpointAnalysis {
        // Checkpoint is older than or inconsistent with our local L1 anchor.
        if let Some(anchor) = &local_state.anchor {
            if checkpoint.block_number < anchor.block_number {
                return CheckpointAnalysis::PredatesAnchor {
                    checkpoint: checkpoint.block_number,
                    anchor: anchor.block_number,
                };
            }
            if checkpoint.block_number == anchor.block_number
                && checkpoint.block_hash != anchor.block_hash
            {
                return CheckpointAnalysis::HashMismatchWithAnchor {
                    block: anchor.block_number,
                    checkpoint: checkpoint.block_hash,
                    anchor: anchor.block_hash,
                };
            }
        }

        // Is local data not secured by an anchor potentially invalid?
        if let Some(latest) = local_state.latest_header {
            if checkpoint.block_number > latest.0 {
                return CheckpointAnalysis::ExceedsLocalChain {
                    local: latest.0,
                    checkpoint: checkpoint.block_number,
                    anchor: local_state.anchor.as_ref().map(|x| x.block_number),
                };
            }
            if let Some((_, hash)) = local_state.checkpoint {
                if hash != checkpoint.block_hash {
                    return CheckpointAnalysis::HashMismatchWithLocalChain {
                        block: checkpoint.block_number,
                        local: hash,
                        checkpoint: checkpoint.block_hash,
                        anchor: local_state.anchor.as_ref().map(|x| x.block_number),
                    };
                }
            }
        }

        CheckpointAnalysis::Consistent
    }

    /// Handles the [checkpoint analysis](Self::analyse) [result](Self).
    ///
    /// Returns an error for [PredatesAnchor](Self::PredatesAnchor) and
    /// [HashMismatchWithAnchor](Self::HashMismatchWithAnchor) since these indicate an inconsistency with the Ethereum
    /// source - making all data suspect.
    ///
    /// Rolls back local state to the anchor for [ExceedsLocalChain](Self::ExceedsLocalChain) and
    /// [HashMismatchWithLocalChain](Self::HashMismatchWithLocalChain) conditions.
    ///
    /// Does nothing for [Consistent](Self::Consistent). This leaves any insecure local data intact. Always rolling
    /// back to the L1 anchor would result in a poor user experience if restarting frequently as each restart would
    /// purge new data.
    async fn handle(self, storage: Storage) -> anyhow::Result<()> {
        match self {
            CheckpointAnalysis::HashMismatchWithAnchor {
                block,
                checkpoint,
                anchor,
            } => {
                tracing::error!(
                    %block, %checkpoint, %anchor,
                    "Ethereum checkpoint's hash did not match the local Ethereum anchor. This indicates a serious inconsistency in the Ethereum source used by this sync and the previous sync."
                );
                anyhow::bail!("Ethereum checkpoint hash did not match local anchor.");
            }
            CheckpointAnalysis::PredatesAnchor { checkpoint, anchor } => {
                // TODO: or consider this valid. If so, then we should continue sync but use the local anchor instead of the checkpoint.
                tracing::error!(
                    %checkpoint, %anchor,
                    "Ethereum checkpoint is older than the local anchor. This indicates a serious inconsistency in the Ethereum source used by this sync and the previous sync."
                );
                anyhow::bail!("Ethereum checkpoint hash did not match local anchor.");
            }
            CheckpointAnalysis::ExceedsLocalChain {
                local,
                checkpoint,
                anchor,
            } => {
                tracing::info!(
                    %local, anchor=%anchor.unwrap_or_default(), %checkpoint,
                    "Rolling back local chain to latest anchor point. Local data is potentially invalid as the Ethereum checkpoint is newer the local chain."
                );
                rollback_to_anchor(storage, anchor)
                    .await
                    .context("Rolling back chain state to L1 anchor")?;
            }
            CheckpointAnalysis::HashMismatchWithLocalChain {
                block,
                local,
                checkpoint,
                anchor,
            } => {
                tracing::info!(
                    %block, %local, %checkpoint, ?anchor,
                    "Rolling back local chain to latest anchor point. Local data is invalid as it did not match the Ethereum checkpoint's hash."
                );
                rollback_to_anchor(storage, anchor)
                    .await
                    .context("Rolling back chain state to L1 anchor")?;
            }
            CheckpointAnalysis::Consistent => {
                tracing::info!("Ethereum checkpoint is consistent with local data");
            }
        };

        Ok(())
    }
}

struct LocalState {
    latest_header: Option<(BlockNumber, BlockHash)>,
    anchor: Option<EthereumStateUpdate>,
    checkpoint: Option<(BlockNumber, BlockHash)>,
}

impl LocalState {
    async fn from_db(storage: Storage, checkpoint: EthereumStateUpdate) -> anyhow::Result<Self> {
        // TODO: this should include header gaps.
        spawn_blocking(move || {
            let mut db = storage
                .connection()
                .context("Creating database connection")?;
            let db = db.transaction().context("Creating database transaction")?;

            let latest_header = db
                .block_id(pathfinder_storage::BlockId::Latest)
                .context("Querying latest header")?;

            let checkpoint = db
                .block_id(checkpoint.block_number.into())
                .context("Querying checkpoint header")?;

            let anchor = db.latest_l1_state().context("Querying latest L1 anchor")?;

            Ok(LocalState {
                latest_header,
                checkpoint,
                anchor,
            })
        })
        .await
        .context("Joining blocking task")?
    }
}

/// Rolls back local chain-state until the given anchor point, making it the tip of the local chain. If this is ['None']
/// then all data will be rolled back.
async fn rollback_to_anchor(storage: Storage, anchor: Option<BlockNumber>) -> anyhow::Result<()> {
    spawn_blocking(move || {
        todo!("Rollback storage to anchor point");
    })
    .await
    .context("Joining blocking task")?
}

async fn persist_anchor(storage: Storage, anchor: EthereumStateUpdate) -> anyhow::Result<()> {
    spawn_blocking(move || {
        let mut db = storage
            .connection()
            .context("Creating database connection")?;
        let db = db.transaction().context("Creating database transaction")?;
        db.upsert_l1_state(&anchor).context("Inserting anchor")?;
        // TODO: this is a bit dodgy, but is used by the sync process. However it destroys
        //       some RPC assumptions which we should be aware of.
        db.update_l1_l2_pointer(Some(anchor.block_number))
            .context("Updating L1-L2 pointer")?;
        db.commit().context("Committing database transaction")?;
        Ok(())
    })
    .await
    .context("Joining blocking task")?
}

#[cfg(test)]
mod tests {
    use super::*;

    mod handle_events_stream {
        use crate::state::block_hash::calculate_event_commitment;

        use super::super::handle_events_stream;
        use super::*;
        use fake::{Fake, Faker};
        use futures::stream;
        use p2p::libp2p::PeerId;
        use pathfinder_common::{event::Event, transaction::TransactionVariant, TransactionHash};
        use pathfinder_crypto::Felt;
        use pathfinder_storage::fake as fake_storage;
        use pathfinder_storage::{StorageBuilder, TransactionData};

        struct Setup {
            pub streamed_events: Vec<anyhow::Result<PeerData<EventsForBlockByTransaction>>>,
            pub expected_events: Vec<Vec<(TransactionHash, Vec<Event>)>>,
            pub storage: Storage,
        }

        async fn setup(num_blocks: usize, compute_event_commitments: bool) -> Setup {
            tokio::task::spawn_blocking(move || {
                let mut blocks = fake_storage::init::with_n_blocks(num_blocks);
                let streamed_events = blocks
                    .iter()
                    .map(|block| {
                        anyhow::Result::Ok(PeerData::for_tests((
                            block.header.header.number,
                            block
                                .transaction_data
                                .iter()
                                .map(|x| x.2.clone())
                                .collect::<Vec<_>>(),
                        )))
                    })
                    .collect::<Vec<_>>();
                let expected_events = blocks
                    .iter()
                    .map(|block| {
                        block
                            .transaction_data
                            .iter()
                            .map(|x| (x.0.hash, x.2.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();

                let storage = StorageBuilder::in_memory().unwrap();
                blocks.iter_mut().for_each(|block| {
                    if compute_event_commitments {
                        block.header.header.event_commitment = calculate_event_commitment(
                            &block
                                .transaction_data
                                .iter()
                                .flat_map(|(_, _, events)| events)
                                .collect::<Vec<_>>(),
                        )
                        .unwrap();
                    }
                    // Purge events
                    block
                        .transaction_data
                        .iter_mut()
                        .for_each(|(_, _, events)| events.clear())
                });
                fake_storage::fill(&storage, &blocks);
                Setup {
                    streamed_events,
                    expected_events,
                    storage,
                }
            })
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn happy_path() {
            const NUM_BLOCKS: usize = 10;
            let Setup {
                streamed_events,
                expected_events,
                storage,
            } = setup(NUM_BLOCKS, true).await;

            handle_events_stream(stream::iter(streamed_events), storage.clone())
                .await
                .unwrap();

            let actual_events = tokio::task::spawn_blocking(move || {
                let mut conn = storage.connection().unwrap();
                let db_tx = conn.transaction().unwrap();
                (0..NUM_BLOCKS)
                    .map(|n| {
                        db_tx
                            .events_for_block(BlockNumber::new_or_panic(n as u64).into())
                            .unwrap()
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap();

            pretty_assertions_sorted::assert_eq!(expected_events, actual_events);
        }

        #[tokio::test]
        async fn commitment_mismatch() {
            const NUM_BLOCKS: usize = 1;
            let Setup {
                streamed_events,
                expected_events,
                storage,
            } = setup(NUM_BLOCKS, false).await;
            let expected_peer_id = streamed_events[0].as_ref().unwrap().peer;

            assert_matches::assert_matches!(
                handle_events_stream(stream::iter(streamed_events), storage.clone())
                    .await
                    .unwrap_err(),
                SyncError::EventCommitmentMismatch(expected_peer_id)
            );
        }

        #[tokio::test]
        async fn stream_failure() {
            assert_matches::assert_matches!(
                handle_events_stream(
                    stream::once(std::future::ready(Err(anyhow::anyhow!("")))),
                    StorageBuilder::in_memory().unwrap()
                )
                .await
                .unwrap_err(),
                SyncError::Other(_)
            );
        }

        #[tokio::test]
        async fn header_missing() {
            assert_matches::assert_matches!(
                handle_events_stream(
                    stream::once(std::future::ready(Ok(Faker.fake()))),
                    StorageBuilder::in_memory().unwrap()
                )
                .await
                .unwrap_err(),
                SyncError::Other(_)
            );
        }
    }
}
