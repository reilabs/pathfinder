use std::{mem, sync::mpsc, thread, time::Duration};

use anyhow::Context;
use rusqlite::params;

pub(crate) fn migrate(tx: &rusqlite::Transaction<'_>) -> anyhow::Result<()> {
    tracing::info!("Migrating starknet_transactions to new format");

    let mut transformers = Vec::new();
    let (insert_tx, insert_rx) = mpsc::channel();
    let (transform_tx, transform_rx) =
        flume::unbounded::<(i64, Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>)>();
    for _ in 0..thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4)
    {
        let insert_tx = insert_tx.clone();
        let transform_rx = transform_rx.clone();
        let mut compressor = zstd::bulk::Compressor::new(10).context("Create zstd compressor")?;
        let transformer = thread::spawn(move || {
            for (block_number, transactions, events) in transform_rx.iter() {
                let len = transactions.len();
                let transactions: Vec<_> = transactions
                    .into_iter()
                    .map(|(tx, receipt)| {
                        let transaction = zstd::decode_all(tx.as_slice())
                            .context("Decompressing transaction")
                            .unwrap();
                        let (transaction, _): (dto::Transaction, _) =
                            bincode::serde::decode_from_slice(
                                &transaction,
                                bincode::config::standard(),
                            )
                            .context("Deserializing transaction")
                            .unwrap();
                        let receipt = zstd::decode_all(receipt.as_slice())
                            .context("Decompressing receipt")
                            .unwrap();
                        let (receipt, _): (dto::Receipt, _) = bincode::serde::decode_from_slice(
                            &receipt,
                            bincode::config::standard(),
                        )
                        .context("Deserializing receipt")
                        .unwrap();
                        dto::TransactionWithReceipt {
                            transaction,
                            receipt: Some(receipt),
                        }
                    })
                    .collect();
                let events: Vec<_> = events
                    .into_iter()
                    .map(|events| {
                        let events = zstd::decode_all(events.as_slice())
                            .context("Decompressing events")
                            .unwrap();
                        let (events, _): (dto::Events, _) =
                            bincode::serde::decode_from_slice(&events, bincode::config::standard())
                                .context("Deserializing events")
                                .unwrap();
                        events
                    })
                    .collect();
                let transactions =
                    bincode::serde::encode_to_vec(transactions, bincode::config::standard())
                        .context("Serializing transaction")
                        .unwrap();
                let transactions = compressor
                    .compress(&transactions)
                    .context("Compressing transaction")
                    .unwrap();
                let events = bincode::serde::encode_to_vec(events, bincode::config::standard())
                    .context("Serializing events")
                    .unwrap();
                let events = compressor
                    .compress(&events)
                    .context("Compressing events")
                    .unwrap();

                // Store the updated values.
                if let Err(err) = insert_tx.send((block_number, transactions, events, len)) {
                    panic!("Failed to send transaction: {:?}", err);
                }
            }
        });
        transformers.push(transformer);
    }

    let count = tx.query_row("SELECT COUNT(*) FROM starknet_transactions", [], |row| {
        row.get::<_, i64>(0)
    })?;
    tx.execute_batch(
        r"
        CREATE TABLE transactions (
            block_number INTEGER NOT NULL,
            transactions BLOB NOT NULL,
            events BLOB
        );
        CREATE INDEX transactions_block_number ON transactions(block_number);
        CREATE TABLE transaction_hashes (
            hash         BLOB PRIMARY KEY NOT NULL,
            block_number INTEGER NOT NULL
        );
        CREATE INDEX transaction_hashes_block_number ON transaction_hashes(block_number);
        ",
    )
    .context("Creating new tables and indices")?;
    let mut query_stmt = tx.prepare_cached(
        r"
        SELECT hash, tx, block_number, tx, receipt, events FROM starknet_transactions ORDER BY block_number, idx
        ",
    )?;
    let mut insert_transaction_stmt = tx.prepare_cached(
        r"INSERT INTO transactions (block_number, transactions, events) VALUES (?, ?, ?)",
    )?;
    let mut insert_transaction_hash_stmt =
        tx.prepare_cached(r"INSERT INTO transaction_hashes (hash, block_number) VALUES (?, ?)")?;
    const BATCH_SIZE: u32 = 10_000;

    const LOG_RATE: Duration = Duration::from_secs(10);
    let mut last_log = std::time::Instant::now();

    let mut rows = query_stmt.query([])?;
    let mut progress = 0;
    loop {
        let mut current_block = 0i64;
        let mut transactions_in_block = Vec::new();
        let mut events_in_block = Vec::new();
        let mut batch_size = 0;
        loop {
            match rows.next() {
                Ok(Some(row)) => {
                    let hash = row.get_ref_unwrap("hash").as_blob()?;
                    let block_number = row.get_ref_unwrap("block_number").as_i64()?;
                    let transaction = row.get_ref_unwrap("tx").as_blob()?.to_vec();
                    let receipt = row.get_ref_unwrap("receipt").as_blob()?.to_vec();
                    let events = row.get_ref_unwrap("events").as_blob()?.to_vec();
                    insert_transaction_hash_stmt.execute(params![hash, block_number])?;
                    if block_number != current_block {
                        transform_tx
                            .send((
                                current_block,
                                mem::take(&mut transactions_in_block),
                                mem::take(&mut events_in_block),
                            ))
                            .context("Sending transactions to transformer")?;
                        current_block = block_number;
                        batch_size += 1;
                        if batch_size == BATCH_SIZE {
                            break;
                        }
                    }
                    transactions_in_block.push((transaction, receipt));
                    events_in_block.push(events);
                }
                Ok(None) => break,
                Err(err) => return Err(err.into()),
            }
        }
        for _ in 0..batch_size {
            let (block_number, transactions, events, len) = insert_rx.recv()?;
            insert_transaction_stmt.execute(params![block_number, transactions, events])?;
            progress += len;
        }
        if last_log.elapsed() > LOG_RATE {
            last_log = std::time::Instant::now();
            tracing::info!(
                "Migrating: {:.2}%",
                (progress as f64 / count as f64) * 100.0
            );
        }
        if batch_size < BATCH_SIZE {
            // This was the last batch.
            break;
        }
    }

    drop(insert_tx);
    drop(transform_tx);

    // Ensure that all transformers have finished successfully.
    for transformer in transformers {
        transformer.join().unwrap();
    }

    tracing::info!("Dropping old starknet_transactions table");
    tx.execute("DROP TABLE starknet_transactions", [])?;
    Ok(())
}

pub(crate) mod dto {
    use std::fmt;

    use fake::{Dummy, Fake, Faker};
    use pathfinder_common::*;
    use pathfinder_crypto::Felt;
    use serde::{Deserialize, Serialize};

    /// Minimally encoded Felt value.
    #[derive(Clone, Debug, PartialEq, Eq, Default)]
    pub struct MinimalFelt(Felt);

    impl serde::Serialize for MinimalFelt {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let bytes = self.0.as_be_bytes();
            let zeros = bytes.iter().take_while(|&&x| x == 0).count();
            bytes[zeros..].serialize(serializer)
        }
    }

    impl<'de> serde::Deserialize<'de> for MinimalFelt {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct Visitor;

            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = MinimalFelt;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a sequence")
                }

                fn visit_seq<B>(self, mut seq: B) -> Result<Self::Value, B::Error>
                where
                    B: serde::de::SeqAccess<'de>,
                {
                    let len = seq.size_hint().unwrap();
                    let mut bytes = [0; 32];
                    let num_zeros = bytes.len() - len;
                    let mut i = num_zeros;
                    while let Some(value) = seq.next_element()? {
                        bytes[i] = value;
                        i += 1;
                    }
                    Ok(MinimalFelt(Felt::from_be_bytes(bytes).unwrap()))
                }
            }

            deserializer.deserialize_seq(Visitor)
        }
    }

    impl From<Felt> for MinimalFelt {
        fn from(value: Felt) -> Self {
            Self(value)
        }
    }

    impl From<MinimalFelt> for Felt {
        fn from(value: MinimalFelt) -> Self {
            value.0
        }
    }

    impl<T> Dummy<T> for MinimalFelt {
        fn dummy_with_rng<R: rand::prelude::Rng + ?Sized>(config: &T, rng: &mut R) -> Self {
            let felt: Felt = Dummy::dummy_with_rng(config, rng);
            felt.into()
        }
    }

    /// Represents deserialized L2 transaction entry point values.
    #[derive(Copy, Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub enum EntryPointType {
        External,
        L1Handler,
    }

    impl From<pathfinder_common::transaction::EntryPointType> for EntryPointType {
        fn from(value: pathfinder_common::transaction::EntryPointType) -> Self {
            use pathfinder_common::transaction::EntryPointType::{External, L1Handler};
            match value {
                External => Self::External,
                L1Handler => Self::L1Handler,
            }
        }
    }

    impl From<EntryPointType> for pathfinder_common::transaction::EntryPointType {
        fn from(value: EntryPointType) -> Self {
            match value {
                EntryPointType::External => Self::External,
                EntryPointType::L1Handler => Self::L1Handler,
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub enum Events {
        V0 { events: Vec<Event> },
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct Event {
        pub data: Vec<MinimalFelt>,
        pub from_address: MinimalFelt,
        pub keys: Vec<MinimalFelt>,
    }

    impl From<pathfinder_common::event::Event> for Event {
        fn from(value: pathfinder_common::event::Event) -> Self {
            let pathfinder_common::event::Event {
                data,
                from_address,
                keys,
            } = value;
            Self {
                data: data
                    .into_iter()
                    .map(|x| x.as_inner().to_owned().into())
                    .collect(),
                from_address: from_address.as_inner().to_owned().into(),
                keys: keys
                    .into_iter()
                    .map(|x| x.as_inner().to_owned().into())
                    .collect(),
            }
        }
    }

    impl From<Event> for pathfinder_common::event::Event {
        fn from(value: Event) -> Self {
            Self {
                data: value
                    .data
                    .into_iter()
                    .map(|x| EventData(x.into()))
                    .collect(),
                from_address: ContractAddress::new_or_panic(value.from_address.into()),
                keys: value.keys.into_iter().map(|x| EventKey(x.into())).collect(),
            }
        }
    }

    /// Represents execution resources for L2 transaction.
    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ExecutionResources {
        pub builtins: BuiltinCounters,
        pub n_steps: u64,
        pub n_memory_holes: u64,
        pub data_availability: ExecutionDataAvailability,
    }

    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct ExecutionDataAvailability {
        // TODO make these mandatory once some new release makes resyncing necessary
        pub l1_gas: Option<u128>,
        pub l1_data_gas: Option<u128>,
    }

    impl From<&ExecutionResources> for pathfinder_common::receipt::ExecutionResources {
        fn from(value: &ExecutionResources) -> Self {
            Self {
                builtins: value.builtins.into(),
                n_steps: value.n_steps,
                n_memory_holes: value.n_memory_holes,
                data_availability: match (
                    value.data_availability.l1_gas,
                    value.data_availability.l1_data_gas,
                ) {
                    (Some(l1_gas), Some(l1_data_gas)) => {
                        pathfinder_common::receipt::ExecutionDataAvailability {
                            l1_gas,
                            l1_data_gas,
                        }
                    }
                    _ => Default::default(),
                },
            }
        }
    }

    impl From<&pathfinder_common::receipt::ExecutionResources> for ExecutionResources {
        fn from(value: &pathfinder_common::receipt::ExecutionResources) -> Self {
            Self {
                builtins: (&value.builtins).into(),
                n_steps: value.n_steps,
                n_memory_holes: value.n_memory_holes,
                data_availability: ExecutionDataAvailability {
                    l1_gas: Some(value.data_availability.l1_gas),
                    l1_data_gas: Some(value.data_availability.l1_data_gas),
                },
            }
        }
    }

    impl<T> Dummy<T> for ExecutionResources {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            let (l1_gas, l1_data_gas) = if rng.gen() {
                (Some(rng.next_u32() as u128), Some(rng.next_u32() as u128))
            } else {
                (None, None)
            };

            Self {
                builtins: Faker.fake_with_rng(rng),
                n_steps: rng.next_u32() as u64,
                n_memory_holes: rng.next_u32() as u64,
                data_availability: ExecutionDataAvailability {
                    l1_gas,
                    l1_data_gas,
                },
            }
        }
    }

    // This struct purposefully allows for unknown fields as it is not critical to
    // store these counters perfectly. Failure would be far more costly than simply
    // ignoring them.
    #[derive(Copy, Clone, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
    pub struct BuiltinCounters {
        pub output: u64,
        pub pedersen: u64,
        pub range_check: u64,
        pub ecdsa: u64,
        pub bitwise: u64,
        pub ec_op: u64,
        pub keccak: u64,
        pub poseidon: u64,
        pub segment_arena: u64,
    }

    impl From<BuiltinCounters> for pathfinder_common::receipt::BuiltinCounters {
        fn from(value: BuiltinCounters) -> Self {
            // Use deconstruction to ensure these structs remain in-sync.
            let BuiltinCounters {
                output,
                pedersen,
                range_check,
                ecdsa,
                bitwise,
                ec_op,
                keccak,
                poseidon,
                segment_arena,
            } = value;
            Self {
                output,
                pedersen,
                range_check,
                ecdsa,
                bitwise,
                ec_op,
                keccak,
                poseidon,
                segment_arena,
            }
        }
    }

    impl From<&pathfinder_common::receipt::BuiltinCounters> for BuiltinCounters {
        fn from(value: &pathfinder_common::receipt::BuiltinCounters) -> Self {
            // Use deconstruction to ensure these structs remain in-sync.
            let pathfinder_common::receipt::BuiltinCounters {
                output,
                pedersen,
                range_check,
                ecdsa,
                bitwise,
                ec_op,
                keccak,
                poseidon,
                segment_arena,
            } = value.clone();
            Self {
                output,
                pedersen,
                range_check,
                ecdsa,
                bitwise,
                ec_op,
                keccak,
                poseidon,
                segment_arena,
            }
        }
    }

    impl<T> Dummy<T> for BuiltinCounters {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                output: rng.next_u32() as u64,
                pedersen: rng.next_u32() as u64,
                range_check: rng.next_u32() as u64,
                ecdsa: rng.next_u32() as u64,
                bitwise: rng.next_u32() as u64,
                ec_op: rng.next_u32() as u64,
                keccak: rng.next_u32() as u64,
                poseidon: rng.next_u32() as u64,
                segment_arena: 0, // Not used in p2p
            }
        }
    }

    /// Represents deserialized L2 to L1 message.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub struct L2ToL1Message {
        pub from_address: MinimalFelt,
        pub payload: Vec<MinimalFelt>,
        pub to_address: EthereumAddress,
    }

    impl From<L2ToL1Message> for pathfinder_common::receipt::L2ToL1Message {
        fn from(value: L2ToL1Message) -> Self {
            let L2ToL1Message {
                from_address,
                payload,
                to_address,
            } = value;
            pathfinder_common::receipt::L2ToL1Message {
                from_address: ContractAddress::new_or_panic(from_address.into()),
                payload: payload
                    .into_iter()
                    .map(|x| L2ToL1MessagePayloadElem(x.into()))
                    .collect(),
                to_address,
            }
        }
    }

    impl From<&pathfinder_common::receipt::L2ToL1Message> for L2ToL1Message {
        fn from(value: &pathfinder_common::receipt::L2ToL1Message) -> Self {
            let pathfinder_common::receipt::L2ToL1Message {
                from_address,
                payload,
                to_address,
            } = value.clone();
            Self {
                from_address: from_address.as_inner().to_owned().into(),
                payload: payload
                    .into_iter()
                    .map(|x| x.as_inner().to_owned().into())
                    .collect(),
                to_address,
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    pub enum ExecutionStatus {
        Succeeded,
        Reverted { reason: String },
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub enum Receipt {
        V0(ReceiptV0),
    }

    /// Represents deserialized L2 transaction receipt data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub struct ReceiptV0 {
        pub actual_fee: MinimalFelt,
        pub execution_resources: Option<ExecutionResources>,
        pub l2_to_l1_messages: Vec<L2ToL1Message>,
        pub transaction_hash: MinimalFelt,
        pub transaction_index: TransactionIndex,
        pub execution_status: ExecutionStatus,
    }

    impl From<Receipt> for pathfinder_common::receipt::Receipt {
        fn from(value: Receipt) -> Self {
            use pathfinder_common::receipt as common;

            let Receipt::V0(ReceiptV0 {
                actual_fee,
                execution_resources,
                // This information is redundant as it is already in the transaction itself.
                l2_to_l1_messages,
                transaction_hash,
                transaction_index,
                execution_status,
            }) = value;

            common::Receipt {
                actual_fee: Fee(actual_fee.into()),
                execution_resources: (&execution_resources.unwrap_or_default()).into(),
                l2_to_l1_messages: l2_to_l1_messages.into_iter().map(Into::into).collect(),
                transaction_hash: TransactionHash(transaction_hash.into()),
                transaction_index,
                execution_status: match execution_status {
                    ExecutionStatus::Succeeded => common::ExecutionStatus::Succeeded,
                    ExecutionStatus::Reverted { reason } => {
                        common::ExecutionStatus::Reverted { reason }
                    }
                },
            }
        }
    }

    impl From<&pathfinder_common::receipt::Receipt> for Receipt {
        fn from(value: &pathfinder_common::receipt::Receipt) -> Self {
            Self::V0(ReceiptV0 {
                actual_fee: value.actual_fee.as_inner().to_owned().into(),
                execution_resources: Some((&value.execution_resources).into()),
                l2_to_l1_messages: value.l2_to_l1_messages.iter().map(Into::into).collect(),
                transaction_hash: value.transaction_hash.as_inner().to_owned().into(),
                transaction_index: value.transaction_index,
                execution_status: match &value.execution_status {
                    receipt::ExecutionStatus::Succeeded => ExecutionStatus::Succeeded,
                    receipt::ExecutionStatus::Reverted { reason } => ExecutionStatus::Reverted {
                        reason: reason.clone(),
                    },
                },
            })
        }
    }

    #[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Dummy)]
    pub enum DataAvailabilityMode {
        L1,
        L2,
    }

    impl From<DataAvailabilityMode> for pathfinder_common::transaction::DataAvailabilityMode {
        fn from(value: DataAvailabilityMode) -> Self {
            match value {
                DataAvailabilityMode::L1 => Self::L1,
                DataAvailabilityMode::L2 => Self::L2,
            }
        }
    }

    impl From<pathfinder_common::transaction::DataAvailabilityMode> for DataAvailabilityMode {
        fn from(value: pathfinder_common::transaction::DataAvailabilityMode) -> Self {
            match value {
                pathfinder_common::transaction::DataAvailabilityMode::L1 => Self::L1,
                pathfinder_common::transaction::DataAvailabilityMode::L2 => Self::L2,
            }
        }
    }

    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    pub struct ResourceBounds {
        pub l1_gas: ResourceBound,
        pub l2_gas: ResourceBound,
    }

    impl From<ResourceBounds> for pathfinder_common::transaction::ResourceBounds {
        fn from(value: ResourceBounds) -> Self {
            Self {
                l1_gas: value.l1_gas.into(),
                l2_gas: value.l2_gas.into(),
            }
        }
    }

    impl From<pathfinder_common::transaction::ResourceBounds> for ResourceBounds {
        fn from(value: pathfinder_common::transaction::ResourceBounds) -> Self {
            Self {
                l1_gas: value.l1_gas.into(),
                l2_gas: value.l2_gas.into(),
            }
        }
    }

    #[derive(Copy, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    pub struct ResourceBound {
        pub max_amount: ResourceAmount,
        pub max_price_per_unit: ResourcePricePerUnit,
    }

    impl From<ResourceBound> for pathfinder_common::transaction::ResourceBound {
        fn from(value: ResourceBound) -> Self {
            Self {
                max_amount: value.max_amount,
                max_price_per_unit: value.max_price_per_unit,
            }
        }
    }

    impl From<pathfinder_common::transaction::ResourceBound> for ResourceBound {
        fn from(value: pathfinder_common::transaction::ResourceBound) -> Self {
            Self {
                max_amount: value.max_amount,
                max_price_per_unit: value.max_price_per_unit,
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct TransactionWithReceipt {
        pub transaction: Transaction,
        pub receipt: Option<Receipt>,
    }

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub enum Transaction {
        V0 {
            hash: MinimalFelt,
            variant: TransactionVariantV0,
        },
    }

    /// Represents deserialized L2 transaction data.
    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub enum TransactionVariantV0 {
        DeclareV0(DeclareTransactionV0V1),
        DeclareV1(DeclareTransactionV0V1),
        DeclareV2(DeclareTransactionV2),
        DeclareV3(DeclareTransactionV3),
        // FIXME regenesis: remove Deploy txn type after regenesis
        // We are keeping this type of transaction until regenesis
        // only to support older pre-0.11.0 blocks
        Deploy(DeployTransaction),
        DeployAccountV1(DeployAccountTransactionV1),
        DeployAccountV3(DeployAccountTransactionV3),
        InvokeV0(InvokeTransactionV0),
        InvokeV1(InvokeTransactionV1),
        InvokeV3(InvokeTransactionV3),
        L1HandlerV0(L1HandlerTransactionV0),
    }

    impl From<&pathfinder_common::transaction::Transaction> for Transaction {
        fn from(value: &pathfinder_common::transaction::Transaction) -> Self {
            use pathfinder_common::transaction::TransactionVariant::*;
            use pathfinder_common::transaction::*;

            let transaction_hash = value.hash;
            match value.variant.clone() {
                DeclareV0(DeclareTransactionV0V1 {
                    class_hash,
                    max_fee,
                    nonce,
                    sender_address,
                    signature,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeclareV0(self::DeclareTransactionV0V1 {
                        class_hash: class_hash.as_inner().to_owned().into(),
                        max_fee: max_fee.as_inner().to_owned().into(),
                        nonce: nonce.as_inner().to_owned().into(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
                DeclareV1(DeclareTransactionV0V1 {
                    class_hash,
                    max_fee,
                    nonce,
                    sender_address,
                    signature,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeclareV1(self::DeclareTransactionV0V1 {
                        class_hash: class_hash.as_inner().to_owned().into(),
                        max_fee: max_fee.as_inner().to_owned().into(),
                        nonce: nonce.as_inner().to_owned().into(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
                DeclareV2(DeclareTransactionV2 {
                    class_hash,
                    max_fee,
                    nonce,
                    sender_address,
                    signature,
                    compiled_class_hash,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeclareV2(self::DeclareTransactionV2 {
                        class_hash: class_hash.as_inner().to_owned().into(),
                        max_fee: max_fee.as_inner().to_owned().into(),
                        nonce: nonce.as_inner().to_owned().into(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        compiled_class_hash: compiled_class_hash.as_inner().to_owned().into(),
                    }),
                },
                DeclareV3(DeclareTransactionV3 {
                    class_hash,
                    nonce,
                    nonce_data_availability_mode,
                    fee_data_availability_mode,
                    resource_bounds,
                    tip,
                    paymaster_data,
                    signature,
                    account_deployment_data,
                    sender_address,
                    compiled_class_hash,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeclareV3(self::DeclareTransactionV3 {
                        class_hash: class_hash.as_inner().to_owned().into(),
                        nonce: nonce.as_inner().to_owned().into(),
                        nonce_data_availability_mode: nonce_data_availability_mode.into(),
                        fee_data_availability_mode: fee_data_availability_mode.into(),
                        resource_bounds: resource_bounds.into(),
                        tip,
                        paymaster_data: paymaster_data
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        compiled_class_hash: compiled_class_hash.as_inner().to_owned().into(),
                        account_deployment_data: account_deployment_data
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
                Deploy(DeployTransaction {
                    contract_address,
                    contract_address_salt,
                    class_hash,
                    constructor_calldata,
                    version,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::Deploy(self::DeployTransaction {
                        contract_address: contract_address.as_inner().to_owned().into(),
                        contract_address_salt: contract_address_salt.as_inner().to_owned().into(),
                        class_hash: class_hash.as_inner().to_owned().into(),
                        constructor_calldata: constructor_calldata
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        version: version.0.into(),
                    }),
                },
                DeployAccountV1(DeployAccountTransactionV1 {
                    contract_address,
                    max_fee,
                    signature,
                    nonce,
                    contract_address_salt,
                    constructor_calldata,
                    class_hash,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeployAccountV1(
                        self::DeployAccountTransactionV1 {
                            contract_address: contract_address.as_inner().to_owned().into(),
                            max_fee: max_fee.as_inner().to_owned().into(),
                            signature: signature
                                .into_iter()
                                .map(|x| x.as_inner().to_owned().into())
                                .collect(),
                            nonce: nonce.as_inner().to_owned().into(),
                            contract_address_salt: contract_address_salt
                                .as_inner()
                                .to_owned()
                                .into(),
                            constructor_calldata: constructor_calldata
                                .into_iter()
                                .map(|x| x.as_inner().to_owned().into())
                                .collect(),
                            class_hash: class_hash.as_inner().to_owned().into(),
                        },
                    ),
                },
                DeployAccountV3(DeployAccountTransactionV3 {
                    contract_address,
                    signature,
                    nonce,
                    nonce_data_availability_mode,
                    fee_data_availability_mode,
                    resource_bounds,
                    tip,
                    paymaster_data,
                    contract_address_salt,
                    constructor_calldata,
                    class_hash,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::DeployAccountV3(
                        self::DeployAccountTransactionV3 {
                            nonce: nonce.as_inner().to_owned().into(),
                            nonce_data_availability_mode: nonce_data_availability_mode.into(),
                            fee_data_availability_mode: fee_data_availability_mode.into(),
                            resource_bounds: resource_bounds.into(),
                            tip,
                            paymaster_data: paymaster_data
                                .into_iter()
                                .map(|x| x.as_inner().to_owned().into())
                                .collect(),
                            sender_address: contract_address.as_inner().to_owned().into(),
                            signature: signature
                                .into_iter()
                                .map(|x| x.as_inner().to_owned().into())
                                .collect(),
                            contract_address_salt: contract_address_salt
                                .as_inner()
                                .to_owned()
                                .into(),
                            constructor_calldata: constructor_calldata
                                .into_iter()
                                .map(|x| x.as_inner().to_owned().into())
                                .collect(),
                            class_hash: class_hash.as_inner().to_owned().into(),
                        },
                    ),
                },
                InvokeV0(InvokeTransactionV0 {
                    calldata,
                    sender_address,
                    entry_point_selector,
                    entry_point_type,
                    max_fee,
                    signature,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::InvokeV0(self::InvokeTransactionV0 {
                        calldata: calldata
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        entry_point_selector: entry_point_selector.as_inner().to_owned().into(),
                        entry_point_type: entry_point_type.map(Into::into),
                        max_fee: max_fee.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
                InvokeV1(InvokeTransactionV1 {
                    calldata,
                    sender_address,
                    max_fee,
                    signature,
                    nonce,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::InvokeV1(self::InvokeTransactionV1 {
                        calldata: calldata
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        max_fee: max_fee.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        nonce: nonce.as_inner().to_owned().into(),
                    }),
                },
                InvokeV3(InvokeTransactionV3 {
                    signature,
                    nonce,
                    nonce_data_availability_mode,
                    fee_data_availability_mode,
                    resource_bounds,
                    tip,
                    paymaster_data,
                    account_deployment_data,
                    calldata,
                    sender_address,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::InvokeV3(self::InvokeTransactionV3 {
                        nonce: nonce.as_inner().to_owned().into(),
                        nonce_data_availability_mode: nonce_data_availability_mode.into(),
                        fee_data_availability_mode: fee_data_availability_mode.into(),
                        resource_bounds: resource_bounds.into(),
                        tip,
                        paymaster_data: paymaster_data
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        sender_address: sender_address.as_inner().to_owned().into(),
                        signature: signature
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        calldata: calldata
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                        account_deployment_data: account_deployment_data
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
                L1Handler(L1HandlerTransaction {
                    contract_address,
                    entry_point_selector,
                    nonce,
                    calldata,
                }) => Self::V0 {
                    hash: transaction_hash.as_inner().to_owned().into(),
                    variant: TransactionVariantV0::L1HandlerV0(self::L1HandlerTransactionV0 {
                        contract_address: contract_address.as_inner().to_owned().into(),
                        entry_point_selector: entry_point_selector.as_inner().to_owned().into(),
                        nonce: nonce.as_inner().to_owned().into(),
                        calldata: calldata
                            .into_iter()
                            .map(|x| x.as_inner().to_owned().into())
                            .collect(),
                    }),
                },
            }
        }
    }

    impl From<Transaction> for pathfinder_common::transaction::Transaction {
        fn from(value: Transaction) -> Self {
            use pathfinder_common::transaction::TransactionVariant;

            let hash = value.hash();
            let variant = match value {
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeclareV0(DeclareTransactionV0V1 {
                            class_hash,
                            max_fee,
                            nonce,
                            sender_address,
                            signature,
                        }),
                } => TransactionVariant::DeclareV0(
                    pathfinder_common::transaction::DeclareTransactionV0V1 {
                        class_hash: ClassHash(class_hash.into()),
                        max_fee: Fee(max_fee.into()),
                        nonce: TransactionNonce(nonce.into()),
                        sender_address: ContractAddress::new_or_panic(sender_address.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeclareV1(DeclareTransactionV0V1 {
                            class_hash,
                            max_fee,
                            nonce,
                            sender_address,
                            signature,
                        }),
                } => TransactionVariant::DeclareV1(
                    pathfinder_common::transaction::DeclareTransactionV0V1 {
                        class_hash: ClassHash(class_hash.into()),
                        max_fee: Fee(max_fee.into()),
                        nonce: TransactionNonce(nonce.into()),
                        sender_address: ContractAddress(sender_address.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeclareV2(DeclareTransactionV2 {
                            class_hash,
                            max_fee,
                            nonce,
                            sender_address,
                            signature,
                            compiled_class_hash,
                        }),
                } => TransactionVariant::DeclareV2(
                    pathfinder_common::transaction::DeclareTransactionV2 {
                        class_hash: ClassHash(class_hash.into()),
                        max_fee: Fee(max_fee.into()),
                        nonce: TransactionNonce(nonce.into()),
                        sender_address: ContractAddress(sender_address.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        compiled_class_hash: CasmHash::new_or_panic(compiled_class_hash.into()),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeclareV3(DeclareTransactionV3 {
                            class_hash,
                            nonce,
                            nonce_data_availability_mode,
                            fee_data_availability_mode,
                            resource_bounds,
                            tip,
                            paymaster_data,
                            sender_address,
                            signature,
                            compiled_class_hash,
                            account_deployment_data,
                        }),
                } => TransactionVariant::DeclareV3(
                    pathfinder_common::transaction::DeclareTransactionV3 {
                        class_hash: ClassHash(class_hash.into()),
                        nonce: TransactionNonce(nonce.into()),
                        nonce_data_availability_mode: nonce_data_availability_mode.into(),
                        fee_data_availability_mode: fee_data_availability_mode.into(),
                        resource_bounds: resource_bounds.into(),
                        tip,
                        paymaster_data: paymaster_data
                            .into_iter()
                            .map(|x| PaymasterDataElem(x.into()))
                            .collect(),
                        sender_address: ContractAddress::new_or_panic(sender_address.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        compiled_class_hash: CasmHash::new_or_panic(compiled_class_hash.into()),
                        account_deployment_data: account_deployment_data
                            .into_iter()
                            .map(|x| AccountDeploymentDataElem(x.into()))
                            .collect(),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::Deploy(DeployTransaction {
                            contract_address,
                            contract_address_salt,
                            class_hash,
                            constructor_calldata,
                            version,
                        }),
                } => {
                    TransactionVariant::Deploy(pathfinder_common::transaction::DeployTransaction {
                        contract_address: ContractAddress::new_or_panic(contract_address.into()),
                        contract_address_salt: ContractAddressSalt(contract_address_salt.into()),
                        class_hash: ClassHash(class_hash.into()),
                        constructor_calldata: constructor_calldata
                            .into_iter()
                            .map(|x| ConstructorParam(x.into()))
                            .collect(),
                        version: TransactionVersion(version.into()),
                    })
                }
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeployAccountV1(DeployAccountTransactionV1 {
                            contract_address,
                            max_fee,
                            signature,
                            nonce,
                            contract_address_salt,
                            constructor_calldata,
                            class_hash,
                        }),
                } => TransactionVariant::DeployAccountV1(
                    pathfinder_common::transaction::DeployAccountTransactionV1 {
                        contract_address: ContractAddress::new_or_panic(contract_address.into()),
                        max_fee: Fee(max_fee.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        nonce: TransactionNonce(nonce.into()),
                        contract_address_salt: ContractAddressSalt(contract_address_salt.into()),
                        constructor_calldata: constructor_calldata
                            .into_iter()
                            .map(|x| CallParam(x.into()))
                            .collect(),
                        class_hash: ClassHash(class_hash.into()),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::DeployAccountV3(DeployAccountTransactionV3 {
                            nonce,
                            nonce_data_availability_mode,
                            fee_data_availability_mode,
                            resource_bounds,
                            tip,
                            paymaster_data,
                            sender_address,
                            signature,
                            contract_address_salt,
                            constructor_calldata,
                            class_hash,
                        }),
                } => TransactionVariant::DeployAccountV3(
                    pathfinder_common::transaction::DeployAccountTransactionV3 {
                        contract_address: ContractAddress::new_or_panic(sender_address.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        nonce: TransactionNonce(nonce.into()),
                        nonce_data_availability_mode: nonce_data_availability_mode.into(),
                        fee_data_availability_mode: fee_data_availability_mode.into(),
                        resource_bounds: resource_bounds.into(),
                        tip,
                        paymaster_data: paymaster_data
                            .into_iter()
                            .map(|x| PaymasterDataElem(x.into()))
                            .collect(),
                        contract_address_salt: ContractAddressSalt(contract_address_salt.into()),
                        constructor_calldata: constructor_calldata
                            .into_iter()
                            .map(|x| CallParam(x.into()))
                            .collect(),
                        class_hash: ClassHash(class_hash.into()),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::InvokeV0(InvokeTransactionV0 {
                            calldata,
                            sender_address,
                            entry_point_selector,
                            entry_point_type,
                            max_fee,
                            signature,
                        }),
                } => TransactionVariant::InvokeV0(
                    pathfinder_common::transaction::InvokeTransactionV0 {
                        calldata: calldata.into_iter().map(|x| CallParam(x.into())).collect(),
                        sender_address: ContractAddress::new_or_panic(sender_address.into()),
                        entry_point_selector: EntryPoint(entry_point_selector.into()),
                        entry_point_type: entry_point_type.map(Into::into),
                        max_fee: Fee(max_fee.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::InvokeV1(InvokeTransactionV1 {
                            calldata,
                            sender_address,
                            max_fee,
                            signature,
                            nonce,
                        }),
                } => TransactionVariant::InvokeV1(
                    pathfinder_common::transaction::InvokeTransactionV1 {
                        calldata: calldata.into_iter().map(|x| CallParam(x.into())).collect(),
                        sender_address: ContractAddress::new_or_panic(sender_address.into()),
                        max_fee: Fee(max_fee.into()),
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        nonce: TransactionNonce(nonce.into()),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::InvokeV3(InvokeTransactionV3 {
                            nonce,
                            nonce_data_availability_mode,
                            fee_data_availability_mode,
                            resource_bounds,
                            tip,
                            paymaster_data,
                            sender_address,
                            signature,
                            calldata,
                            account_deployment_data,
                        }),
                } => TransactionVariant::InvokeV3(
                    pathfinder_common::transaction::InvokeTransactionV3 {
                        signature: signature
                            .into_iter()
                            .map(|x| TransactionSignatureElem(x.into()))
                            .collect(),
                        nonce: TransactionNonce(nonce.into()),
                        nonce_data_availability_mode: nonce_data_availability_mode.into(),
                        fee_data_availability_mode: fee_data_availability_mode.into(),
                        resource_bounds: resource_bounds.into(),
                        tip,
                        paymaster_data: paymaster_data
                            .into_iter()
                            .map(|x| PaymasterDataElem(x.into()))
                            .collect(),
                        account_deployment_data: account_deployment_data
                            .into_iter()
                            .map(|x| AccountDeploymentDataElem(x.into()))
                            .collect(),
                        calldata: calldata.into_iter().map(|x| CallParam(x.into())).collect(),
                        sender_address: ContractAddress::new_or_panic(sender_address.into()),
                    },
                ),
                Transaction::V0 {
                    hash: _,
                    variant:
                        TransactionVariantV0::L1HandlerV0(L1HandlerTransactionV0 {
                            contract_address,
                            entry_point_selector,
                            nonce,
                            calldata,
                        }),
                } => TransactionVariant::L1Handler(
                    pathfinder_common::transaction::L1HandlerTransaction {
                        contract_address: ContractAddress::new_or_panic(contract_address.into()),
                        entry_point_selector: EntryPoint(entry_point_selector.into()),
                        nonce: TransactionNonce(nonce.into()),
                        calldata: calldata.into_iter().map(|x| CallParam(x.into())).collect(),
                    },
                ),
            };

            pathfinder_common::transaction::Transaction { hash, variant }
        }
    }

    impl Transaction {
        /// Returns hash of the transaction
        pub fn hash(&self) -> TransactionHash {
            match self {
                Transaction::V0 { hash, .. } => TransactionHash(hash.to_owned().into()),
            }
        }
    }

    /// A version 0 or 1 declare transaction.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeclareTransactionV0V1 {
        pub class_hash: MinimalFelt,
        pub max_fee: MinimalFelt,
        pub nonce: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
    }

    impl<T> Dummy<T> for DeclareTransactionV0V1 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                class_hash: Faker.fake_with_rng(rng),
                max_fee: Faker.fake_with_rng(rng),
                nonce: TransactionNonce::ZERO.0.into(),
                sender_address: Faker.fake_with_rng(rng),
                signature: Faker.fake_with_rng(rng),
            }
        }
    }

    /// A version 2 declare transaction.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub struct DeclareTransactionV2 {
        pub class_hash: MinimalFelt,
        pub max_fee: MinimalFelt,
        pub nonce: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
        pub compiled_class_hash: MinimalFelt,
    }

    /// A version 2 declare transaction.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeclareTransactionV3 {
        pub class_hash: MinimalFelt,
        pub nonce: MinimalFelt,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub resource_bounds: ResourceBounds,
        pub tip: Tip,
        pub paymaster_data: Vec<MinimalFelt>,
        pub signature: Vec<MinimalFelt>,
        pub account_deployment_data: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
        pub compiled_class_hash: MinimalFelt,
    }

    impl<T> Dummy<T> for DeclareTransactionV3 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                class_hash: Faker.fake_with_rng(rng),
                nonce: Faker.fake_with_rng(rng),
                nonce_data_availability_mode: Faker.fake_with_rng(rng),
                fee_data_availability_mode: Faker.fake_with_rng(rng),
                resource_bounds: Faker.fake_with_rng(rng),
                tip: Faker.fake_with_rng(rng),
                paymaster_data: vec![Faker.fake_with_rng(rng)], // TODO p2p allows 1 elem only
                sender_address: Faker.fake_with_rng(rng),
                signature: Faker.fake_with_rng(rng),
                compiled_class_hash: Faker.fake_with_rng(rng),
                account_deployment_data: vec![Faker.fake_with_rng(rng)], // TODO p2p allows 1 elem only
            }
        }
    }

    /// Represents deserialized L2 deploy transaction data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeployTransaction {
        pub contract_address: MinimalFelt,
        pub version: MinimalFelt,
        pub contract_address_salt: MinimalFelt,
        pub class_hash: MinimalFelt,
        pub constructor_calldata: Vec<MinimalFelt>,
    }

    impl<T> Dummy<T> for DeployTransaction {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                version: Felt::from_u64(rng.gen_range(0..=1)).into(),
                contract_address: ContractAddress::ZERO.0.into(), // Faker.fake_with_rng(rng), FIXME
                contract_address_salt: Faker.fake_with_rng(rng),
                class_hash: Faker.fake_with_rng(rng),
                constructor_calldata: Faker.fake_with_rng(rng),
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeployAccountTransactionV1 {
        pub contract_address: MinimalFelt,
        pub max_fee: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
        pub nonce: MinimalFelt,
        pub contract_address_salt: MinimalFelt,
        pub constructor_calldata: Vec<MinimalFelt>,
        pub class_hash: MinimalFelt,
    }

    impl<T> Dummy<T> for DeployAccountTransactionV1 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            let contract_address_salt = Faker.fake_with_rng(rng);
            let constructor_calldata: Vec<CallParam> = Faker.fake_with_rng(rng);
            let class_hash = Faker.fake_with_rng(rng);

            Self {
                contract_address: ContractAddress::deployed_contract_address(
                    constructor_calldata.iter().copied(),
                    &contract_address_salt,
                    &class_hash,
                )
                .as_inner()
                .to_owned()
                .into(),
                max_fee: Faker.fake_with_rng(rng),
                signature: Faker.fake_with_rng(rng),
                nonce: Faker.fake_with_rng(rng),
                contract_address_salt: contract_address_salt.as_inner().to_owned().into(),
                constructor_calldata: constructor_calldata
                    .into_iter()
                    .map(|x| x.as_inner().to_owned().into())
                    .collect(),
                class_hash: class_hash.as_inner().to_owned().into(),
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct DeployAccountTransactionV3 {
        pub sender_address: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
        pub nonce: MinimalFelt,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub resource_bounds: ResourceBounds,
        pub tip: Tip,
        pub paymaster_data: Vec<MinimalFelt>,
        pub contract_address_salt: MinimalFelt,
        pub constructor_calldata: Vec<MinimalFelt>,
        pub class_hash: MinimalFelt,
    }

    impl<T> Dummy<T> for DeployAccountTransactionV3 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            let contract_address_salt = Faker.fake_with_rng(rng);
            let constructor_calldata: Vec<CallParam> = Faker.fake_with_rng(rng);
            let class_hash = Faker.fake_with_rng(rng);

            Self {
                nonce: Faker.fake_with_rng(rng),
                nonce_data_availability_mode: Faker.fake_with_rng(rng),
                fee_data_availability_mode: Faker.fake_with_rng(rng),
                resource_bounds: Faker.fake_with_rng(rng),
                tip: Faker.fake_with_rng(rng),
                paymaster_data: vec![Faker.fake_with_rng(rng)], // TODO p2p allows 1 elem only

                sender_address: ContractAddress::deployed_contract_address(
                    constructor_calldata.iter().copied(),
                    &contract_address_salt,
                    &class_hash,
                )
                .as_inner()
                .to_owned()
                .into(),
                signature: Faker.fake_with_rng(rng),
                contract_address_salt: contract_address_salt.as_inner().to_owned().into(),
                constructor_calldata: constructor_calldata
                    .into_iter()
                    .map(|x| x.as_inner().to_owned().into())
                    .collect(),
                class_hash: class_hash.as_inner().to_owned().into(),
            }
        }
    }

    /// Represents deserialized L2 invoke transaction v0 data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct InvokeTransactionV0 {
        pub calldata: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
        pub entry_point_selector: MinimalFelt,
        pub entry_point_type: Option<EntryPointType>,
        pub max_fee: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
    }

    impl<T> Dummy<T> for InvokeTransactionV0 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                calldata: Faker.fake_with_rng(rng),
                sender_address: Faker.fake_with_rng(rng),
                entry_point_selector: Faker.fake_with_rng(rng),
                entry_point_type: None,
                max_fee: Faker.fake_with_rng(rng),
                signature: Faker.fake_with_rng(rng),
            }
        }
    }

    /// Represents deserialized L2 invoke transaction v1 data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, Dummy)]
    #[serde(deny_unknown_fields)]
    pub struct InvokeTransactionV1 {
        pub calldata: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
        pub max_fee: MinimalFelt,
        pub signature: Vec<MinimalFelt>,
        pub nonce: MinimalFelt,
    }

    /// Represents deserialized L2 invoke transaction v3 data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct InvokeTransactionV3 {
        pub signature: Vec<MinimalFelt>,
        pub nonce: MinimalFelt,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub resource_bounds: ResourceBounds,
        pub tip: Tip,
        pub paymaster_data: Vec<MinimalFelt>,
        pub account_deployment_data: Vec<MinimalFelt>,
        pub calldata: Vec<MinimalFelt>,
        pub sender_address: MinimalFelt,
    }

    impl<T> Dummy<T> for InvokeTransactionV3 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                nonce: Faker.fake_with_rng(rng),
                nonce_data_availability_mode: Faker.fake_with_rng(rng),
                fee_data_availability_mode: Faker.fake_with_rng(rng),
                resource_bounds: Faker.fake_with_rng(rng),
                tip: Faker.fake_with_rng(rng),
                paymaster_data: vec![Faker.fake_with_rng(rng)], // TODO p2p allows 1 elem only

                sender_address: Faker.fake_with_rng(rng),
                signature: Faker.fake_with_rng(rng),
                calldata: Faker.fake_with_rng(rng),
                account_deployment_data: vec![Faker.fake_with_rng(rng)], // TODO p2p allows 1 elem only
            }
        }
    }

    /// Represents deserialized L2 "L1 handler" transaction data.
    #[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct L1HandlerTransactionV0 {
        pub contract_address: MinimalFelt,
        pub entry_point_selector: MinimalFelt,
        pub nonce: MinimalFelt,
        pub calldata: Vec<MinimalFelt>,
    }

    impl<T> Dummy<T> for L1HandlerTransactionV0 {
        fn dummy_with_rng<R: rand::Rng + ?Sized>(_: &T, rng: &mut R) -> Self {
            Self {
                contract_address: Faker.fake_with_rng(rng),
                entry_point_selector: Faker.fake_with_rng(rng),
                nonce: Faker.fake_with_rng(rng),
                calldata: Faker.fake_with_rng(rng),
            }
        }
    }
}
