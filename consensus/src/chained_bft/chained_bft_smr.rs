// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::chained_bft::event_processor::EventProcessor;
use crate::{
    chained_bft::{
        block_storage::BlockStore,
        epoch_manager::{EpochManager, LivenessStorageData, Processor},
        network::{NetworkReceivers, NetworkTask},
        network_interface::{ConsensusNetworkEvents, ConsensusNetworkSender},
        persistent_liveness_storage::PersistentLivenessStorage,
    },
    consensus_provider::ConsensusProvider,
    counters,
    state_replication::{StateComputer, TxnManager},
    util::time_service::ClockTimeService,
};
use anyhow::Result;
use consensus_types::common::{Author, Payload, Round};
use futures::{select, stream::StreamExt};
use libra_config::config::{ConsensusConfig, NodeConfig};
use libra_logger::prelude::*;
use safety_rules::SafetyRulesManager;
use std::{
    sync::{Arc, RwLock},
    time::Instant,
};
use tokio::runtime::{self, Handle, Runtime};

/// All these structures need to be moved into EpochManager. Rather than make each one an option
/// and perform ugly unwraps, they are bundled here.
pub struct ChainedBftSMRInput<T> {
    network_sender: ConsensusNetworkSender,
    network_events: ConsensusNetworkEvents,
    safety_rules_manager: SafetyRulesManager<T>,
    state_computer: Arc<dyn StateComputer<Payload = T>>,
    txn_manager: Box<dyn TxnManager<Payload = T>>,
    config: ConsensusConfig,
}

/// ChainedBFTSMR is the one to generate the components (BlockStore, Proposer, etc.) and start the
/// driver. ChainedBftSMR implements the StateMachineReplication, it is going to be used by
/// ConsensusProvider for the e2e flow.
pub struct ChainedBftSMR<T> {
    author: Author,
    runtime: Option<Runtime>,
    block_store: Option<Arc<BlockStore<T>>>,
    storage: Arc<dyn PersistentLivenessStorage<T>>,
    input: Option<ChainedBftSMRInput<T>>,
}

impl<T: Payload> ChainedBftSMR<T> {
    pub fn new(
        network_sender: ConsensusNetworkSender,
        network_events: ConsensusNetworkEvents,
        node_config: &mut NodeConfig,
        state_computer: Arc<dyn StateComputer<Payload = T>>,
        storage: Arc<dyn PersistentLivenessStorage<T>>,
        txn_manager: Box<dyn TxnManager<Payload = T>>,
    ) -> Self {
        let input = ChainedBftSMRInput {
            network_sender,
            network_events,
            safety_rules_manager: SafetyRulesManager::new(node_config),
            state_computer,
            txn_manager,
            config: node_config.consensus.clone(),
        };

        Self {
            author: node_config.validator_network.as_ref().unwrap().peer_id,
            runtime: None,
            block_store: None,
            storage,
            input: Some(input),
        }
    }

    #[cfg(test)]
    pub fn author(&self) -> Author {
        self.author
    }

    #[cfg(test)]
    pub fn block_store(&self) -> Option<Arc<BlockStore<T>>> {
        self.block_store.clone()
    }

    // Depending on what data we can extract from consensusdb, we may or may not have an
    // event processor at startup. If we need to sync up with peers for blocks to construct
    // a valid block store, which is required to construct an event processor, we will take
    // care of the sync up here. If we already have an event processor, it will just simply
    // be extracted out of the enum and returned
    async fn gen_event_processor(
        processor: Processor<T>,
        epoch_manager: &mut EpochManager<T>,
        network_receivers: &mut NetworkReceivers<T>,
    ) -> EventProcessor<T> {
        match processor {
            Processor::StartupSyncProcessor(mut startup_sync_processor) => {
                loop {
                    let pre_select_instant = Instant::now();
                    let idle_duration;
                    select! {
                        proposal_msg = network_receivers.proposals.select_next_some() => {
                            idle_duration = pre_select_instant.elapsed();
                            if let Ok(initial_data) = startup_sync_processor.process_proposal_msg(proposal_msg).await {
                                break epoch_manager.start_epoch_with_recovery_data(initial_data);
                            }
                        }
                        vote_msg = network_receivers.votes.select_next_some() => {
                            idle_duration = pre_select_instant.elapsed();
                            if let Ok(initial_data) = startup_sync_processor.process_vote(vote_msg).await {
                                break epoch_manager.start_epoch_with_recovery_data(initial_data);
                            }
                        }
                        sync_info_msg = network_receivers.sync_info_msgs.select_next_some() => {
                            idle_duration = pre_select_instant.elapsed();
                            if let Ok(initial_data) = startup_sync_processor.process_sync_info_msg(sync_info_msg.0, sync_info_msg.1).await {
                                break epoch_manager.start_epoch_with_recovery_data(initial_data);
                            }
                        }
                        ledger_info = network_receivers.epoch_change.select_next_some() => {
                            idle_duration = pre_select_instant.elapsed();
                            if epoch_manager.epoch() <= ledger_info.ledger_info().epoch() {
                                let event_processor = epoch_manager.start_new_epoch(ledger_info).await;
                                // clean up all the previous messages from the old epochs
                                network_receivers.clear_prev_epoch_msgs();
                                break event_processor;
                            }
                        }
                        different_epoch_and_peer = network_receivers.different_epoch.select_next_some() => {
                            idle_duration = pre_select_instant.elapsed();
                            epoch_manager.process_different_epoch(different_epoch_and_peer.0, different_epoch_and_peer.1).await
                        }
                    }
                    counters::STARTUP_SYNC_LOOP_BUSY_DURATION_S
                        .observe_duration(pre_select_instant.elapsed() - idle_duration);
                    counters::STARTUP_SYNC_LOOP_IDLE_DURATION_S.observe_duration(idle_duration);
                }
            }
            Processor::EventProcessor(event_processor) => event_processor,
        }
    }

    fn start_event_processing(
        executor: Handle,
        mut epoch_manager: EpochManager<T>,
        processor: Processor<T>,
        mut pacemaker_timeout_sender_rx: channel::Receiver<Round>,
        network_task: NetworkTask<T>,
        mut network_receivers: NetworkReceivers<T>,
    ) {
        let fut = async move {
            // TODO: Event loop logic need to get cleaned up(#2518)
            let mut event_processor =
                Self::gen_event_processor(processor, &mut epoch_manager, &mut network_receivers)
                    .await;
            event_processor.start().await;
            loop {
                let pre_select_instant = Instant::now();
                let idle_duration;
                select! {
                    proposal_msg = network_receivers.proposals.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        event_processor.process_proposal_msg(proposal_msg).await;
                    }
                    block_retrieval = network_receivers.block_retrieval.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        event_processor.process_block_retrieval(block_retrieval).await;
                    }
                    vote_msg = network_receivers.votes.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        event_processor.process_vote(vote_msg).await;
                    }
                    local_timeout_round = pacemaker_timeout_sender_rx.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        event_processor.process_local_timeout(local_timeout_round).await;
                    }
                    sync_info_msg = network_receivers.sync_info_msgs.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        event_processor.process_sync_info_msg(sync_info_msg.0, sync_info_msg.1).await;
                    }
                    ledger_info = network_receivers.epoch_change.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        if epoch_manager.epoch() <= ledger_info.ledger_info().epoch() {
                            event_processor = epoch_manager.start_new_epoch(ledger_info).await;
                            // clean up all the previous messages from the old epochs
                            network_receivers.clear_prev_epoch_msgs();
                            event_processor.start().await;
                        }
                    }
                    different_epoch_and_peer = network_receivers.different_epoch.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        epoch_manager.process_different_epoch(different_epoch_and_peer.0, different_epoch_and_peer.1).await
                    }
                    epoch_retrieval_and_peer = network_receivers.epoch_retrieval.select_next_some() => {
                        idle_duration = pre_select_instant.elapsed();
                        epoch_manager.process_epoch_retrieval(epoch_retrieval_and_peer.0, epoch_retrieval_and_peer.1).await
                    }
                }
                counters::EVENT_PROCESSING_LOOP_BUSY_DURATION_S
                    .observe_duration(pre_select_instant.elapsed() - idle_duration);
                counters::EVENT_PROCESSING_LOOP_IDLE_DURATION_S.observe_duration(idle_duration);
            }
        };
        executor.spawn(network_task.start());
        executor.spawn(fut);
    }
}

impl<T: Payload> ConsensusProvider for ChainedBftSMR<T> {
    /// We're following the steps to start
    /// 1. Construct the EpochManager from the latest libradb state
    /// 2. Construct per-epoch component with the fixed Validators provided by EpochManager including
    /// ProposerElection, Pacemaker, SafetyRules, Network(Populate with known validators), EventProcessor
    fn start(&mut self) -> Result<()> {
        let mut runtime = runtime::Builder::new()
            .thread_name("consensus-")
            .threaded_scheduler()
            .enable_all()
            .build()
            .expect("Failed to create Tokio runtime!");
        let input = self.input.take().expect("already started, input is None");

        let executor = runtime.handle().clone();
        let time_service = Arc::new(ClockTimeService::new(executor.clone()));

        let (timeout_sender, timeout_receiver) =
            channel::new(1_024, &counters::PENDING_PACEMAKER_TIMEOUTS);
        let (self_sender, self_receiver) = channel::new(1_024, &counters::PENDING_SELF_MESSAGES);

        let liveness_storage_data: LivenessStorageData<T> = runtime.block_on(self.storage.start());

        let epoch_info = Arc::new(RwLock::new(liveness_storage_data.epoch_info()));

        let mut epoch_mgr = EpochManager::new(
            self.author,
            Arc::clone(&epoch_info),
            input.config,
            time_service,
            self_sender,
            input.network_sender.clone(),
            timeout_sender,
            input.txn_manager,
            input.state_computer,
            self.storage.clone(),
            input.safety_rules_manager,
        );

        let (network_task, network_receiver) =
            NetworkTask::new(epoch_info, input.network_events, self_receiver);

        let processor = epoch_mgr.start(liveness_storage_data);

        if let Processor::EventProcessor(p) = &processor {
            self.block_store = Some(p.block_store());
        }

        Self::start_event_processing(
            executor,
            epoch_mgr,
            processor,
            timeout_receiver,
            network_task,
            network_receiver,
        );

        self.runtime = Some(runtime);

        debug!("Chained BFT SMR started.");
        Ok(())
    }

    /// Stop is synchronous: waits for all the worker threads to terminate.
    fn stop(&mut self) {
        if let Some(_rt) = self.runtime.take() {
            debug!("Chained BFT SMR stopped.")
        }
    }
}
