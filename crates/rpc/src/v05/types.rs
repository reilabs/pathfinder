use crate::felt::RpcFelt;
use pathfinder_common::GasPrice;
use pathfinder_common::{
    BlockHash, BlockNumber, BlockTimestamp, SequencerAddress, StarknetVersion, StateCommitment,
};
use serde::Serialize;
use serde_with::{serde_as, skip_serializing_none, DisplayFromStr};

#[serde_as]
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ResourcePrice {
    #[serde_as(as = "pathfinder_serde::GasPriceAsHexStr")]
    pub price_in_wei: GasPrice,
}

impl From<GasPrice> for ResourcePrice {
    fn from(price: GasPrice) -> Self {
        Self {
            price_in_wei: price,
        }
    }
}

#[serde_as]
#[skip_serializing_none]
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct BlockHeader {
    #[serde_as(as = "Option<RpcFelt>")]
    pub block_hash: Option<BlockHash>,
    #[serde_as(as = "RpcFelt")]
    pub parent_hash: BlockHash,
    pub block_number: Option<BlockNumber>,
    #[serde_as(as = "Option<RpcFelt>")]
    pub new_root: Option<StateCommitment>,
    pub timestamp: BlockTimestamp,
    #[serde_as(as = "RpcFelt")]
    pub sequencer_address: SequencerAddress,
    pub l1_gas_price: ResourcePrice,
    #[serde_as(as = "DisplayFromStr")]
    pub starknet_version: StarknetVersion,
}

impl From<pathfinder_common::BlockHeader> for BlockHeader {
    fn from(header: pathfinder_common::BlockHeader) -> Self {
        Self {
            block_hash: Some(header.hash),
            parent_hash: header.parent_hash,
            block_number: Some(header.number),
            new_root: Some(header.state_commitment),
            timestamp: header.timestamp,
            sequencer_address: header.sequencer_address,
            l1_gas_price: header.eth_l1_gas_price.into(),
            starknet_version: header.starknet_version,
        }
    }
}

impl BlockHeader {
    /// Constructs [BlockHeader] from [sequencer's pending block representation](starknet_gateway_types::reply::PendingBlock)
    pub fn from_sequencer_pending(pending: starknet_gateway_types::reply::PendingBlock) -> Self {
        Self {
            block_hash: None,
            parent_hash: pending.parent_hash,
            block_number: None,
            new_root: None,
            timestamp: pending.timestamp,
            sequencer_address: pending.sequencer_address,
            l1_gas_price: pending.l1_gas_price.price_in_wei.into(),
            starknet_version: pending.starknet_version,
        }
    }
}
