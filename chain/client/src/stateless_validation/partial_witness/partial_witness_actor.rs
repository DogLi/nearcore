use std::collections::HashSet;
use std::sync::Arc;

use itertools::Itertools;
use near_async::futures::{AsyncComputationSpawner, AsyncComputationSpawnerExt};
use near_async::messaging::{Actor, CanSend, Handler, Sender};
use near_async::time::Clock;
use near_async::{MultiSend, MultiSenderFrom};
use near_chain::types::RuntimeAdapter;
use near_chain::Error;
use near_chain_configs::MutableValidatorSigner;
use near_epoch_manager::EpochManagerAdapter;
use near_network::state_witness::{
    ChunkContractAccessesMessage, ChunkStateWitnessAckMessage, ContractCodeRequestMessage,
    ContractCodeResponseMessage, PartialEncodedContractDeploysMessage,
    PartialEncodedStateWitnessForwardMessage, PartialEncodedStateWitnessMessage,
};
use near_network::types::{NetworkRequests, PeerManagerAdapter, PeerManagerMessageRequest};
use near_parameters::RuntimeConfig;
use near_performance_metrics_macros::perf;
use near_primitives::reed_solomon::{ReedSolomonEncoder, ReedSolomonEncoderCache};
use near_primitives::sharding::ShardChunkHeader;
use near_primitives::stateless_validation::contract_distribution::{
    ChunkContractAccesses, ChunkContractDeploys, CodeBytes, CodeHash, ContractCodeRequest,
    ContractCodeResponse, PartialEncodedContractDeploys, PartialEncodedContractDeploysPart,
};
use near_primitives::stateless_validation::partial_witness::PartialEncodedStateWitness;
use near_primitives::stateless_validation::state_witness::{
    ChunkStateWitness, ChunkStateWitnessAck, EncodedChunkStateWitness,
};
use near_primitives::stateless_validation::ChunkProductionKey;
use near_primitives::types::{AccountId, EpochId};
use near_primitives::validator_signer::ValidatorSigner;
use near_store::adapter::trie_store::TrieStoreAdapter;
use near_store::{StorageError, TrieDBStorage, TrieStorage};
use near_vm_runner::{get_contract_cache_key, ContractCode, ContractRuntimeCache};

use crate::client_actor::ClientSenderForPartialWitness;
use crate::metrics;
use crate::stateless_validation::state_witness_tracker::ChunkStateWitnessTracker;
use crate::stateless_validation::validate::{
    validate_chunk_contract_accesses, validate_partial_encoded_contract_deploys,
    validate_partial_encoded_state_witness,
};

use super::encoding::{CONTRACT_DEPLOYS_RATIO_DATA_PARTS, WITNESS_RATIO_DATA_PARTS};
use super::partial_deploys_tracker::PartialEncodedContractDeploysTracker;
use super::partial_witness_tracker::PartialEncodedStateWitnessTracker;
use near_primitives::utils::compression::CompressedData;

pub struct PartialWitnessActor {
    /// Adapter to send messages to the network.
    network_adapter: PeerManagerAdapter,
    /// Validator signer to sign the state witness. This field is mutable and optional. Use with caution!
    /// Lock the value of mutable validator signer for the duration of a request to ensure consistency.
    /// Please note that the locked value should not be stored anywhere or passed through the thread boundary.
    my_signer: MutableValidatorSigner,
    epoch_manager: Arc<dyn EpochManagerAdapter>,
    runtime: Arc<dyn RuntimeAdapter>,
    /// Tracks the parts of the state witness sent from chunk producers to chunk validators.
    partial_witness_tracker: PartialEncodedStateWitnessTracker,
    partial_deploys_tracker: PartialEncodedContractDeploysTracker,
    /// Tracks a collection of state witnesses sent from chunk producers to chunk validators.
    state_witness_tracker: ChunkStateWitnessTracker,
    /// Reed Solomon encoder for encoding state witness parts.
    /// We keep one wrapper for each length of chunk_validators to avoid re-creating the encoder.
    witness_encoders: ReedSolomonEncoderCache,
    /// Same as above for contract deploys
    contract_deploys_encoders: ReedSolomonEncoderCache,
    compile_contracts_spawner: Arc<dyn AsyncComputationSpawner>,
}

impl Actor for PartialWitnessActor {}

#[derive(actix::Message, Debug)]
#[rtype(result = "()")]
pub struct DistributeStateWitnessRequest {
    pub epoch_id: EpochId,
    pub chunk_header: ShardChunkHeader,
    pub state_witness: ChunkStateWitness,
    pub contract_deploys: Vec<ContractCode>,
}

#[derive(Clone, MultiSend, MultiSenderFrom)]
pub struct PartialWitnessSenderForClient {
    pub distribute_chunk_state_witness: Sender<DistributeStateWitnessRequest>,
}

impl Handler<DistributeStateWitnessRequest> for PartialWitnessActor {
    #[perf]
    fn handle(&mut self, msg: DistributeStateWitnessRequest) {
        if let Err(err) = self.handle_distribute_state_witness_request(msg) {
            tracing::error!(target: "client", ?err, "Failed to handle distribute chunk state witness request");
        }
    }
}

impl Handler<ChunkStateWitnessAckMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: ChunkStateWitnessAckMessage) {
        self.handle_chunk_state_witness_ack(msg.0);
    }
}

impl Handler<PartialEncodedStateWitnessMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: PartialEncodedStateWitnessMessage) {
        if let Err(err) = self.handle_partial_encoded_state_witness(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle PartialEncodedStateWitnessMessage");
        }
    }
}

impl Handler<PartialEncodedStateWitnessForwardMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: PartialEncodedStateWitnessForwardMessage) {
        if let Err(err) = self.handle_partial_encoded_state_witness_forward(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle PartialEncodedStateWitnessForwardMessage");
        }
    }
}

impl Handler<ChunkContractAccessesMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: ChunkContractAccessesMessage) {
        if let Err(err) = self.handle_chunk_contract_accesses(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle ChunkContractAccessesMessage");
        }
    }
}

impl Handler<PartialEncodedContractDeploysMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: PartialEncodedContractDeploysMessage) {
        if let Err(err) = self.handle_partial_encoded_contract_deploys(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle PartialEncodedContractDeploysMessage");
        }
    }
}

impl Handler<ContractCodeRequestMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: ContractCodeRequestMessage) {
        if let Err(err) = self.handle_contract_code_request(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle ContractCodeRequestMessage");
        }
    }
}

impl Handler<ContractCodeResponseMessage> for PartialWitnessActor {
    fn handle(&mut self, msg: ContractCodeResponseMessage) {
        if let Err(err) = self.handle_contract_code_response(msg.0) {
            tracing::error!(target: "client", ?err, "Failed to handle ContractCodeResponseMessage");
        }
    }
}

impl PartialWitnessActor {
    pub fn new(
        clock: Clock,
        network_adapter: PeerManagerAdapter,
        client_sender: ClientSenderForPartialWitness,
        my_signer: MutableValidatorSigner,
        epoch_manager: Arc<dyn EpochManagerAdapter>,
        runtime: Arc<dyn RuntimeAdapter>,
        compile_contracts_spawner: Arc<dyn AsyncComputationSpawner>,
    ) -> Self {
        let partial_witness_tracker =
            PartialEncodedStateWitnessTracker::new(client_sender, epoch_manager.clone());
        Self {
            network_adapter,
            my_signer,
            epoch_manager,
            partial_witness_tracker,
            partial_deploys_tracker: PartialEncodedContractDeploysTracker::new(),
            state_witness_tracker: ChunkStateWitnessTracker::new(clock),
            runtime,
            witness_encoders: ReedSolomonEncoderCache::new(WITNESS_RATIO_DATA_PARTS),
            contract_deploys_encoders: ReedSolomonEncoderCache::new(
                CONTRACT_DEPLOYS_RATIO_DATA_PARTS,
            ),
            compile_contracts_spawner,
        }
    }

    fn handle_distribute_state_witness_request(
        &mut self,
        msg: DistributeStateWitnessRequest,
    ) -> Result<(), Error> {
        let DistributeStateWitnessRequest {
            epoch_id,
            chunk_header,
            state_witness,
            contract_deploys,
        } = msg;

        tracing::debug!(
            target: "client",
            chunk_hash=?chunk_header.chunk_hash(),
            "distribute_chunk_state_witness",
        );

        let signer = self.my_validator_signer()?;
        let witness_bytes = compress_witness(&state_witness)?;

        self.send_state_witness_parts(epoch_id, chunk_header, witness_bytes, &signer)?;

        self.send_chunk_contract_deploys_parts(
            state_witness.chunk_production_key(),
            contract_deploys,
        )?;

        Ok(())
    }

    // Function to generate the parts of the state witness and return them as a tuple of chunk_validator and part.
    fn generate_state_witness_parts(
        &mut self,
        epoch_id: EpochId,
        chunk_header: ShardChunkHeader,
        witness_bytes: EncodedChunkStateWitness,
        signer: &ValidatorSigner,
    ) -> Result<Vec<(AccountId, PartialEncodedStateWitness)>, Error> {
        let chunk_validators = self
            .epoch_manager
            .get_chunk_validator_assignments(
                &epoch_id,
                chunk_header.shard_id(),
                chunk_header.height_created(),
            )?
            .ordered_chunk_validators();

        tracing::debug!(
            target: "client",
            chunk_hash=?chunk_header.chunk_hash(),
            ?chunk_validators,
            "generate_state_witness_parts",
        );

        // Break the state witness into parts using Reed Solomon encoding.
        let encoder = self.witness_encoders.entry(chunk_validators.len());
        let (parts, encoded_length) = encoder.encode(&witness_bytes);

        Ok(chunk_validators
            .iter()
            .zip_eq(parts)
            .enumerate()
            .map(|(part_ord, (chunk_validator, part))| {
                // It's fine to unwrap part here as we just constructed the parts above and we expect
                // all of them to be present.
                let partial_witness = PartialEncodedStateWitness::new(
                    epoch_id,
                    chunk_header.clone(),
                    part_ord,
                    part.unwrap().to_vec(),
                    encoded_length,
                    signer,
                );
                (chunk_validator.clone(), partial_witness)
            })
            .collect_vec())
    }

    fn generate_contract_deploys_parts(
        &mut self,
        key: &ChunkProductionKey,
        deploys: ChunkContractDeploys,
    ) -> Result<Vec<(AccountId, PartialEncodedContractDeploys)>, Error> {
        let validators = self.ordered_contract_deploys_validators(key)?;
        let encoder = self.contract_deploys_encoder(validators.len());
        let (parts, encoded_length) = encoder.encode(&deploys);
        let signer = self.my_validator_signer()?;

        Ok(validators
            .into_iter()
            .zip_eq(parts)
            .enumerate()
            .map(|(part_ord, (validator, part))| {
                let partial_deploys = PartialEncodedContractDeploys::new(
                    key.clone(),
                    PartialEncodedContractDeploysPart {
                        part_ord,
                        data: part.unwrap().to_vec().into_boxed_slice(),
                        encoded_length,
                    },
                    &signer,
                );
                (validator, partial_deploys)
            })
            .collect_vec())
    }

    // Break the state witness into parts and send each part to the corresponding chunk validator owner.
    // The chunk validator owner will then forward the part to all other chunk validators.
    // Each chunk validator would collect the parts and reconstruct the state witness.
    fn send_state_witness_parts(
        &mut self,
        epoch_id: EpochId,
        chunk_header: ShardChunkHeader,
        witness_bytes: EncodedChunkStateWitness,
        signer: &ValidatorSigner,
    ) -> Result<(), Error> {
        // Capture these values first, as the sources are consumed before calling record_witness_sent.
        let chunk_hash = chunk_header.chunk_hash();
        let witness_size_in_bytes = witness_bytes.size_bytes();

        // Record time taken to encode the state witness parts.
        let shard_id_label = chunk_header.shard_id().to_string();
        let encode_timer = metrics::PARTIAL_WITNESS_ENCODE_TIME
            .with_label_values(&[shard_id_label.as_str()])
            .start_timer();
        let validator_witness_tuple =
            self.generate_state_witness_parts(epoch_id, chunk_header, witness_bytes, signer)?;
        encode_timer.observe_duration();

        // Record the witness in order to match the incoming acks for measuring round-trip times.
        // See process_chunk_state_witness_ack for the handling of the ack messages.
        self.state_witness_tracker.record_witness_sent(
            chunk_hash,
            witness_size_in_bytes,
            validator_witness_tuple.len(),
        );

        // Send the parts to the corresponding chunk validator owners.
        self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::PartialEncodedStateWitness(validator_witness_tuple),
        ));
        Ok(())
    }

    /// Sends the witness part to the chunk validators, except the chunk producer that generated the witness part.
    fn forward_state_witness_part(
        &self,
        partial_witness: PartialEncodedStateWitness,
    ) -> Result<(), Error> {
        let ChunkProductionKey { shard_id, epoch_id, height_created } =
            partial_witness.chunk_production_key();
        let chunk_producer =
            self.epoch_manager.get_chunk_producer(&epoch_id, height_created, shard_id)?;

        // Forward witness part to chunk validators except the validator that produced the chunk and witness.
        let target_chunk_validators = self
            .epoch_manager
            .get_chunk_validator_assignments(&epoch_id, shard_id, height_created)?
            .ordered_chunk_validators()
            .into_iter()
            .filter(|validator| validator != &chunk_producer)
            .collect();

        self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::PartialEncodedStateWitnessForward(
                target_chunk_validators,
                partial_witness,
            ),
        ));
        Ok(())
    }

    /// Function to handle receiving partial_encoded_state_witness message from chunk producer.
    fn handle_partial_encoded_state_witness(
        &mut self,
        partial_witness: PartialEncodedStateWitness,
    ) -> Result<(), Error> {
        tracing::debug!(target: "client", ?partial_witness, "Receive PartialEncodedStateWitnessMessage");

        let signer = self.my_validator_signer()?;
        // Validate the partial encoded state witness and forward the part to all the chunk validators.
        if validate_partial_encoded_state_witness(
            self.epoch_manager.as_ref(),
            &partial_witness,
            &signer,
            self.runtime.store(),
        )? {
            self.forward_state_witness_part(partial_witness)?;
        }

        Ok(())
    }

    /// Function to handle receiving partial_encoded_state_witness_forward message from chunk producer.
    fn handle_partial_encoded_state_witness_forward(
        &mut self,
        partial_witness: PartialEncodedStateWitness,
    ) -> Result<(), Error> {
        tracing::debug!(target: "client", ?partial_witness, "Receive PartialEncodedStateWitnessForwardMessage");

        let signer = self.my_validator_signer()?;
        // Validate the partial encoded state witness and store the partial encoded state witness.
        if validate_partial_encoded_state_witness(
            self.epoch_manager.as_ref(),
            &partial_witness,
            &signer,
            self.runtime.store(),
        )? {
            self.partial_witness_tracker.store_partial_encoded_state_witness(partial_witness)?;
        }

        Ok(())
    }

    /// Handles partial contract deploy message received from a peer.
    ///
    /// This message may belong to one of two steps of distributing contract code. In the first step the code is compressed
    /// and encoded into parts using Reed Solomon encoding and each part is sent to one of the validators (part owner).
    /// See `send_chunk_contract_deploys_parts` for the code implementing this. In the second step each validator (part-owner)
    /// forwards the part it receives to other validators.
    fn handle_partial_encoded_contract_deploys(
        &mut self,
        partial_deploys: PartialEncodedContractDeploys,
    ) -> Result<(), Error> {
        tracing::debug!(target: "client", ?partial_deploys, "Receive PartialEncodedContractDeploys");

        let signer = self.my_validator_signer()?;
        if !validate_partial_encoded_contract_deploys(
            self.epoch_manager.as_ref(),
            &partial_deploys,
            &signer,
            self.runtime.store(),
        )? {
            return Ok(());
        }
        let key = partial_deploys.chunk_production_key().clone();
        let validators = self.ordered_contract_deploys_validators(&key)?;

        // Forward to other validators if the part received is my part
        let signer = self.my_validator_signer()?;
        let my_account_id = signer.validator_id();
        let my_part_ord = validators
            .iter()
            .position(|validator| validator == my_account_id)
            .expect("expected to be validated");
        if partial_deploys.part().part_ord == my_part_ord {
            let other_validators = validators
                .iter()
                .filter(|&validator| validator != my_account_id)
                .cloned()
                .collect_vec();
            self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::PartialEncodedContractDeploys(
                    other_validators,
                    partial_deploys.clone(),
                ),
            ));
        }

        // Store part
        let encoder = self.contract_deploys_encoder(validators.len());
        if let Some(deploys) = self
            .partial_deploys_tracker
            .store_partial_encoded_contract_deploys(partial_deploys, encoder)?
        {
            let contracts = match deploys.decompress_contracts() {
                Ok(contracts) => contracts,
                Err(err) => {
                    tracing::warn!(
                        target: "client",
                        ?err,
                        ?key,
                        "Failed to decompress deployed contracts."
                    );
                    return Ok(());
                }
            };
            let contract_codes = contracts.into_iter().map(|contract| contract.into()).collect();
            let runtime = self.runtime.clone();
            self.compile_contracts_spawner.spawn("precompile_deployed_contracts", move || {
                if let Err(err) = runtime.precompile_contracts(&key.epoch_id, contract_codes) {
                    tracing::error!(
                        target: "client",
                        ?err,
                        ?key,
                        "Failed to precompile deployed contracts."
                    );
                }
            });
        }

        Ok(())
    }

    /// Handles the state witness ack message from the chunk validator.
    /// It computes the round-trip time between sending the state witness and receiving
    /// the ack message and updates the corresponding metric with it.
    /// Currently we do not raise an error for handling of witness-ack messages,
    /// as it is used only for tracking some networking metrics.
    fn handle_chunk_state_witness_ack(&mut self, witness_ack: ChunkStateWitnessAck) {
        self.state_witness_tracker.on_witness_ack_received(witness_ack);
    }

    /// Handles contract code accesses message from chunk producer.
    /// This is sent in parallel to a chunk state witness and contains the hashes
    /// of the contract code accessed when applying the previous chunk of the witness.
    fn handle_chunk_contract_accesses(
        &mut self,
        accesses: ChunkContractAccesses,
    ) -> Result<(), Error> {
        let signer = self.my_validator_signer()?;
        if !validate_chunk_contract_accesses(
            self.epoch_manager.as_ref(),
            &accesses,
            &signer,
            self.runtime.store(),
        )? {
            return Ok(());
        }
        let key = accesses.chunk_production_key();
        let contracts_cache = self.runtime.compiled_contract_cache();
        let runtime_config = self
            .runtime
            .get_runtime_config(self.epoch_manager.get_epoch_protocol_version(&key.epoch_id)?)?;
        let missing_contract_hashes = HashSet::from_iter(
            accesses
                .contracts()
                .iter()
                .filter(|&hash| {
                    !contracts_cache_contains_contract(contracts_cache, hash, &runtime_config)
                })
                .cloned(),
        );
        if missing_contract_hashes.is_empty() {
            return Ok(());
        }
        self.partial_witness_tracker
            .store_accessed_contract_hashes(key.clone(), missing_contract_hashes.clone())?;
        let random_chunk_producer =
            self.epoch_manager.get_random_chunk_producer_for_shard(&key.epoch_id, key.shard_id)?;
        let request = ContractCodeRequest::new(key.clone(), missing_contract_hashes, &signer);
        self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::ContractCodeRequest(random_chunk_producer, request),
        ));
        Ok(())
    }

    /// Retrieves the code for the given contract hashes and distributes them to validator in parts.
    ///
    /// This implements the first step of distributing contract code to validators where the contract codes
    /// are compressed and encoded into parts using Reed Solomon encoding, and then each part is sent to
    /// one of the validators (part-owner). Second step of the distribution, where each validator (part-owner)
    /// forwards the part it receives is implemented in `handle_partial_encoded_contract_deploys`.
    fn send_chunk_contract_deploys_parts(
        &mut self,
        key: ChunkProductionKey,
        contract_codes: Vec<ContractCode>,
    ) -> Result<(), Error> {
        if contract_codes.is_empty() {
            return Ok(());
        }
        let contracts = contract_codes.into_iter().map(|contract| contract.into()).collect();
        let compressed_deploys = ChunkContractDeploys::compress_contracts(&contracts)?;
        let validator_parts = self.generate_contract_deploys_parts(&key, compressed_deploys)?;
        for (part_owner, deploys_part) in validator_parts.into_iter() {
            self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::PartialEncodedContractDeploys(vec![part_owner], deploys_part),
            ));
        }
        Ok(())
    }

    /// Handles contract code requests message from chunk validators.
    /// As response to this message, sends the contract code requested to
    /// the requesting chunk validator for the given hashes of the contract code.
    fn handle_contract_code_request(&mut self, request: ContractCodeRequest) -> Result<(), Error> {
        let signer = self.my_validator_signer()?;
        // TODO(#11099): validate request
        let key = request.chunk_production_key();
        let storage = TrieDBStorage::new(
            TrieStoreAdapter::new(self.runtime.store().clone()),
            self.epoch_manager.shard_id_to_uid(key.shard_id, &key.epoch_id)?,
        );
        let mut contracts = Vec::new();
        for contract_hash in request.contracts() {
            match storage.retrieve_raw_bytes(&contract_hash.0) {
                Ok(bytes) => contracts.push(CodeBytes(bytes)),
                Err(StorageError::MissingTrieValue(_, _)) => {
                    tracing::warn!(
                        target: "client",
                        ?contract_hash,
                        chunk_production_key = ?key,
                        "Requested contract hash is not present in the storage"
                    );
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            }
        }
        let response = ContractCodeResponse::new(key.clone(), &contracts, &signer);
        self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
            NetworkRequests::ContractCodeResponse(request.requester().clone(), response),
        ));
        Ok(())
    }

    /// Handles contract code responses message from chunk producer.
    fn handle_contract_code_response(
        &mut self,
        response: ContractCodeResponse,
    ) -> Result<(), Error> {
        // TODO(#11099): validate response
        let key = response.chunk_production_key().clone();
        let contracts = response.decompress_contracts()?;
        self.partial_witness_tracker.store_accessed_contract_codes(key, contracts)
    }

    fn my_validator_signer(&self) -> Result<Arc<ValidatorSigner>, Error> {
        self.my_signer.get().ok_or_else(|| Error::NotAValidator("not a validator".to_owned()))
    }

    fn contract_deploys_encoder(&mut self, validators_count: usize) -> Arc<ReedSolomonEncoder> {
        self.contract_deploys_encoders.entry(validators_count)
    }

    fn ordered_contract_deploys_validators(
        &mut self,
        key: &ChunkProductionKey,
    ) -> Result<Vec<AccountId>, Error> {
        let chunk_producers = HashSet::<AccountId>::from_iter(
            self.epoch_manager.get_epoch_chunk_producers_for_shard(&key.epoch_id, key.shard_id)?,
        );
        let mut validators = self
            .epoch_manager
            .get_epoch_all_validators(&key.epoch_id)?
            .into_iter()
            .filter(|stake| !chunk_producers.contains(stake.account_id()))
            .map(|stake| stake.account_id().clone())
            .collect::<Vec<_>>();
        validators.sort();
        Ok(validators)
    }
}

fn compress_witness(witness: &ChunkStateWitness) -> Result<EncodedChunkStateWitness, Error> {
    let shard_id_label = witness.chunk_header.shard_id().to_string();
    let encode_timer = near_chain::stateless_validation::metrics::CHUNK_STATE_WITNESS_ENCODE_TIME
        .with_label_values(&[shard_id_label.as_str()])
        .start_timer();
    let (witness_bytes, raw_witness_size) = EncodedChunkStateWitness::encode(&witness)?;
    encode_timer.observe_duration();

    near_chain::stateless_validation::metrics::record_witness_size_metrics(
        raw_witness_size,
        witness_bytes.size_bytes(),
        witness,
    );
    Ok(witness_bytes)
}

fn contracts_cache_contains_contract(
    cache: &dyn ContractRuntimeCache,
    contract_hash: &CodeHash,
    runtime_config: &RuntimeConfig,
) -> bool {
    let cache_key = get_contract_cache_key(contract_hash.0, &runtime_config.wasm_config);
    cache.memory_cache().contains(cache_key) || cache.has(&cache_key).is_ok_and(|has| has)
}
