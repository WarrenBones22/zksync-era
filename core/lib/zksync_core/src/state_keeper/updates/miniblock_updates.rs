use std::collections::HashMap;

use multivm::{
    interface::{ExecutionResult, L2BlockEnv, VmExecutionResultAndLogs},
    vm_latest::TransactionVmExt,
};
use zksync_types::{
    block::{BlockGasCount, MiniblockHasher},
    event::extract_bytecodes_marked_as_known,
    l2_to_l1_log::{SystemL2ToL1Log, UserL2ToL1Log},
    tx::{tx_execution_info::TxExecutionStatus, ExecutionMetrics, TransactionExecutionResult},
    vm_trace::Call,
    MiniblockNumber, ProtocolVersionId, StorageLogQuery, Transaction, VmEvent, H256,
};
use zksync_utils::bytecode::{hash_bytecode, CompressedBytecodeInfo};

#[derive(Debug, Clone, PartialEq)]
pub struct MiniblockUpdates {
    pub executed_transactions: Vec<TransactionExecutionResult>,
    pub events: Vec<VmEvent>,
    pub storage_logs: Vec<StorageLogQuery>,
    pub user_l2_to_l1_logs: Vec<UserL2ToL1Log>,
    pub system_l2_to_l1_logs: Vec<SystemL2ToL1Log>,
    pub new_factory_deps: HashMap<H256, Vec<u8>>,
    /// How much L1 gas will it take to submit this block?
    pub l1_gas_count: BlockGasCount,
    pub block_execution_metrics: ExecutionMetrics,
    pub txs_encoding_size: usize,
    pub payload_encoding_size: usize,
    pub timestamp: u64,
    pub number: MiniblockNumber,
    pub prev_block_hash: H256,
    pub virtual_blocks: u32,
    pub protocol_version: ProtocolVersionId,
}

impl MiniblockUpdates {
    pub(crate) fn new(
        timestamp: u64,
        number: MiniblockNumber,
        prev_block_hash: H256,
        virtual_blocks: u32,
        protocol_version: ProtocolVersionId,
    ) -> Self {
        Self {
            executed_transactions: vec![],
            events: vec![],
            storage_logs: vec![],
            user_l2_to_l1_logs: vec![],
            system_l2_to_l1_logs: vec![],
            new_factory_deps: HashMap::new(),
            l1_gas_count: BlockGasCount::default(),
            block_execution_metrics: ExecutionMetrics::default(),
            txs_encoding_size: 0,
            payload_encoding_size: 0,
            timestamp,
            number,
            prev_block_hash,
            virtual_blocks,
            protocol_version,
        }
    }

    pub(crate) fn extend_from_fictive_transaction(
        &mut self,
        result: VmExecutionResultAndLogs,
        l1_gas_count: BlockGasCount,
        execution_metrics: ExecutionMetrics,
    ) {
        self.events.extend(result.logs.events);
        self.storage_logs.extend(result.logs.storage_logs);
        self.user_l2_to_l1_logs
            .extend(result.logs.user_l2_to_l1_logs);
        self.system_l2_to_l1_logs
            .extend(result.logs.system_l2_to_l1_logs);

        self.l1_gas_count += l1_gas_count;
        self.block_execution_metrics += execution_metrics;
    }

    pub(crate) fn extend_from_executed_transaction(
        &mut self,
        tx: Transaction,
        tx_execution_result: VmExecutionResultAndLogs,
        tx_l1_gas_this_tx: BlockGasCount,
        execution_metrics: ExecutionMetrics,
        compressed_bytecodes: Vec<CompressedBytecodeInfo>,
        call_traces: Vec<Call>,
    ) {
        let saved_factory_deps =
            extract_bytecodes_marked_as_known(&tx_execution_result.logs.events);
        self.events.extend(tx_execution_result.logs.events);
        self.user_l2_to_l1_logs
            .extend(tx_execution_result.logs.user_l2_to_l1_logs);
        self.system_l2_to_l1_logs
            .extend(tx_execution_result.logs.system_l2_to_l1_logs);

        let gas_refunded = tx_execution_result.refunds.gas_refunded;
        let operator_suggested_refund = tx_execution_result.refunds.operator_suggested_refund;
        let execution_status = if tx_execution_result.result.is_failed() {
            TxExecutionStatus::Failure
        } else {
            TxExecutionStatus::Success
        };

        let revert_reason = match &tx_execution_result.result {
            ExecutionResult::Success { .. } => None,
            ExecutionResult::Revert { output } => Some(output.to_string()),
            ExecutionResult::Halt { reason } => Some(reason.to_string()),
        };

        // Get transaction factory deps
        let factory_deps = tx.execute.factory_deps.as_deref().unwrap_or_default();
        let tx_factory_deps: HashMap<_, _> = factory_deps
            .iter()
            .map(|bytecode| (hash_bytecode(bytecode), bytecode))
            .collect();

        // Save all bytecodes that were marked as known on the bootloader
        let known_bytecodes = saved_factory_deps.into_iter().map(|bytecode_hash| {
            let bytecode = tx_factory_deps.get(&bytecode_hash).unwrap_or_else(|| {
                panic!(
                    "Failed to get factory deps on tx: bytecode hash: {:?}, tx hash: {}",
                    bytecode_hash,
                    tx.hash()
                )
            });
            (bytecode_hash, bytecode.to_vec())
        });
        self.new_factory_deps.extend(known_bytecodes);

        self.l1_gas_count += tx_l1_gas_this_tx;
        self.block_execution_metrics += execution_metrics;
        self.txs_encoding_size += tx.bootloader_encoding_size();
        self.payload_encoding_size +=
            zksync_protobuf::repr::encode::<zksync_dal::consensus::proto::Transaction>(&tx).len();
        self.storage_logs
            .extend(tx_execution_result.logs.storage_logs);

        self.executed_transactions.push(TransactionExecutionResult {
            hash: tx.hash(),
            transaction: tx,
            execution_info: execution_metrics,
            execution_status,
            refunded_gas: gas_refunded,
            operator_suggested_refund,
            compressed_bytecodes,
            call_traces,
            revert_reason,
        });
    }

    /// Calculates miniblock hash based on the protocol version.
    pub(crate) fn get_miniblock_hash(&self) -> H256 {
        let mut digest = MiniblockHasher::new(self.number, self.timestamp, self.prev_block_hash);
        for tx in &self.executed_transactions {
            digest.push_tx_hash(tx.hash);
        }
        digest.finalize(self.protocol_version)
    }

    pub(crate) fn get_miniblock_env(&self) -> L2BlockEnv {
        L2BlockEnv {
            number: self.number.0,
            timestamp: self.timestamp,
            prev_block_hash: self.prev_block_hash,
            max_virtual_blocks_to_create: self.virtual_blocks,
        }
    }
}

#[cfg(test)]
mod tests {
    use multivm::vm_latest::TransactionVmExt;

    use super::*;
    use crate::state_keeper::tests::{create_execution_result, create_transaction};

    #[test]
    fn apply_empty_l2_tx() {
        let mut accumulator = MiniblockUpdates::new(
            0,
            MiniblockNumber(0),
            H256::random(),
            0,
            ProtocolVersionId::latest(),
        );
        let tx = create_transaction(10, 100);
        let bootloader_encoding_size = tx.bootloader_encoding_size();
        let payload_encoding_size =
            zksync_protobuf::repr::encode::<zksync_dal::consensus::proto::Transaction>(&tx).len();

        accumulator.extend_from_executed_transaction(
            tx,
            create_execution_result(0, []),
            BlockGasCount::default(),
            ExecutionMetrics::default(),
            vec![],
            vec![],
        );

        assert_eq!(accumulator.executed_transactions.len(), 1);
        assert_eq!(accumulator.events.len(), 0);
        assert_eq!(accumulator.storage_logs.len(), 0);
        assert_eq!(accumulator.user_l2_to_l1_logs.len(), 0);
        assert_eq!(accumulator.system_l2_to_l1_logs.len(), 0);
        assert_eq!(accumulator.l1_gas_count, Default::default());
        assert_eq!(accumulator.new_factory_deps.len(), 0);
        assert_eq!(accumulator.block_execution_metrics.l2_to_l1_logs, 0);
        assert_eq!(accumulator.txs_encoding_size, bootloader_encoding_size);
        assert_eq!(accumulator.payload_encoding_size, payload_encoding_size);
    }
}
