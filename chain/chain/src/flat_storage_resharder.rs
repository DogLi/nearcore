//! Logic for resharding flat storage in parallel to chain processing.
//!
//! See [FlatStorageResharder] for more details about how the resharding takes place.

use std::sync::{Arc, Mutex};

use near_chain_configs::{MutableConfigValue, ReshardingConfig, ReshardingHandle};
use near_chain_primitives::Error;

use tracing::{debug, error, info};

use crate::resharding::event_type::{ReshardingEventType, ReshardingSplitShardParams};
use crate::resharding::types::FlatStorageSplitShardRequest;
use crate::types::RuntimeAdapter;
use itertools::Itertools;
use near_async::messaging::Sender;
use near_primitives::hash::CryptoHash;
use near_primitives::shard_layout::{account_id_to_shard_id, ShardLayout};
use near_primitives::state::FlatStateValue;
use near_primitives::trie_key::col::{self, ALL_COLUMNS_WITH_NAMES};
use near_primitives::trie_key::trie_key_parsers::{
    parse_account_id_from_access_key_key, parse_account_id_from_account_key,
    parse_account_id_from_contract_code_key, parse_account_id_from_contract_data_key,
    parse_account_id_from_received_data_key, parse_account_id_from_trie_key_with_separator,
};
use near_primitives::types::AccountId;
use near_store::adapter::flat_store::{FlatStoreAdapter, FlatStoreUpdateAdapter};
use near_store::adapter::StoreAdapter;
use near_store::flat::{
    BlockInfo, FlatStorageError, FlatStorageReadyStatus, FlatStorageReshardingStatus,
    FlatStorageStatus, SplittingParentStatus,
};
use near_store::{ShardUId, StorageError};
use std::fmt::{Debug, Formatter};
use std::iter;

/// `FlatStorageResharder` takes care of updating flat storage when a resharding event happens.
///
/// On an high level, the events supported are:
/// - #### Shard splitting
///     Parent shard must be split into two children. The entire operation freezes the flat storage
///     for the involved shards. Children shards are created empty and the key-values of the parent
///     will be copied into one of them, in the background.
///
///     After the copy is finished the children shard will have the correct state at some past block
///     height. It'll be necessary to perform catchup before the flat storage can be put again in
///     Ready state. The parent shard storage is not needed anymore and can be removed.
///
/// The resharder has also the following properties:
/// - Background processing: the bulk of resharding is done in a separate task.
/// - Interruptible: a reshard operation can be cancelled through a
///   [FlatStorageResharderController].
///     - In the case of event `Split` the state of flat storage will go back to what it was
///       previously.
#[derive(Clone)]
pub struct FlatStorageResharder {
    runtime: Arc<dyn RuntimeAdapter>,
    /// The current active resharding event.
    resharding_event: Arc<Mutex<Option<FlatStorageReshardingEventStatus>>>,
    /// Sender responsible to convey requests to the dedicated resharding actor.
    scheduler: Sender<FlatStorageSplitShardRequest>,
    /// Controls cancellation of background processing.
    pub controller: FlatStorageResharderController,
    /// Configuration for resharding.
    resharding_config: MutableConfigValue<ReshardingConfig>,
}

impl FlatStorageResharder {
    /// Creates a new `FlatStorageResharder`.
    ///
    /// # Args:
    /// * `runtime`: runtime adapter
    /// * `scheduler`: component used to schedule the background tasks
    /// * `controller`: manages the execution of the background tasks
    /// * `resharing_config`: configuration options
    pub fn new(
        runtime: Arc<dyn RuntimeAdapter>,
        scheduler: Sender<FlatStorageSplitShardRequest>,
        controller: FlatStorageResharderController,
        resharding_config: MutableConfigValue<ReshardingConfig>,
    ) -> Self {
        let resharding_event = Arc::new(Mutex::new(None));
        Self { runtime, resharding_event, scheduler, controller, resharding_config }
    }

    /// Starts a resharding event.
    ///
    /// For now, only splitting a shard is supported.
    ///
    /// # Args:
    /// * `event_type`: the type of resharding event
    /// * `shard_layout`: the new shard layout
    pub fn start_resharding(
        &self,
        event_type: ReshardingEventType,
        shard_layout: &ShardLayout,
    ) -> Result<(), Error> {
        match event_type {
            ReshardingEventType::SplitShard(params) => self.split_shard(params, shard_layout),
        }
    }

    /// Resumes a resharding event that was interrupted.
    ///
    /// Flat-storage resharding will resume upon a node crash.
    ///
    /// # Args:
    /// * `shard_uid`: UId of the shard
    /// * `status`: resharding status of the shard
    pub fn resume(
        &self,
        shard_uid: ShardUId,
        status: &FlatStorageReshardingStatus,
    ) -> Result<(), Error> {
        match status {
            FlatStorageReshardingStatus::CreatingChild => {
                // Nothing to do here because the parent will take care of resuming work.
            }
            FlatStorageReshardingStatus::SplittingParent(status) => {
                let parent_shard_uid = shard_uid;
                info!(target: "resharding", ?parent_shard_uid, ?status, "resuming flat storage shard split");
                self.check_no_resharding_in_progress()?;
                // On resume flat storage status is already set.
                // However, we don't know the current state of children shards,
                // so it's better to clean them.
                self.clean_children_shards(&status)?;
                self.schedule_split_shard(parent_shard_uid, &status);
            }
            FlatStorageReshardingStatus::CatchingUp(_) => {
                info!(target: "resharding", ?shard_uid, ?status, "resuming flat storage shard catchup");
                // TODO(Trisfald): implement child catch up
                todo!()
            }
        }
        Ok(())
    }

    /// Starts the event of splitting a parent shard flat storage into two children.
    fn split_shard(
        &self,
        split_params: ReshardingSplitShardParams,
        shard_layout: &ShardLayout,
    ) -> Result<(), Error> {
        let ReshardingSplitShardParams {
            parent_shard,
            left_child_shard,
            right_child_shard,
            block_hash,
            prev_block_hash,
            ..
        } = split_params;
        info!(target: "resharding", ?split_params, "initiating flat storage shard split");
        self.check_no_resharding_in_progress()?;

        // Change parent and children shards flat storage status.
        let store = self.runtime.store().flat_store();
        let mut store_update = store.store_update();
        let flat_head = retrieve_shard_flat_head(parent_shard, &store)?;
        let status = SplittingParentStatus {
            left_child_shard,
            right_child_shard,
            shard_layout: shard_layout.clone(),
            block_hash,
            prev_block_hash,
            flat_head,
        };
        store_update.set_flat_storage_status(
            parent_shard,
            FlatStorageStatus::Resharding(FlatStorageReshardingStatus::SplittingParent(
                status.clone(),
            )),
        );
        store_update.set_flat_storage_status(
            left_child_shard,
            FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CreatingChild),
        );
        store_update.set_flat_storage_status(
            right_child_shard,
            FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CreatingChild),
        );
        store_update.commit()?;

        self.schedule_split_shard(parent_shard, &status);
        Ok(())
    }

    /// Returns an error if a resharding event is in progress.
    fn check_no_resharding_in_progress(&self) -> Result<(), StorageError> {
        // Do not allow multiple resharding events in parallel.
        if self.resharding_event().is_some() {
            error!(target: "resharding", "trying to start a new flat storage resharding event while one is already in progress!");
            Err(StorageError::FlatStorageReshardingAlreadyInProgress)
        } else {
            Ok(())
        }
    }

    fn set_resharding_event(&self, event: FlatStorageReshardingEventStatus) {
        *self.resharding_event.lock().unwrap() = Some(event);
    }

    /// Returns the current in-progress resharding event, if any.
    pub fn resharding_event(&self) -> Option<FlatStorageReshardingEventStatus> {
        self.resharding_event.lock().unwrap().clone()
    }

    /// Schedules a task to split a shard.
    fn schedule_split_shard(&self, parent_shard: ShardUId, status: &SplittingParentStatus) {
        let event = FlatStorageReshardingEventStatus::SplitShard(parent_shard, status.clone());
        self.set_resharding_event(event);
        info!(target: "resharding", ?parent_shard, ?status,"scheduling flat storage shard split");
        let resharder = self.clone();
        self.scheduler.send(FlatStorageSplitShardRequest { resharder });
    }

    /// Cleans up children shards flat storage's content (status is excluded).
    #[tracing::instrument(
        level = "info",
        target = "resharding",
        "FlatStorageResharder::clean_children_shards",
        skip_all
    )]
    fn clean_children_shards(&self, status: &SplittingParentStatus) -> Result<(), Error> {
        let SplittingParentStatus { left_child_shard, right_child_shard, .. } = status;
        info!(target: "resharding", ?left_child_shard, ?right_child_shard, "cleaning up children shards flat storage's content");
        let mut store_update = self.runtime.store().flat_store().store_update();
        for child in [left_child_shard, right_child_shard] {
            store_update.remove_all_deltas(*child);
            store_update.remove_all_values(*child);
        }
        store_update.commit()?;
        Ok(())
    }

    /// Retrieves parent shard UIds and current resharding event status, only if a resharding event
    /// is in progress and of type `Split`.
    fn get_parent_shard_and_status(&self) -> Option<(ShardUId, SplittingParentStatus)> {
        let event = self.resharding_event.lock().unwrap();
        match event.as_ref() {
            Some(FlatStorageReshardingEventStatus::SplitShard(parent_shard, status)) => {
                Some((*parent_shard, status.clone()))
            }
            None => None,
        }
    }

    /// Task to perform the actual split of a flat storage shard. This may be a long operation time-wise.
    ///
    /// Conceptually it simply copies each key-value pair from the parent shard to the correct child.
    pub fn split_shard_task(&self) -> FlatStorageReshardingTaskStatus {
        let task_status = self.split_shard_task_impl();
        self.split_shard_task_postprocessing(task_status);
        info!(target: "resharding", ?task_status, "flat storage shard split task finished");
        task_status
    }

    /// Performs the bulk of [split_shard_task].
    ///
    /// Returns `true` if the routine completed successfully.
    fn split_shard_task_impl(&self) -> FlatStorageReshardingTaskStatus {
        if self.controller.is_cancelled() {
            return FlatStorageReshardingTaskStatus::Cancelled;
        }

        // Determines after how many bytes worth of key-values the process stops to commit changes
        // and to check cancellation.
        let batch_size = self.resharding_config.get().batch_size.as_u64() as usize;
        // Delay between every batch.
        let batch_delay = self.resharding_config.get().batch_delay.unsigned_abs();

        let (parent_shard, status) = self
            .get_parent_shard_and_status()
            .expect("flat storage resharding event must be Split!");
        info!(target: "resharding", ?parent_shard, ?status, ?batch_delay, ?batch_size, "flat storage shard split task: starting key-values copy");

        // Prepare the store object for commits and the iterator over parent's flat storage.
        let flat_store = self.runtime.store().flat_store();
        let mut iter = match self.flat_storage_iterator(
            &flat_store,
            &parent_shard,
            &status.block_hash,
        ) {
            Ok(iter) => iter,
            Err(err) => {
                error!(target: "resharding", ?parent_shard, block_hash=?status.block_hash, ?err, "failed to build flat storage iterator");
                return FlatStorageReshardingTaskStatus::Failed;
            }
        };

        let mut num_batches_done: usize = 0;
        let mut iter_exhausted = false;

        loop {
            let _span = tracing::debug_span!(
                target: "resharding",
                "split_shard_task_impl/batch",
                batch_id = ?num_batches_done)
            .entered();
            let mut store_update = flat_store.store_update();
            let mut processed_size = 0;

            // Process a `batch_size` worth of key value pairs.
            while processed_size < batch_size && !iter_exhausted {
                match iter.next() {
                    // Stop iterating and commit the batch.
                    Some(FlatStorageAndDeltaIterItem::CommitPoint) => break,
                    Some(FlatStorageAndDeltaIterItem::Entry(Ok((key, value)))) => {
                        processed_size += key.len() + value.as_ref().map_or(0, |v| v.size());
                        if let Err(err) =
                            shard_split_handle_key_value(key, value, &mut store_update, &status)
                        {
                            error!(target: "resharding", ?err, "failed to handle flat storage key");
                            return FlatStorageReshardingTaskStatus::Failed;
                        }
                    }
                    Some(FlatStorageAndDeltaIterItem::Entry(Err(err))) => {
                        error!(target: "resharding", ?err, "failed to read flat storage value from parent shard");
                        return FlatStorageReshardingTaskStatus::Failed;
                    }
                    None => {
                        iter_exhausted = true;
                    }
                }
            }

            // Make a pause to commit and check if the routine should stop.
            if let Err(err) = store_update.commit() {
                error!(target: "resharding", ?err, "failed to commit store update");
                return FlatStorageReshardingTaskStatus::Failed;
            }

            num_batches_done += 1;

            // If `iter`` is exhausted we can exit after the store commit.
            if iter_exhausted {
                return FlatStorageReshardingTaskStatus::Successful { num_batches_done };
            }
            if self.controller.is_cancelled() {
                return FlatStorageReshardingTaskStatus::Cancelled;
            }

            // Sleep between batches in order to throttle resharding and leave some resource for the
            // regular node operation.
            std::thread::sleep(batch_delay);
        }
    }

    /// Performs post-processing of shard splitting after all key-values have been moved from parent to
    /// children. `success` indicates whether or not the previous phase was successful.
    #[tracing::instrument(
        level = "info",
        target = "resharding",
        "FlatStorageResharder::split_shard_task_postprocessing",
        skip_all
    )]
    fn split_shard_task_postprocessing(&self, task_status: FlatStorageReshardingTaskStatus) {
        let (parent_shard, split_status) = self
            .get_parent_shard_and_status()
            .expect("flat storage resharding event must be Split!");
        let SplittingParentStatus { left_child_shard, right_child_shard, flat_head, .. } =
            split_status;
        let flat_store = self.runtime.store().flat_store();
        info!(target: "resharding", ?parent_shard, ?task_status, ?split_status, "flat storage shard split task: post-processing");

        let mut store_update = flat_store.store_update();
        match task_status {
            FlatStorageReshardingTaskStatus::Successful { .. } => {
                // Split shard completed successfully.
                // Parent flat storage can be deleted from the FlatStoreManager.
                // If FlatStoreManager has no reference to the shard, delete it manually.
                if !self
                    .runtime
                    .get_flat_storage_manager()
                    .remove_flat_storage_for_shard(parent_shard, &mut store_update)
                    .unwrap()
                {
                    store_update.remove_flat_storage(parent_shard);
                }
                // Children must perform catchup.
                for child_shard in [left_child_shard, right_child_shard] {
                    store_update.set_flat_storage_status(
                        child_shard,
                        FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CatchingUp(
                            flat_head.hash,
                        )),
                    );
                }
                // TODO(trisfald): trigger catchup
            }
            FlatStorageReshardingTaskStatus::Failed
            | FlatStorageReshardingTaskStatus::Cancelled => {
                // We got an error or a cancellation request.
                // Reset parent.
                store_update.set_flat_storage_status(
                    parent_shard,
                    FlatStorageStatus::Ready(FlatStorageReadyStatus { flat_head }),
                );
                // Remove children shards leftovers.
                for child_shard in [left_child_shard, right_child_shard] {
                    store_update.remove_flat_storage(child_shard);
                }
            }
        }
        store_update.commit().unwrap();
        // Terminate the resharding event.
        *self.resharding_event.lock().unwrap() = None;
    }

    /// Returns an iterator over a shard's flat storage at the given block hash. This
    /// iterator contains both flat storage values and deltas.
    fn flat_storage_iterator<'a>(
        &self,
        flat_store: &'a FlatStoreAdapter,
        shard_uid: &ShardUId,
        block_hash: &CryptoHash,
    ) -> Result<Box<FlatStorageAndDeltaIter<'a>>, Error> {
        let mut iter: Box<FlatStorageAndDeltaIter<'a>> = Box::new(
            flat_store
                .iter(*shard_uid)
                // Get the flat storage iter and wrap the value in Optional::Some to
                // match the delta iterator so that they can be chained.
                .map_ok(|(key, value)| (key, Some(value)))
                // Wrap the iterator's item into an Entry.
                .map(|entry| FlatStorageAndDeltaIterItem::Entry(entry)),
        );

        // Get all the blocks from flat head to the wanted block hash.
        let flat_storage = self
            .runtime
            .get_flat_storage_manager()
            .get_flat_storage_for_shard(*shard_uid)
            .expect("the flat storage undergoing resharding must exist!");
        // Must reverse the result because we want ascending block heights.
        let mut blocks_to_head = flat_storage.get_blocks_to_head(block_hash).map_err(|err| {
            StorageError::StorageInconsistentState(format!(
                "failed to find path of blocks to flat storage head ({err})"
            ))
        })?;
        blocks_to_head.reverse();
        debug!(target = "resharding", "flat storage blocks to head len = {}", blocks_to_head.len());

        // Get all the delta iterators and wrap the items in Result to match the flat
        // storage iter so that they can be chained.
        for block in blocks_to_head {
            let deltas = flat_store.get_delta(*shard_uid, block).map_err(|err| {
                StorageError::StorageInconsistentState(format!(
                    "can't retrieve deltas for flat storage at {block}/{shard_uid:?}({err})"
                ))
            })?;
            let Some(deltas) = deltas else {
                continue;
            };
            // Chain the iterators effectively adding a block worth of deltas.
            // Before doing so insert a commit point to separate changes to the same key in different transactions.
            iter = Box::new(iter.chain(iter::once(FlatStorageAndDeltaIterItem::CommitPoint)));
            let deltas_iter = deltas.0.into_iter();
            let deltas_iter = deltas_iter.map(|item| FlatStorageAndDeltaIterItem::Entry(Ok(item)));
            iter = Box::new(iter.chain(deltas_iter));
        }

        Ok(iter)
    }
}

/// Enum used to wrap the `Item` of iterators over flat storage contents or flat storage deltas. Its
/// purpose is to insert a marker to force store commits during iteration over all entries. This is
/// necessary because otherwise deltas might set again the value of a flat storage entry inside the
/// same transaction.
enum FlatStorageAndDeltaIterItem {
    Entry(Result<(Vec<u8>, Option<FlatStateValue>), FlatStorageError>),
    CommitPoint,
}

type FlatStorageAndDeltaIter<'a> = dyn Iterator<Item = FlatStorageAndDeltaIterItem> + 'a;

impl Debug for FlatStorageResharder {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlatStorageResharder")
            .field("event", &self.resharding_event())
            .field("controller", &self.controller)
            .finish()
    }
}

/// Retrieves the flat head of the given `shard`.
/// The shard must be in [FlatStorageStatus::Ready] state otherwise this method returns an error.
fn retrieve_shard_flat_head(shard: ShardUId, store: &FlatStoreAdapter) -> Result<BlockInfo, Error> {
    let status =
        store.get_flat_storage_status(shard).map_err(|err| Into::<StorageError>::into(err))?;
    if let FlatStorageStatus::Ready(FlatStorageReadyStatus { flat_head }) = status {
        Ok(flat_head)
    } else {
        let err_msg = "flat storage shard status is not ready!";
        error!(target: "resharding", ?shard, ?status, err_msg);
        Err(Error::ReshardingError(err_msg.to_owned()))
    }
}

/// Handles the inheritance of a key-value pair from parent shard to children shards.
fn shard_split_handle_key_value(
    key: Vec<u8>,
    value: Option<FlatStateValue>,
    store_update: &mut FlatStoreUpdateAdapter,
    status: &SplittingParentStatus,
) -> Result<(), Error> {
    if key.is_empty() {
        panic!("flat storage key is empty!")
    }
    let key_column_prefix = key[0];

    match key_column_prefix {
        col::ACCOUNT => {
            copy_kv_to_child(&status, key, value, store_update, parse_account_id_from_account_key)?
        }
        col::CONTRACT_DATA => copy_kv_to_child(
            &status,
            key,
            value,
            store_update,
            parse_account_id_from_contract_data_key,
        )?,
        col::CONTRACT_CODE => copy_kv_to_child(
            &status,
            key,
            value,
            store_update,
            parse_account_id_from_contract_code_key,
        )?,
        col::ACCESS_KEY => copy_kv_to_child(
            &status,
            key,
            value,
            store_update,
            parse_account_id_from_access_key_key,
        )?,
        col::RECEIVED_DATA => copy_kv_to_child(
            &status,
            key,
            value,
            store_update,
            parse_account_id_from_received_data_key,
        )?,
        col::POSTPONED_RECEIPT_ID | col::PENDING_DATA_COUNT | col::POSTPONED_RECEIPT => {
            copy_kv_to_child(&status, key, value, store_update, |raw_key: &[u8]| {
                parse_account_id_from_trie_key_with_separator(
                    key_column_prefix,
                    raw_key,
                    ALL_COLUMNS_WITH_NAMES[key_column_prefix as usize].1,
                )
            })?
        }
        col::DELAYED_RECEIPT_OR_INDICES
        | col::PROMISE_YIELD_INDICES
        | col::PROMISE_YIELD_TIMEOUT
        | col::PROMISE_YIELD_RECEIPT => copy_kv_to_all_children(&status, key, value, store_update),
        col::BUFFERED_RECEIPT_INDICES | col::BUFFERED_RECEIPT => {
            copy_kv_to_left_child(&status, key, value, store_update)
        }
        _ => unreachable!(),
    }
    Ok(())
}

/// Copies a key-value pair to the correct child shard by matching the account-id to the provided shard layout.
fn copy_kv_to_child(
    status: &SplittingParentStatus,
    key: Vec<u8>,
    value: Option<FlatStateValue>,
    store_update: &mut FlatStoreUpdateAdapter,
    account_id_parser: impl FnOnce(&[u8]) -> Result<AccountId, std::io::Error>,
) -> Result<(), Error> {
    let SplittingParentStatus { left_child_shard, right_child_shard, shard_layout, .. } = &status;
    // Derive the shard uid for this account in the new shard layout.
    let account_id = account_id_parser(&key)?;
    let new_shard_id = account_id_to_shard_id(&account_id, shard_layout);
    let new_shard_uid = ShardUId::from_shard_id_and_layout(new_shard_id, &shard_layout);

    // Sanity check we are truly writing to one of the expected children shards.
    if new_shard_uid != *left_child_shard && new_shard_uid != *right_child_shard {
        let err_msg = "account id doesn't map to any child shard!";
        error!(target: "resharding", ?new_shard_uid, ?left_child_shard, ?right_child_shard, ?shard_layout, ?account_id, err_msg);
        return Err(Error::ReshardingError(err_msg.to_string()));
    }
    // Add the new flat store entry.
    store_update.set(new_shard_uid, key, value);
    Ok(())
}

/// Copies a key-value pair to both children.
fn copy_kv_to_all_children(
    status: &SplittingParentStatus,
    key: Vec<u8>,
    value: Option<FlatStateValue>,
    store_update: &mut FlatStoreUpdateAdapter,
) {
    store_update.set(status.left_child_shard, key.clone(), value.clone());
    store_update.set(status.right_child_shard, key, value);
}

/// Copies a key-value pair to the child on the left of the account boundary (also called 'first child').
fn copy_kv_to_left_child(
    status: &SplittingParentStatus,
    key: Vec<u8>,
    value: Option<FlatStateValue>,
    store_update: &mut FlatStoreUpdateAdapter,
) {
    store_update.set(status.left_child_shard, key, value);
}

/// Struct to describe, perform and track progress of a flat storage resharding.
#[derive(Clone, Debug)]
pub enum FlatStorageReshardingEventStatus {
    /// Split a shard.
    /// Includes the parent shard uid and the operation' status.
    SplitShard(ShardUId, SplittingParentStatus),
}

/// Status of a flat storage resharding task.
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub enum FlatStorageReshardingTaskStatus {
    Successful { num_batches_done: usize },
    Failed,
    Cancelled,
}

/// Helps control the flat storage resharder background operations. This struct wraps
/// [ReshardingHandle] and gives better meaning request to stop any processing when applied to flat
/// storage. In flat storage resharding there's a slight difference between interrupt and cancel.
/// Interruption happens when the node crashes whilst cancellation is an on demand request. An
/// interrupted flat storage resharding will resume on node restart, a cancelled one won't.
#[derive(Clone, Debug)]
pub struct FlatStorageResharderController {
    /// Resharding handle to control cancellation.
    handle: ReshardingHandle,
}

impl FlatStorageResharderController {
    /// Creates a new `FlatStorageResharderController` with its own handle.
    pub fn new() -> Self {
        let handle = ReshardingHandle::new();
        Self { handle }
    }

    pub fn from_resharding_handle(handle: ReshardingHandle) -> Self {
        Self { handle }
    }

    /// Returns whether or not background task is cancelled.
    pub fn is_cancelled(&self) -> bool {
        !self.handle.get()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use near_async::time::Clock;
    use near_chain_configs::{Genesis, MutableConfigValue};
    use near_epoch_manager::{shard_tracker::ShardTracker, EpochManager};
    use near_o11y::testonly::init_test_logger;
    use near_primitives::{
        hash::CryptoHash,
        shard_layout::ShardLayout,
        state::FlatStateValue,
        test_utils::{create_test_signer, TestBlockBuilder},
        trie_key::TrieKey,
        types::{AccountId, RawStateChange, RawStateChangesWithTrieKey, ShardId, StateChangeCause},
    };
    use near_store::{
        flat::{BlockInfo, FlatStorageReadyStatus},
        genesis::initialize_genesis_state,
        test_utils::create_test_store,
    };

    use crate::{
        rayon_spawner::RayonAsyncComputationSpawner, resharding::types::ReshardingSender,
        runtime::NightshadeRuntime, types::ChainConfig, Chain, ChainGenesis, DoomslugThresholdMode,
    };

    use super::*;
    use near_async::messaging::{CanSend, IntoMultiSender};
    use near_crypto::{KeyType, PublicKey};

    /// Shorthand to create account ID.
    macro_rules! account {
        ($str:expr) => {
            $str.parse::<AccountId>().unwrap()
        };
    }

    #[derive(Default)]
    struct TestScheduler {}

    impl CanSend<FlatStorageSplitShardRequest> for TestScheduler {
        fn send(&self, msg: FlatStorageSplitShardRequest) {
            msg.resharder.split_shard_task();
        }
    }

    #[derive(Default)]
    struct DelayedScheduler {
        split_shard_request: Mutex<Option<FlatStorageSplitShardRequest>>,
    }

    impl DelayedScheduler {
        fn call_split_shard_task(&self) -> FlatStorageReshardingTaskStatus {
            let msg_guard = self.split_shard_request.lock().unwrap();
            msg_guard.as_ref().unwrap().resharder.split_shard_task()
        }
    }

    impl CanSend<FlatStorageSplitShardRequest> for DelayedScheduler {
        fn send(&self, msg: FlatStorageSplitShardRequest) {
            *self.split_shard_request.lock().unwrap() = Some(msg);
        }
    }

    /// Simple shard layout with two shards.
    fn simple_shard_layout() -> ShardLayout {
        let s0 = ShardId::new(0);
        let s1 = ShardId::new(1);
        let shards_split_map = BTreeMap::from([(s0, vec![s0]), (s1, vec![s1])]);
        ShardLayout::v2(vec![account!("ff")], vec![s0, s1], Some(shards_split_map))
    }

    /// Derived from [simple_shard_layout] by splitting the second shard.
    fn shard_layout_after_split() -> ShardLayout {
        let s0 = ShardId::new(0);
        let s1 = ShardId::new(1);
        let s2 = ShardId::new(2);
        let s3 = ShardId::new(3);

        let shards_split_map = BTreeMap::from([(s0, vec![s0]), (s1, vec![s2, s3])]);
        ShardLayout::v2(
            vec![account!("ff"), account!("pp")],
            vec![s0, s2, s3],
            Some(shards_split_map),
        )
    }

    /// Generic test setup. It creates an instance of chain and a FlatStorageResharder.
    fn create_chain_and_resharder(
        shard_layout: ShardLayout,
        resharding_sender: ReshardingSender,
    ) -> (Chain, FlatStorageResharder) {
        let num_shards = shard_layout.shard_ids().count();
        let genesis = Genesis::test_with_seeds(
            Clock::real(),
            vec![account!("aa"), account!("mm"), account!("vv")],
            1,
            vec![1; num_shards],
            shard_layout.clone(),
        );
        let tempdir = tempfile::tempdir().unwrap();
        let store = create_test_store();
        initialize_genesis_state(store.clone(), &genesis, Some(tempdir.path()));
        let epoch_manager = EpochManager::new_arc_handle(store.clone(), &genesis.config, None);
        let shard_tracker = ShardTracker::new_empty(epoch_manager.clone());
        let runtime =
            NightshadeRuntime::test(tempdir.path(), store, &genesis.config, epoch_manager.clone());
        let chain_genesis = ChainGenesis::new(&genesis.config);
        let chain = Chain::new(
            Clock::real(),
            epoch_manager,
            shard_tracker,
            runtime,
            &chain_genesis,
            DoomslugThresholdMode::NoApprovals,
            ChainConfig::test(),
            None,
            Arc::new(RayonAsyncComputationSpawner),
            MutableConfigValue::new(None, "validator_signer"),
            resharding_sender,
        )
        .unwrap();
        for shard_uid in shard_layout.shard_uids() {
            chain
                .runtime_adapter
                .get_flat_storage_manager()
                .create_flat_storage_for_shard(shard_uid)
                .unwrap();
        }
        let resharder = chain.resharding_manager.flat_storage_resharder.clone();
        (chain, resharder)
    }

    /// Utility function to derive the resharding event type from chain and shard layout.
    fn event_type_from_chain_and_layout(
        chain: &Chain,
        new_shard_layout: &ShardLayout,
    ) -> ReshardingEventType {
        ReshardingEventType::from_shard_layout(
            &new_shard_layout,
            chain.head().unwrap().last_block_hash,
            chain.head().unwrap().prev_block_hash,
        )
        .unwrap()
        .unwrap()
    }

    /// Verify that another resharding can't be triggered if one is ongoing.
    #[test]
    fn concurrent_reshardings_are_disallowed() {
        init_test_logger();
        let sender = DelayedScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let controller = FlatStorageResharderController::new();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        assert!(resharder
            .start_resharding(resharding_event_type.clone(), &new_shard_layout)
            .is_ok());

        // Immediately cancel the resharding.
        controller.handle.stop();

        assert!(resharder.resharding_event().is_some());
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_err());
    }

    /// Flat storage shard status should be set correctly upon starting a shard split.
    #[test]
    fn flat_storage_split_status_set() {
        init_test_logger();
        let sender = DelayedScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let flat_store = resharder.runtime.store().flat_store();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        let resharding_event = resharder.resharding_event();
        match resharding_event.unwrap() {
            FlatStorageReshardingEventStatus::SplitShard(parent, status) => {
                assert_eq!(
                    flat_store.get_flat_storage_status(parent),
                    Ok(FlatStorageStatus::Resharding(
                        FlatStorageReshardingStatus::SplittingParent(status.clone())
                    ))
                );
                assert_eq!(
                    flat_store.get_flat_storage_status(status.left_child_shard),
                    Ok(FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CreatingChild))
                );
                assert_eq!(
                    flat_store.get_flat_storage_status(status.right_child_shard),
                    Ok(FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CreatingChild))
                );
            }
        }
    }

    /// In this test we write some dirty state into children shards and then try to resume a shard split.
    /// Verify that the dirty writes are cleaned up correctly.
    #[test]
    fn resume_split_starts_from_clean_state() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let flat_store = resharder.runtime.store().flat_store();
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type {
            ReshardingEventType::SplitShard(params) => params,
        };

        let mut store_update = flat_store.store_update();

        // Write some random key-values in children shards.
        let dirty_key: Vec<u8> = vec![1, 2, 3, 4];
        let dirty_value = Some(FlatStateValue::Inlined(dirty_key.clone()));
        for child_shard in [left_child_shard, right_child_shard] {
            store_update.set(child_shard, dirty_key.clone(), dirty_value.clone());
        }

        // Set parent state to ShardSplitting, manually, to simulate a forcibly cancelled resharding attempt.
        let resharding_status =
            FlatStorageReshardingStatus::SplittingParent(SplittingParentStatus {
                // Values don't matter.
                left_child_shard,
                right_child_shard,
                shard_layout: new_shard_layout,
                block_hash: CryptoHash::default(),
                prev_block_hash: CryptoHash::default(),
                flat_head: BlockInfo {
                    hash: CryptoHash::default(),
                    height: 1,
                    prev_hash: CryptoHash::default(),
                },
            });
        store_update.set_flat_storage_status(
            parent_shard,
            FlatStorageStatus::Resharding(resharding_status.clone()),
        );

        store_update.commit().unwrap();

        // Resume resharding.
        resharder.resume(parent_shard, &resharding_status).unwrap();

        // Children should not contain the random keys written before.
        for child_shard in [left_child_shard, right_child_shard] {
            assert_eq!(flat_store.get(child_shard, &dirty_key), Ok(None));
        }
    }

    /// Tests a simple split shard scenario.
    ///
    /// Old layout:
    /// shard 0 -> accounts [aa]
    /// shard 1 -> accounts [mm, vv]
    ///
    /// New layout:
    /// shard 0 -> accounts [aa]
    /// shard 2 -> accounts [mm]
    /// shard 3 -> accounts [vv]
    ///
    /// Shard to split is shard 1.
    #[test]
    fn simple_split_shard() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        // Perform resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check flat storages of children contain the correct accounts and access keys.
        let left_child = ShardUId { version: 3, shard_id: 2 };
        let right_child = ShardUId { version: 3, shard_id: 3 };
        let flat_store = resharder.runtime.store().flat_store();
        let account_mm_key = TrieKey::Account { account_id: account!("mm") };
        let account_vv_key = TrieKey::Account { account_id: account!("vv") };
        assert!(flat_store
            .get(left_child, &account_mm_key.to_vec())
            .is_ok_and(|val| val.is_some()));
        assert!(flat_store
            .get(right_child, &account_vv_key.to_vec())
            .is_ok_and(|val| val.is_some()));
        let account_mm_access_key = TrieKey::AccessKey {
            account_id: account!("mm"),
            public_key: PublicKey::from_seed(KeyType::ED25519, account!("mm").as_str()),
        };
        let account_vv_access_key = TrieKey::AccessKey {
            account_id: account!("vv"),
            public_key: PublicKey::from_seed(KeyType::ED25519, account!("vv").as_str()),
        };
        assert!(flat_store
            .get(left_child, &account_mm_access_key.to_vec())
            .is_ok_and(|val| val.is_some()));
        assert!(flat_store
            .get(right_child, &account_vv_access_key.to_vec())
            .is_ok_and(|val| val.is_some()));

        // Check final status of parent flat storage.
        let parent = ShardUId { version: 3, shard_id: 1 };
        assert_eq!(flat_store.get_flat_storage_status(parent), Ok(FlatStorageStatus::Empty));
        assert_eq!(flat_store.iter(parent).count(), 0);
        assert!(resharder
            .runtime
            .get_flat_storage_manager()
            .get_flat_storage_for_shard(parent)
            .is_none());

        // Check final status of children flat storages.
        let last_hash = chain.head().unwrap().last_block_hash;
        assert_eq!(
            flat_store.get_flat_storage_status(left_child),
            Ok(FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CatchingUp(last_hash)))
        );
        assert_eq!(
            flat_store.get_flat_storage_status(left_child),
            Ok(FlatStorageStatus::Resharding(FlatStorageReshardingStatus::CatchingUp(last_hash)))
        );
    }

    /// Split shard task should run in batches.
    #[test]
    fn split_shard_batching() {
        init_test_logger();
        let scheduler = Arc::new(DelayedScheduler::default());
        let (chain, resharder) =
            create_chain_and_resharder(simple_shard_layout(), scheduler.as_multi_sender());
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        // Tweak the resharding config to make smaller batches.
        let mut config = resharder.resharding_config.get();
        config.batch_size = bytesize::ByteSize(1);
        resharder.resharding_config.update(config);

        // Perform resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check that more than one batch has been processed.
        let FlatStorageReshardingTaskStatus::Successful { num_batches_done } =
            scheduler.call_split_shard_task()
        else {
            assert!(false);
            return;
        };
        assert!(num_batches_done > 1);
    }

    #[test]
    fn cancel_split_shard() {
        init_test_logger();
        let scheduler = Arc::new(DelayedScheduler::default());
        let (chain, resharder) =
            create_chain_and_resharder(simple_shard_layout(), scheduler.as_multi_sender());
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        // Perform resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());
        let (parent_shard, status) = resharder.get_parent_shard_and_status().unwrap();
        let SplittingParentStatus { left_child_shard, right_child_shard, flat_head, .. } = status;

        // Cancel the task before it starts.
        resharder.controller.handle.stop();

        // Run the task.
        scheduler.call_split_shard_task();

        // Check that resharding was effectively cancelled.
        let flat_store = resharder.runtime.store().flat_store();
        assert_eq!(
            flat_store.get_flat_storage_status(parent_shard),
            Ok(FlatStorageStatus::Ready(FlatStorageReadyStatus { flat_head }))
        );
        for child_shard in [left_child_shard, right_child_shard] {
            assert_eq!(
                flat_store.get_flat_storage_status(status.left_child_shard),
                Ok(FlatStorageStatus::Empty)
            );
            assert_eq!(flat_store.iter(child_shard).count(), 0);
        }
    }

    /// A shard can't be split if it isn't in ready state.
    #[test]
    fn reject_split_shard_if_parent_is_not_ready() {
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);

        // Make flat storage of parent shard not ready.
        let parent_shard = ShardUId { version: 3, shard_id: 1 };
        let flat_store = resharder.runtime.store().flat_store();
        let mut store_update = flat_store.store_update();
        store_update.set_flat_storage_status(parent_shard, FlatStorageStatus::Empty);
        store_update.commit().unwrap();

        // Trigger resharding and it should fail.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_err());
    }

    /// Verify the correctness of a shard split in the presence of flat storage deltas in the parent
    /// shard.
    #[test]
    fn split_shard_parent_flat_store_with_deltas() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (mut chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();

        // In order to have flat state deltas we must bring the chain forward by adding blocks.
        let signer = Arc::new(create_test_signer("aa"));
        for height in 1..3 {
            let prev_block = chain.get_block_by_height(height - 1).unwrap();
            let block = TestBlockBuilder::new(Clock::real(), &prev_block, signer.clone())
                .height(height)
                .build();
            chain.process_block_test(&None, block).unwrap();
        }
        assert_eq!(chain.head().unwrap().height, 2);

        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type.clone() {
            ReshardingEventType::SplitShard(params) => params,
        };
        let manager = chain.runtime_adapter.get_flat_storage_manager();

        // Manually add deltas on top of parent's flat storage.
        // Pick different kind of keys and operations in order to maximize test coverage.
        // List of all keys and their values:
        let account_vv_key = TrieKey::Account { account_id: account!("vv") };
        let account_vv_value = Some("vv-update".as_bytes().to_vec());
        let account_oo_key = TrieKey::Account { account_id: account!("oo") };
        let account_oo_value = Some("oo".as_bytes().to_vec());
        let account_mm_key = TrieKey::Account { account_id: account!("mm") };
        let delayed_receipt_0_key = TrieKey::DelayedReceipt { index: 0 };
        let delayed_receipt_0_value_0 = Some("delayed0-0".as_bytes().to_vec());
        let delayed_receipt_0_value_1 = Some("delayed0-1".as_bytes().to_vec());
        let delayed_receipt_1_key = TrieKey::DelayedReceipt { index: 1 };
        let delayed_receipt_1_value = Some("delayed1".as_bytes().to_vec());
        let buffered_receipt_0_key =
            TrieKey::BufferedReceipt { receiving_shard: ShardId::new(0), index: 0 };
        let buffered_receipt_0_value_0 = Some("buffered0-0".as_bytes().to_vec());
        let buffered_receipt_0_value_1 = Some("buffered0-1".as_bytes().to_vec());
        let buffered_receipt_1_key =
            TrieKey::BufferedReceipt { receiving_shard: ShardId::new(0), index: 1 };
        let buffered_receipt_1_value = Some("buffered1".as_bytes().to_vec());

        // First set of deltas.
        let height = 1;
        let prev_hash = *chain.get_block_by_height(height).unwrap().header().prev_hash();
        let block_hash = *chain.get_block_by_height(height).unwrap().hash();
        let state_changes = vec![
            // Change: add account.
            RawStateChangesWithTrieKey {
                trie_key: account_oo_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: account_oo_value.clone(),
                }],
            },
            // Change: update account.
            RawStateChangesWithTrieKey {
                trie_key: account_vv_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: account_vv_value.clone(),
                }],
            },
            // Change: add two delayed receipts.
            RawStateChangesWithTrieKey {
                trie_key: delayed_receipt_0_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: delayed_receipt_0_value_0,
                }],
            },
            RawStateChangesWithTrieKey {
                trie_key: delayed_receipt_1_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: delayed_receipt_1_value,
                }],
            },
            // Change: update delayed receipt.
            RawStateChangesWithTrieKey {
                trie_key: delayed_receipt_0_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: delayed_receipt_0_value_1.clone(),
                }],
            },
            // Change: add two buffered receipts.
            RawStateChangesWithTrieKey {
                trie_key: buffered_receipt_0_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: buffered_receipt_0_value_0,
                }],
            },
            RawStateChangesWithTrieKey {
                trie_key: buffered_receipt_1_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: buffered_receipt_1_value,
                }],
            },
            // Change: update buffered receipt.
            RawStateChangesWithTrieKey {
                trie_key: buffered_receipt_0_key.clone(),
                changes: vec![RawStateChange {
                    cause: StateChangeCause::InitialState,
                    data: buffered_receipt_0_value_1.clone(),
                }],
            },
        ];
        manager
            .save_flat_state_changes(block_hash, prev_hash, height, parent_shard, &state_changes)
            .unwrap()
            .commit()
            .unwrap();

        // Second set of deltas.
        let height = 2;
        let prev_hash = *chain.get_block_by_height(height).unwrap().header().prev_hash();
        let block_hash = *chain.get_block_by_height(height).unwrap().hash();
        let state_changes = vec![
            // Change: remove account.
            RawStateChangesWithTrieKey {
                trie_key: account_mm_key,
                changes: vec![RawStateChange { cause: StateChangeCause::InitialState, data: None }],
            },
            // Change: remove delayed receipt.
            RawStateChangesWithTrieKey {
                trie_key: delayed_receipt_1_key.clone(),
                changes: vec![RawStateChange { cause: StateChangeCause::InitialState, data: None }],
            },
            // Change: remove buffered receipt.
            RawStateChangesWithTrieKey {
                trie_key: buffered_receipt_1_key.clone(),
                changes: vec![RawStateChange { cause: StateChangeCause::InitialState, data: None }],
            },
        ];
        manager
            .save_flat_state_changes(block_hash, prev_hash, height, parent_shard, &state_changes)
            .unwrap()
            .commit()
            .unwrap();

        // Do resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Validate integrity of children shards.
        let flat_store = resharder.runtime.store().flat_store();
        // Account 'oo' should exist only in the left child.
        assert_eq!(
            flat_store.get(left_child_shard, &account_oo_key.to_vec()),
            Ok(account_oo_value.map(|val| FlatStateValue::inlined(&val)))
        );
        assert_eq!(flat_store.get(right_child_shard, &account_oo_key.to_vec()), Ok(None));
        // Account 'vv' should exist with updated value only in the right child.
        assert_eq!(flat_store.get(left_child_shard, &account_vv_key.to_vec()), Ok(None));
        assert_eq!(
            flat_store.get(right_child_shard, &account_vv_key.to_vec()),
            Ok(account_vv_value.map(|val| FlatStateValue::inlined(&val)))
        );
        // Delayed receipt '1' shouldn't exist.
        // Delayed receipt '0' should exist with updated value in both children.
        for child in [left_child_shard, right_child_shard] {
            assert_eq!(
                flat_store.get(child, &delayed_receipt_0_key.to_vec()),
                Ok(delayed_receipt_0_value_1.clone().map(|val| FlatStateValue::inlined(&val)))
            );

            assert_eq!(flat_store.get(child, &delayed_receipt_1_key.to_vec()), Ok(None));
        }
        // Buffered receipt '0' should exist with updated value only in the left child.
        assert_eq!(
            flat_store.get(left_child_shard, &buffered_receipt_0_key.to_vec()),
            Ok(buffered_receipt_0_value_1.map(|val| FlatStateValue::inlined(&val)))
        );
        assert_eq!(flat_store.get(right_child_shard, &buffered_receipt_0_key.to_vec()), Ok(None));
        // Buffered receipt '1' shouldn't exist.
        for child in [left_child_shard, right_child_shard] {
            assert_eq!(flat_store.get(child, &buffered_receipt_1_key.to_vec()), Ok(None));
        }
    }

    /// Tests the split of "account-id based" keys that are not covered in [simple_split_shard].
    ///
    /// Old layout:
    /// shard 0 -> accounts [aa]
    /// shard 1 -> accounts [mm, vv]
    ///
    /// New layout:
    /// shard 0 -> accounts [aa]
    /// shard 2 -> accounts [mm]
    /// shard 3 -> accounts [vv]
    #[test]
    fn split_shard_handle_account_id_keys() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type.clone() {
            ReshardingEventType::SplitShard(params) => params,
        };
        let flat_store = resharder.runtime.store().flat_store();

        let mut store_update = flat_store.store_update();
        let test_value = Some(FlatStateValue::Inlined(vec![0]));

        // Helper closure to create all test keys for a given account. Returns the created keys.
        let mut inject = |account: AccountId| -> Vec<Vec<u8>> {
            let mut keys = vec![];

            // Inject contract data.
            let key = TrieKey::ContractData { account_id: account.clone(), key: vec![] }.to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);

            // Inject contract code.
            let key = TrieKey::ContractCode { account_id: account.clone() }.to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);

            // Inject received_data.
            let key = TrieKey::ReceivedData {
                receiver_id: account.clone(),
                data_id: CryptoHash::default(),
            }
            .to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);

            // Inject postponed receipt.
            let key = TrieKey::PostponedReceiptId {
                receiver_id: account.clone(),
                data_id: CryptoHash::default(),
            }
            .to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);
            let key = TrieKey::PendingDataCount {
                receiver_id: account.clone(),
                receipt_id: CryptoHash::default(),
            }
            .to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);
            let key = TrieKey::PostponedReceipt {
                receiver_id: account,
                receipt_id: CryptoHash::default(),
            }
            .to_vec();
            store_update.set(parent_shard, key.clone(), test_value.clone());
            keys.push(key);

            keys
        };

        let account_mm_keys = inject(account!("mm"));
        let account_vv_keys = inject(account!("vv"));
        store_update.commit().unwrap();

        // Do resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check each child has the correct keys assigned to itself.
        for key in &account_mm_keys {
            assert_eq!(flat_store.get(left_child_shard, key), Ok(test_value.clone()));
            assert_eq!(flat_store.get(right_child_shard, key), Ok(None));
        }
        for key in &account_vv_keys {
            assert_eq!(flat_store.get(left_child_shard, key), Ok(None));
            assert_eq!(flat_store.get(right_child_shard, key), Ok(test_value.clone()));
        }
    }

    /// Tests the split of delayed receipts.
    #[test]
    fn split_shard_handle_delayed_receipts() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type.clone() {
            ReshardingEventType::SplitShard(params) => params,
        };
        let flat_store = resharder.runtime.store().flat_store();

        // Inject a delayed receipt into the parent flat storage.
        let mut store_update = flat_store.store_update();

        let delayed_receipt_indices_key = TrieKey::DelayedReceiptIndices.to_vec();
        let delayed_receipt_indices_value = Some(FlatStateValue::Inlined(vec![0]));
        store_update.set(
            parent_shard,
            delayed_receipt_indices_key.clone(),
            delayed_receipt_indices_value.clone(),
        );

        let delayed_receipt_key = TrieKey::DelayedReceipt { index: 0 }.to_vec();
        let delayed_receipt_value = Some(FlatStateValue::Inlined(vec![1]));
        store_update.set(parent_shard, delayed_receipt_key.clone(), delayed_receipt_value.clone());

        store_update.commit().unwrap();

        // Do resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check that flat storages of both children contain the delayed receipt.
        for child_shard in [left_child_shard, right_child_shard] {
            assert_eq!(
                flat_store.get(child_shard, &delayed_receipt_indices_key),
                Ok(delayed_receipt_indices_value.clone())
            );
            assert_eq!(
                flat_store.get(child_shard, &delayed_receipt_key),
                Ok(delayed_receipt_value.clone())
            );
        }
    }

    /// Tests the split of promise yield receipts.
    #[test]
    fn split_shard_handle_promise_yield() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type.clone() {
            ReshardingEventType::SplitShard(params) => params,
        };
        let flat_store = resharder.runtime.store().flat_store();

        // Inject a promise yield receipt into the parent flat storage.
        let mut store_update = flat_store.store_update();

        let promise_yield_indices_key = TrieKey::PromiseYieldIndices.to_vec();
        let promise_yield_indices_value = Some(FlatStateValue::Inlined(vec![0]));
        store_update.set(
            parent_shard,
            promise_yield_indices_key.clone(),
            promise_yield_indices_value.clone(),
        );

        let promise_yield_timeout_key = TrieKey::PromiseYieldTimeout { index: 0 }.to_vec();
        let promise_yield_timeout_value = Some(FlatStateValue::Inlined(vec![1]));
        store_update.set(
            parent_shard,
            promise_yield_timeout_key.clone(),
            promise_yield_timeout_value.clone(),
        );

        let promise_yield_receipt_key = TrieKey::PromiseYieldReceipt {
            receiver_id: account!("ff"),
            data_id: CryptoHash::default(),
        }
        .to_vec();
        let promise_yield_receipt_value = Some(FlatStateValue::Inlined(vec![2]));
        store_update.set(
            parent_shard,
            promise_yield_receipt_key.clone(),
            promise_yield_receipt_value.clone(),
        );

        store_update.commit().unwrap();

        // Do resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check that flat storages of both children contain the promise yield.
        for child_shard in [left_child_shard, right_child_shard] {
            assert_eq!(
                flat_store.get(child_shard, &promise_yield_indices_key),
                Ok(promise_yield_indices_value.clone())
            );
            assert_eq!(
                flat_store.get(child_shard, &promise_yield_timeout_key),
                Ok(promise_yield_timeout_value.clone())
            );
            assert_eq!(
                flat_store.get(child_shard, &promise_yield_receipt_key),
                Ok(promise_yield_receipt_value.clone())
            );
        }
    }

    /// Tests the split of buffered receipts.
    #[test]
    fn split_shard_handle_buffered_receipts() {
        init_test_logger();
        let sender = TestScheduler::default().into_multi_sender();
        let (chain, resharder) = create_chain_and_resharder(simple_shard_layout(), sender);
        let new_shard_layout = shard_layout_after_split();
        let resharding_event_type = event_type_from_chain_and_layout(&chain, &new_shard_layout);
        let ReshardingSplitShardParams {
            parent_shard, left_child_shard, right_child_shard, ..
        } = match resharding_event_type.clone() {
            ReshardingEventType::SplitShard(params) => params,
        };
        let flat_store = resharder.runtime.store().flat_store();

        // Inject a buffered receipt into the parent flat storage.
        let mut store_update = flat_store.store_update();

        let buffered_receipt_indices_key = TrieKey::BufferedReceiptIndices.to_vec();
        let buffered_receipt_indices_value = Some(FlatStateValue::Inlined(vec![0]));
        store_update.set(
            parent_shard,
            buffered_receipt_indices_key.clone(),
            buffered_receipt_indices_value.clone(),
        );

        let receiving_shard = ShardId::new(0);
        let buffered_receipt_key = TrieKey::BufferedReceipt { receiving_shard, index: 0 }.to_vec();
        let buffered_receipt_value = Some(FlatStateValue::Inlined(vec![1]));
        store_update.set(
            parent_shard,
            buffered_receipt_key.clone(),
            buffered_receipt_value.clone(),
        );

        store_update.commit().unwrap();

        // Do resharding.
        assert!(resharder.start_resharding(resharding_event_type, &new_shard_layout).is_ok());

        // Check that only the first child contain the buffered receipt.
        assert_eq!(
            flat_store.get(left_child_shard, &buffered_receipt_indices_key),
            Ok(buffered_receipt_indices_value)
        );
        assert_eq!(flat_store.get(right_child_shard, &buffered_receipt_indices_key), Ok(None));
        assert_eq!(
            flat_store.get(left_child_shard, &buffered_receipt_key),
            Ok(buffered_receipt_value)
        );
        assert_eq!(flat_store.get(right_child_shard, &buffered_receipt_key), Ok(None));
    }
}
