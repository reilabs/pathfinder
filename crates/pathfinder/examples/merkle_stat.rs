use std::{
    collections::{HashMap, VecDeque},
    num::NonZeroU32,
    ops::ControlFlow,
};

use anyhow::Context;
use bitvec::{prelude::Msb0, slice::BitSlice, vec::BitVec, view::BitView};
use pathfinder_common::{trie::TrieNode, BlockNumber, ContractStateHash};
use pathfinder_merkle_tree::{
    merkle_node::{Direction, InternalNode},
    tree::Visit,
    ContractsStorageTree, StorageCommitmentTree,
};
use pathfinder_storage::{BlockId, JournalMode, Storage};
use stark_hash::Felt;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .compact()
        .init();

    let database_path = std::env::args().nth(1).unwrap();
    let storage = Storage::migrate(database_path.into(), JournalMode::WAL)?
        .create_pool(NonZeroU32::new(2).unwrap())?;
    let mut db = storage
        .connection()
        .context("Opening database connection")?;

    let latest_block = {
        let tx = db.transaction().unwrap();
        let (latest_block, _) = tx.block_id(BlockId::Latest)?.unwrap();
        latest_block.get()
    };

    let block_number = std::env::args()
        .nth(2)
        .map(|s| str::parse(&s).unwrap())
        .unwrap_or(latest_block);

    let tx = db.transaction().unwrap();

    let block_header = tx
        .block_header(BlockNumber::new_or_panic(block_number).into())?
        .context("Getting block header")?;

    println!(
        "Checking block number {}, storage root {}",
        block_number, block_header.storage_commitment
    );

    let storage_trie_reader = tx.storage_trie_reader();

    let mut global_nodes: HashMap<Felt, (TrieNode, usize)> = Default::default();
    let mut contract_states = Vec::new();

    let mut to_visit = VecDeque::new();
    to_visit.push_back((block_header.storage_commitment.0, BitVec::new()));
    while let Some((felt, current_path)) = to_visit.pop_front() {
        if current_path.len() == 251 {
            contract_states.push(felt);
            continue;
        }

        let node = storage_trie_reader.get(&felt)?.unwrap();
        match &node {
            TrieNode::Binary { left, right } => {
                let mut path_left: BitVec<u8, Msb0> = current_path.clone();
                path_left.push(Direction::Left.into());
                to_visit.push_back((*left, path_left));
                let mut path_right = current_path.clone();
                path_right.push(Direction::Right.into());
                to_visit.push_back((*right, path_right));
            }
            TrieNode::Edge { child, path } => {
                let mut path_edge = current_path.clone();
                path_edge.extend_from_bitslice(path);
                to_visit.push_back((*child, path_edge));
            }
        }

        global_nodes
            .entry(felt)
            .and_modify(|(_node, refcount)| *refcount += 1)
            .or_insert((node, 1));
    }

    // let mut visitor_fn = |node: &InternalNode, _path: &BitSlice<u8, Msb0>| {
    //     match node {
    //         InternalNode::Binary(node) => {
    //             let hash = node.hash.unwrap();
    //             let trie_node = TrieNode::Binary {
    //                 left: node.left.borrow().hash().unwrap(),
    //                 right: node.right.borrow().hash().unwrap(),
    //             };
    //             global_nodes
    //                 .entry(hash)
    //                 .and_modify(|(_node, refcount)| *refcount += 1)
    //                 .or_insert((trie_node, 1));
    //         }
    //         InternalNode::Edge(node) => {
    //             let hash = node.hash.unwrap();
    //             let trie_node = TrieNode::Edge {
    //                 child: node.child.borrow().hash().unwrap(),
    //                 path: node.path.clone(),
    //             };
    //             global_nodes
    //                 .entry(hash)
    //                 .and_modify(|(_node, refcount)| *refcount += 1)
    //                 .or_insert((trie_node, 1));
    //         }
    //         InternalNode::Leaf(_) | InternalNode::Unresolved(_) => {}
    //     };

    //     if let InternalNode::Leaf(felt) = node {
    //         contract_states.push(ContractStateHash(*felt));
    //     }

    //     ControlFlow::Continue::<(), Visit>(Default::default())
    // };

    // tree.dfs(&mut visitor_fn)?;

    let r = global_nodes
        .iter()
        .filter_map(|(_key, (_node, refcount))| if *refcount > 1 { Some(*refcount) } else { None })
        .count();

    println!(
        "Global tree nodes: {}, muliple references {}",
        global_nodes.len(),
        r
    );

    let contract_trie_reader = tx.contract_trie_reader();

    let mut contract_storage_nodes: HashMap<Felt, (TrieNode, usize)> = Default::default();

    for contract_index in progressed::ProgressBar::new(0..contract_states.len())
        .set_title("Checking contract state tries")
    {
        let contract_state_hash = contract_states.get(contract_index).unwrap();
        let (contract_root, _, _) = tx
            .contract_state(ContractStateHash(*contract_state_hash))?
            .context("Getting contract state")?;

        let mut to_visit = VecDeque::new();
        to_visit.push_back((contract_root.0, BitVec::new()));
        while let Some((felt, current_path)) = to_visit.pop_front() {
            if felt == Felt::ZERO {
                continue;
            }

            if current_path.len() == 251 {
                // leaf node
                continue;
            }

            let node = contract_trie_reader.get(&felt)?.unwrap();
            match &node {
                TrieNode::Binary { left, right } => {
                    let mut path_left: BitVec<u8, Msb0> = current_path.clone();
                    path_left.push(Direction::Left.into());
                    to_visit.push_back((*left, path_left));
                    let mut path_right = current_path.clone();
                    path_right.push(Direction::Right.into());
                    to_visit.push_back((*right, path_right));
                }
                TrieNode::Edge { child, path } => {
                    let mut path_edge = current_path.clone();
                    path_edge.extend_from_bitslice(path);
                    to_visit.push_back((*child, path_edge));
                }
            }

            contract_storage_nodes
                .entry(felt)
                .and_modify(|(_node, refcount)| *refcount += 1)
                .or_insert((node, 1));
        }
    }

    // for contract_index in progressed::ProgressBar::new(0..contract_states.len())
    //     .set_title("Checking contract state tries")
    // {
    //     let contract_state_hash = contract_states.get(contract_index).unwrap();
    //     let (contract_root, _, _) = tx
    //         .contract_state(*contract_state_hash)?
    //         .context("Getting contract state")?;

    //     let mut tree = ContractsStorageTree::load(&tx, contract_root);

    //     let mut visitor_fn = |node: &InternalNode, _path: &BitSlice<u8, Msb0>| {
    //         match node {
    //             InternalNode::Binary(node) => {
    //                 let hash = node.hash.unwrap();
    //                 let trie_node = TrieNode::Binary {
    //                     left: node.left.borrow().hash().unwrap(),
    //                     right: node.right.borrow().hash().unwrap(),
    //                 };
    //                 contract_storage_nodes
    //                     .entry(hash)
    //                     .and_modify(|(_node, refcount)| *refcount += 1)
    //                     .or_insert((trie_node, 1));
    //             }
    //             InternalNode::Edge(node) => {
    //                 let hash = node.hash.unwrap();
    //                 let trie_node = TrieNode::Edge {
    //                     child: node.child.borrow().hash().unwrap(),
    //                     path: node.path.clone(),
    //                 };
    //                 contract_storage_nodes
    //                     .entry(hash)
    //                     .and_modify(|(_node, refcount)| *refcount += 1)
    //                     .or_insert((trie_node, 1));
    //             }
    //             InternalNode::Leaf(_) | InternalNode::Unresolved(_) => {}
    //         };

    //         ControlFlow::Continue::<(), Visit>(Default::default())
    //     };

    //     tree.dfs(&mut visitor_fn)?;
    // }

    let r = contract_storage_nodes
        .iter()
        .filter_map(|(_key, (_node, refcount))| if *refcount > 1 { Some(*refcount) } else { None })
        .count();

    println!(
        "Contracts tree nodes: {}, multiple references {}",
        contract_storage_nodes.len(),
        r
    );

    drop(tx);

    let tx = db.rusqlite_transaction().unwrap();
    tx.execute(
        r"CREATE TABLE tree_global_new (
        hash        BLOB PRIMARY KEY,
        data        BLOB,
        ref_count   INTEGER
    )",
        [],
    )
    .unwrap();

    let batch_factor = 100_usize;
    let values_part = "(?, ?, ?),".repeat(batch_factor - 1) + "(?, ?, ?)";
    let mut batch_stmt = tx
        .prepare(&format!(
            "INSERT INTO tree_global_new (hash, data, ref_count) VALUES {}",
            values_part
        ))
        .unwrap();

    let mut stmt = tx
        .prepare("INSERT INTO tree_global_new (hash, data, ref_count) VALUES (?, ?, ?)")
        .unwrap();

    let global_nodes: Vec<_> = global_nodes
        .into_iter()
        .map(|(hash, (node, refcnt))| (hash, serialize_node(node), refcnt))
        .collect();

    for chunk in global_nodes.chunks(batch_factor) {
        if chunk.len() == batch_factor {
            let mut params = vec![];

            for (hash, node, refcnt) in chunk {
                params.extend_from_slice(rusqlite::params![hash, node, *refcnt]);
            }

            batch_stmt.execute(&params[..]).unwrap();
        } else {
            for (hash, node, refcnt) in chunk {
                stmt.execute(rusqlite::params![hash, node, refcnt]).unwrap();
            }
        }
    }

    Ok(())
}

fn serialize_node(node: TrieNode) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(65);

    match node {
        TrieNode::Binary { left, right } => {
            buffer.extend_from_slice(left.as_be_bytes());
            buffer.extend_from_slice(right.as_be_bytes());
        }
        TrieNode::Edge { child, path } => {
            buffer.extend_from_slice(child.as_be_bytes());
            // Bit path must be written in MSB format. This means that the LSB
            // must be in the last bit position. Since we write a fixed number of
            // bytes (32) but the path length may vary, we have to ensure we are writing
            // to the end of the slice.
            buffer.resize(65, 0);
            buffer[32..][..32].view_bits_mut::<Msb0>()[256 - path.len()..]
                .copy_from_bitslice(&path);

            buffer[64] = path.len() as u8;
        }
    }

    buffer
}
