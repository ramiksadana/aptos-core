// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::data_manager::DataManager;
use crate::network::NetworkSender;
use crate::network_interface::ConsensusMsg;
use crate::quorum_store::quorum_store::QuorumStoreError;
use crate::quorum_store::types::TxnData;
use crate::quorum_store::utils::MempoolProxy;
use crate::quorum_store::{counters, quorum_store::QuorumStoreCommand};
use crate::round_manager::VerifiedEvent;
use aptos_crypto::hash::DefaultHasher;
use aptos_crypto::HashValue;
use aptos_infallible::Mutex;
use aptos_mempool::QuorumStoreRequest;
use aptos_types::PeerId;
use bcs::to_bytes;
use channel::aptos_channel;
use consensus_types::common::{Payload, PayloadFilter};
use consensus_types::proof_of_store::LogicalTime;
use consensus_types::request_response::ConsensusResponse;
use consensus_types::{
    common::TransactionSummary, proof_of_store::ProofOfStore, request_response::WrapperCommand,
};
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{
    channel::{
        mpsc::{Receiver, Sender},
        oneshot,
    },
    StreamExt,
};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

// TODO: Consider storing batches and retrying upon QuorumStoreError:Timeout
#[allow(dead_code)]
pub struct QuorumStoreWrapper {
    // TODO: remove, DataManager sends msg to wrapper
    data_manager: Arc<dyn DataManager>,
    mempool_proxy: MempoolProxy,
    quorum_store_sender: tokio::sync::mpsc::Sender<QuorumStoreCommand>,
    batches_to_filter: HashMap<HashValue, Vec<TransactionSummary>>, // TODO: batch_in_progress
    // TODO: add the expiration priority queue
    batch_in_progress: Vec<TransactionSummary>,
    bytes_in_progress: usize,
    latest_logical_time: LogicalTime,
    batches_for_consensus: HashMap<HashValue, ProofOfStore>, // TODO: use expiration priority queue as well
    // TODO: store all ProofOfStore (created locally, and received via broadcast)
    // TODO: need to be notified of ProofOfStore's that were committed
    mempool_txn_pull_max_count: u64, // TODO: make sure this times max transaction size is smaller than quorumstore max batch size in bytes
    quorum_store_max_batch_bytes: u64,
}

impl QuorumStoreWrapper {
    pub fn new(
        epoch: u64,
        data_manager: Arc<dyn DataManager>,
        mempool_tx: Sender<QuorumStoreRequest>,
        quorum_store_sender: tokio::sync::mpsc::Sender<QuorumStoreCommand>,
        mempool_txn_pull_timeout_ms: u64,
        mempool_txn_pull_max_count: u64,
        quorum_store_max_batch_bytes: u64,
    ) -> Self {
        Self {
            data_manager,
            mempool_proxy: MempoolProxy::new(mempool_tx, mempool_txn_pull_timeout_ms),
            quorum_store_sender,
            batches_to_filter: HashMap::new(),
            batch_in_progress: Vec::new(),
            bytes_in_progress: 0,
            latest_logical_time: LogicalTime::new(epoch, 0),
            batches_for_consensus: HashMap::new(),
            mempool_txn_pull_max_count,
            quorum_store_max_batch_bytes,
        }
    }

    pub(crate) async fn handle_scheduled_pull(
        &mut self,
    ) -> Option<oneshot::Receiver<Result<ProofOfStore, QuorumStoreError>>> {
        let mut exclude_txns: Vec<_> = self.batches_to_filter.values().flatten().cloned().collect();
        exclude_txns.extend(self.batch_in_progress.clone());
        // TODO: size and unwrap or not?
        let pulled_txns = self
            .mempool_proxy
            .pull_internal(self.mempool_txn_pull_max_count, exclude_txns)
            .await
            .unwrap();

        let mut end_batch = false;
        let mut txns_data = Vec::new();

        // TODO: pass TxnData to QuorumStore to save extra serialization.
        let mut pulled_txns_cloned = pulled_txns.clone();
        for txn in pulled_txns {
            let bytes = to_bytes(&txn).unwrap();
            if self.bytes_in_progress + bytes.len() > self.quorum_store_max_batch_bytes as usize {
                end_batch = true;
                self.bytes_in_progress = 0;
                break;
            } else {
                self.batch_in_progress.push(TransactionSummary {
                    sender: txn.sender(),
                    sequence_number: txn.sequence_number(),
                });
                self.bytes_in_progress = self.bytes_in_progress + bytes.len();
                let mut hasher = DefaultHasher::new(b"TxnData");
                hasher.update(&bytes);
                txns_data.push(TxnData {
                    txn_bytes: bytes,
                    hash: hasher.finish(),
                })
            }
        }

        let txns = pulled_txns_cloned.drain(0..txns_data.len()).collect();
        // TODO: also some timer if there are not enough txns (Rati)
        if !end_batch {
            self.quorum_store_sender
                .send(QuorumStoreCommand::AppendToBatch(txns))
                .await
                .expect("could not send to QuorumStore");
            None
        } else {
            let (proof_tx, proof_rx) = oneshot::channel();
            let (digest_tx, digest_rx) = oneshot::channel(); // TODO: consider computing batch digest here
            let logical_time = LogicalTime::new(
                self.latest_logical_time.epoch(),
                self.latest_logical_time.round() + 20, //TODO: take from quorum store config
            );
            self.quorum_store_sender
                .send(QuorumStoreCommand::EndBatch(
                    txns,
                    logical_time.clone(),
                    digest_tx, // TODO (on boarding task for Rati:)): consider getting rid of this channel and maintaining batch id and fragment id here.
                    proof_tx,
                ))
                .await
                .expect("could not send to QuorumStore");
            match digest_rx.await {
                Ok(ret) => {
                    match ret {
                        Ok(digest) => {
                            let last_batch = self.batch_in_progress.drain(..).collect();
                            self.batches_to_filter.insert(digest, last_batch);
                            // TODO: add to the (expiration, digest) to priority queue

                            return Some(proof_rx);
                        }
                        Err(QuorumStoreError::BatchSizeLimit) => {
                            todo!()
                        }
                        Err(_) => {
                            unreachable!();
                        }
                    }
                }
                Err(_) => {
                    // TODO: do something
                }
            }
            return None;
        }
    }

    pub(crate) async fn broadcast_completed_proof(
        &mut self,
        proof: ProofOfStore,
        network_sender: &mut NetworkSender,
    ) {
        network_sender
            .broadcast_without_self(ConsensusMsg::ProofOfStoreBroadcastMsg(Box::new(
                proof.clone(),
            )))
            .await;
    }

    // TODO: priority queue on LogicalTime to clean old proofs
    pub(crate) async fn insert_proof(&mut self, mut new_proof: ProofOfStore) {
        let maybe_proof = self.batches_for_consensus.remove(new_proof.digest());
        if let Some(proof) = maybe_proof {
            if proof.expiration() > new_proof.expiration() {
                new_proof = proof;
            }
        }
        self.batches_for_consensus
            .insert(new_proof.digest().clone(), new_proof);
    }

    pub(crate) async fn handle_local_proof(
        &mut self,
        msg: Result<ProofOfStore, QuorumStoreError>,
        network_sender: &mut NetworkSender,
    ) {
        match msg {
            Ok(proof) => {
                self.insert_proof(proof.clone()).await;
                self.broadcast_completed_proof(proof, network_sender).await;
            }
            Err(QuorumStoreError::Timeout(digest)) => {
                // TODO: even if broadcast fails, we should not remove it?
                // self.batches_to_filter.remove(&digest);
            }
            Err(_) => {
                unreachable!();
            }
        }
    }

    pub(crate) async fn handle_consensus_request(&mut self, msg: WrapperCommand) {
        match msg {
            // TODO: check what max_block_size consensus is using
            WrapperCommand::GetBlockRequest(max_block_size, filter, callback) => {
                // TODO: Pass along to batch_store
                let excluded_proofs: HashSet<HashValue> = match filter {
                    PayloadFilter::Empty => HashSet::new(),
                    PayloadFilter::DirectMempool(_) => {
                        unreachable!()
                    }
                    PayloadFilter::InQuorumStore(proofs) => proofs,
                };

                let mut batch = Vec::new();
                for proof in self.batches_for_consensus.values() {
                    if batch.len() == max_block_size as usize {
                        break;
                    }
                    if excluded_proofs.contains(proof.digest()) {
                        continue;
                    }
                    batch.push(proof.clone());
                }
                let res = ConsensusResponse::GetBlockResponse(Payload::InQuorumStore(batch));
                callback
                    .send(Ok(res))
                    .expect("BlcokResponse receiver not available");
            }
            WrapperCommand::CleanRequest(logical_time, digests) => {
                self.latest_logical_time = logical_time;
                for digest in digests {
                    self.batches_to_filter.remove(&digest);
                    self.batches_for_consensus.remove(&digest);
                }
                // TODO: remove but make CleanRequest not a ConsensusRequest,
                // TODO: as notify_commit now calls CleanRequest.
                // self.data_manager
                //     .notify_commit(self.latest_logical_time)
                //     .await;
            }
        }
    }

    // TODO: use tokio select for the internal timeout feature
    pub async fn start(
        mut self,
        mut network_sender: NetworkSender,
        mut consensus_receiver: Receiver<WrapperCommand>,
        mut shutdown: Receiver<()>,
        mut network_msg_rx: aptos_channel::Receiver<PeerId, VerifiedEvent>,
    ) {
        let mut scheduled_pulls: FuturesUnordered<ScheduledPull> = FuturesUnordered::new();
        scheduled_pulls.push(ScheduledPull::new(
            Instant::now() + Duration::from_millis(50),
            false,
        ));
        let mut proofs_in_progress: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();

        loop {
            let _timer = counters::MAIN_LOOP.start_timer();
            // TODO: need an explicit shutdown message, as pulls will continue to schedule
            ::futures::select! {
                _backoff = scheduled_pulls.next() => {
                    if let Some(proof_rx) = self.handle_scheduled_pull().await {
                        proofs_in_progress.push(Box::pin(proof_rx));
                    }
                    scheduled_pulls.push(ScheduledPull::new(
                        Instant::now() + Duration::from_millis(50),
                        false
                    ));
                },
                next = proofs_in_progress.next() => {
                    // TODO: handle failures
                    if let Some(Ok(msg)) = next {
                        self.handle_local_proof(msg, &mut network_sender).await;
                    }
                },
                msg = consensus_receiver.select_next_some() => {
                    self.handle_consensus_request(msg).await;
                },
                _ = shutdown.select_next_some() => {
                    break;
                }
                msg = network_msg_rx.next() => {
                   if let Some(VerifiedEvent::ProofOfStoreBroadcast(proof)) = msg{
                        self.insert_proof(*proof).await;
                    }
                }
                complete => break,
            }
        }

        // Periodically:
        // 1. Pull from mempool.
        // 2. a. Start a batch with these txns if batch is not active
        //    b. Continue batch with these txns if batch is active
        // 3. Close batch if criteria is met.

        // State needed:
        // 1. txn summaries that are part of all pending batches: map<batch_id, vec<txn>>
        //    - pending batches: batches, including those in progress, that have not yet been cleaned.
        //    - batch_id: needs to include epoch, round info.
        // 2. all completed digests that have not yet been cleaned: map<batch_id, digest>
        //    -- is this really needed? pull_payload filters anyway. maybe all that's needed
        //    is a broadcast queue?
    }
}

/// From: Mempool ScheduledBroadcast
pub(crate) struct ScheduledPull {
    /// Time of scheduled pull
    deadline: Instant,
    backoff: bool,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl ScheduledPull {
    pub fn new(deadline: Instant, backoff: bool) -> Self {
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        let waker_clone = waker.clone();

        if deadline > Instant::now() {
            let tokio_instant = tokio::time::Instant::from_std(deadline);
            // TODO: something more general?
            tokio::spawn(async move {
                tokio::time::sleep_until(tokio_instant).await;
                let mut waker = waker_clone.lock();
                if let Some(waker) = waker.take() {
                    waker.wake()
                }
            });
        }

        Self {
            deadline,
            backoff,
            waker,
        }
    }
}

impl Future for ScheduledPull {
    type Output = bool; // whether this pull was scheduled as a backoff

    fn poll(self: Pin<&mut Self>, context: &mut Context) -> Poll<Self::Output> {
        if Instant::now() < self.deadline {
            let waker_clone = context.waker().clone();
            let mut waker = self.waker.lock();
            *waker = Some(waker_clone);

            Poll::Pending
        } else {
            Poll::Ready(self.backoff)
        }
    }
}