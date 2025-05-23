use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::approval::{
    ApprovalAtHeightStatus, ApprovalContent, ApprovalHistoryEntry, ApprovalInner, ApprovalStake,
    ApprovalValidated,
};
use crate::types::{Balance, BlockHeight, BlockHeightDelta};
use primitives::{hash::CryptoHash, peer::Address};
use tracing::{debug, debug_span, field, info};

/// Have that many iterations in the timer instead of `loop` to prevent potential bugs from blocking
/// the node
const MAX_TIMER_ITERS: usize = 20;

/// How many heights ahead to track approvals. This needs to be sufficiently large so that we can
/// recover after rather long network interruption, but not too large to consume too much memory if
/// someone in the network spams with invalid approvals. Note that we will only store approvals for
/// heights that are targeting us, which is once per as many heights as there are block producers,
/// thus 10_000 heights in practice will mean on the order of one hundred entries.
const MAX_HEIGHTS_AHEAD_TO_STORE_APPROVALS: BlockHeight = 10_000;

// Number of blocks (before head) for which to keep the history of approvals (for debugging).
const MAX_HEIGHTS_BEFORE_TO_STORE_APPROVALS: u64 = 20;

// Maximum amount of historical approvals that we'd keep for debugging purposes.
const MAX_HISTORY_SIZE: usize = 1000;

/// The threshold for doomslug to create a block.
/// `TwoThirds` means the block can only be produced if at least 2/3 of the stake is approving it,
///             and is what should be used in production (and what guarantees finality)
/// `NoApprovals` means the block production is not blocked on approvals. This is used
///             in many tests (e.g. `cross_shard_tx`) to create lots of forkfulness.
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum DoomslugThresholdMode {
    NoApprovals,
    TwoThirds,
}

/// The result of processing an approval.
#[derive(PartialEq, Eq, Debug)]
pub enum DoomslugBlockProductionReadiness {
    NotReady,
    ReadySince(Instant),
}

struct DoomslugTimer {
    started: Instant,
    last_endorsement_sent: Instant,
    height: BlockHeight,
    // Config
    endorsement_delay: Duration,
    min_delay: Duration,
    delay_step: Duration,
    max_delay: Duration,
}

struct DoomslugTip {
    block_hash: CryptoHash,
    height: BlockHeight,
}

struct DoomslugApprovalsTracker {
    witness: HashMap<Address, (ApprovalValidated, chrono::DateTime<chrono::Utc>)>,
    account_id_to_stakes: HashMap<Address, (Balance, Balance)>,
    total_stake_this_epoch: Balance,
    approved_stake_this_epoch: Balance,
    total_stake_next_epoch: Balance,
    approved_stake_next_epoch: Balance,
    time_passed_threshold: Option<Instant>,
    threshold_mode: DoomslugThresholdMode,
}

/// Approvals can arrive before the corresponding blocks, and we need a meaningful way to keep as
/// many approvals as possible that can be useful in the future, while not allowing an adversary
/// to spam us with invalid approvals.
/// To that extent, for each `account_id` and each `target_height` we keep exactly one approval,
/// whichever came last. We only maintain those for
///  a) `account_id`s that match the corresponding epoch (and for which we can validate a signature)
///  b) `target_height`s for which we produce blocks
///  c) `target_height`s within a meaningful horizon from the current tip.
/// This class is responsible for maintaining witnesses for the blocks, while also ensuring that
/// only one approval per (`account_id`) is kept. We instantiate one such class per height, thus
/// ensuring that only one approval is kept per (`target_height`, `account_id`). `Doomslug` below
/// ensures that only instances within the horizon are kept, and the user of the `Doomslug` is
/// responsible for ensuring that only approvals for proper account_ids with valid signatures are
/// provided.
struct DoomslugApprovalsTrackersAtHeight {
    approval_trackers: HashMap<ApprovalInner, DoomslugApprovalsTracker>,
    last_approval_per_account: HashMap<Address, ApprovalInner>,
}

/// Contains all the logic for Doomslug, but no integration with chain or storage. The integration
/// happens via `PersistentDoomslug` struct. The split is to simplify testing of the logic separate
/// from the chain.
pub struct Doomslug {
    approval_tracking: HashMap<BlockHeight, DoomslugApprovalsTrackersAtHeight>,
    /// Largest target height for which we issued an approval
    largest_sent_target_height: BlockHeight,
    /// Largest height for which we saw a block containing 2/3 endorsements in it
    largest_final_height: BlockHeight,
    /// Largest height for which we saw threshold approvals (and thus can potentially create a block)
    largest_threshold_approvals_height: BlockHeight,
    /// Largest target height of approvals that we've received
    largest_approval_target_height: BlockHeight,
    /// Information Doomslug tracks about the chain tip
    /// TODO: what's the difference between `largest_final_height` and `tip.height`?
    tip: DoomslugTip,
    /// Whether an endorsement (or in general an approval) was sent since updating the tip
    endorsement_pending: bool,
    /// Information to track the timer (see `start_timer` routine in the paper)
    timer: DoomslugTimer,
    /// How many approvals to have before producing a block. In production should be always `HalfStake`,
    ///    but for many tests we use `NoApprovals` to invoke more forkfulness
    threshold_mode: DoomslugThresholdMode,
    /// Approvals that were created by this doomslug instance (for debugging only).
    /// Keeps up to MAX_HISTORY_SIZE entries.
    history: VecDeque<ApprovalHistoryEntry>,
}

impl DoomslugTimer {
    /// Computes the delay to sleep given the number of heights from the last final block
    /// This is what `T` represents in the paper.
    ///
    /// # Arguments
    /// * `n` - number of heights since the last block with doomslug finality
    ///
    /// # Returns
    /// Duration to sleep
    pub fn get_delay(&self, n: BlockHeightDelta) -> Duration {
        let n32 = u32::try_from(n).unwrap_or(u32::MAX);
        std::cmp::min(
            self.max_delay,
            self.min_delay + self.delay_step * n32.saturating_sub(2),
        )
    }
}

impl DoomslugApprovalsTracker {
    fn new(
        account_id_to_stakes: HashMap<Address, (Balance, Balance)>,
        threshold_mode: DoomslugThresholdMode,
    ) -> Self {
        let total_stake_this_epoch = account_id_to_stakes
            .values()
            .map(|(x, _)| x)
            .sum::<Balance>();
        let total_stake_next_epoch = account_id_to_stakes
            .values()
            .map(|(_, x)| x)
            .sum::<Balance>();

        DoomslugApprovalsTracker {
            witness: Default::default(),
            account_id_to_stakes,
            total_stake_this_epoch,
            total_stake_next_epoch,
            approved_stake_this_epoch: 0,
            approved_stake_next_epoch: 0,
            time_passed_threshold: None,
            threshold_mode,
        }
    }

    /// Given a single approval (either an endorsement or a skip-message) updates the approved
    /// stake on the block that is being approved, and returns whether the block is now ready to be
    /// produced.
    ///
    /// # Arguments
    /// * now      - the current timestamp
    /// * approval - the approval to process
    ///
    /// # Returns
    /// Whether the block is ready to be produced
    fn process_approval(
        &mut self,
        now: Instant,
        approval: &ApprovalValidated,
    ) -> DoomslugBlockProductionReadiness {
        let mut increment_approved_stake = false;
        self.witness
            .entry(approval.validator.clone())
            .or_insert_with(|| {
                increment_approved_stake = true;
                (approval.clone(), chrono::Utc::now())
            });

        if increment_approved_stake {
            let stakes = self
                .account_id_to_stakes
                .get(&approval.validator)
                .map_or((0, 0), |x| *x);
            self.approved_stake_this_epoch += stakes.0;
            self.approved_stake_next_epoch += stakes.1;
        }

        // We call to `get_block_production_readiness` here so that if the number of approvals crossed
        // the threshold, the timer for block production starts.
        self.get_block_production_readiness(now)
    }

    /// Withdraws an approval. This happens if a newer approval for the same `target_height` comes
    /// from the same account. Removes the approval from the `witness` and updates approved and
    /// endorsed stakes.
    fn withdraw_approval(&mut self, validator: &Address) {
        let approval = match self.witness.remove(validator) {
            None => return,
            Some(approval) => approval.0,
        };

        let stakes = self
            .account_id_to_stakes
            .get(&approval.validator)
            .map_or((0, 0), |x| *x);
        self.approved_stake_this_epoch -= stakes.0;
        self.approved_stake_next_epoch -= stakes.1;
    }

    /// Returns whether the block has enough approvals, and if yes, since what moment it does.
    ///
    /// # Arguments
    /// * now - the current timestamp
    ///
    /// # Returns
    /// `NotReady` if the block doesn't have enough approvals yet to cross the threshold
    /// `ReadySince` if the block has enough approvals to pass the threshold, and since when it
    ///     does
    fn get_block_production_readiness(&mut self, now: Instant) -> DoomslugBlockProductionReadiness {
        if (self.approved_stake_this_epoch > self.total_stake_this_epoch * 2 / 3
            && (self.approved_stake_next_epoch > self.total_stake_next_epoch * 2 / 3
                || self.total_stake_next_epoch == 0))
            || self.threshold_mode == DoomslugThresholdMode::NoApprovals
        {
            if self.time_passed_threshold.is_none() {
                self.time_passed_threshold = Some(now);
            }
            DoomslugBlockProductionReadiness::ReadySince(self.time_passed_threshold.unwrap())
        } else {
            DoomslugBlockProductionReadiness::NotReady
        }
    }

    // Get witnesses together with their arrival time.
    fn get_witnesses(&self) -> Vec<(Address, chrono::DateTime<chrono::Utc>)> {
        self.witness
            .iter()
            .map(|(key, (_, arrival_time))| (key.clone(), *arrival_time))
            .collect::<Vec<_>>()
    }
}

impl DoomslugApprovalsTrackersAtHeight {
    fn new() -> Self {
        Self {
            approval_trackers: HashMap::new(),
            last_approval_per_account: HashMap::new(),
        }
    }

    /// This method is a wrapper around `DoomslugApprovalsTracker::process_approval`, see comment
    /// above it for more details.
    /// This method has an extra logic that ensures that we only track one approval per `account_id`,
    /// if we already know some other approval for this account, we first withdraw it from the
    /// corresponding tracker, and associate the new approval with the account.
    ///
    /// # Arguments
    /// * `now`      - the current timestamp
    /// * `approval` - the approval to be processed
    /// * `stakes`   - all the stakes of all the block producers in the current epoch
    /// * `threshold_mode` - how many approvals are needed to produce a block. Is used to compute
    ///                the return value
    ///
    /// # Returns
    /// Same as `DoomslugApprovalsTracker::process_approval`
    fn process_approval(
        &mut self,
        now: Instant,
        approval: &ApprovalValidated,
        stakes: &[(ApprovalStake, bool)],
        threshold_mode: DoomslugThresholdMode,
    ) -> DoomslugBlockProductionReadiness {
        if let Some(last_parent) = self.last_approval_per_account.get(&approval.validator) {
            let should_remove = self
                .approval_trackers
                .get_mut(last_parent)
                .map(|x| {
                    x.withdraw_approval(&approval.validator);
                    x.witness.is_empty()
                })
                .unwrap_or(false);

            if should_remove {
                self.approval_trackers.remove(last_parent);
            }
        }

        let account_id_to_stakes = stakes
            .iter()
            .filter_map(|(x, is_slashed)| {
                if *is_slashed {
                    None
                } else {
                    Some((
                        x.validator.clone(),
                        (x.stake_this_epoch, x.stake_next_epoch),
                    ))
                }
            })
            .collect::<HashMap<_, _>>();

        assert_eq!(account_id_to_stakes.len(), stakes.len());

        if !account_id_to_stakes.contains_key(&approval.validator) {
            return DoomslugBlockProductionReadiness::NotReady;
        }

        self.last_approval_per_account
            .insert(approval.validator.clone(), approval.content.inner.clone());
        self.approval_trackers
            .entry(approval.content.inner.clone())
            .or_insert_with(|| DoomslugApprovalsTracker::new(account_id_to_stakes, threshold_mode))
            .process_approval(now, approval)
    }

    /// Returns the current approvals status for the trackers at this height.
    /// Status contains information about which account voted (and for what) and whether the doomslug voting threshold was reached.
    pub fn status(&self) -> ApprovalAtHeightStatus {
        let approvals = self
            .approval_trackers
            .iter()
            .flat_map(|(approval, tracker)| {
                let witnesses = tracker.get_witnesses();
                witnesses.into_iter().map(|(account_name, approval_time)| {
                    (account_name, (approval.clone(), approval_time))
                })
            })
            .collect::<HashMap<_, _>>();

        let threshold_approval = self
            .approval_trackers
            .iter()
            .filter_map(|(_, tracker)| tracker.time_passed_threshold)
            .min()
            .map(|ts| {
                chrono::Utc::now()
                    - chrono::Duration::from_std(ts.elapsed()).unwrap_or(chrono::Duration::days(1))
            });
        ApprovalAtHeightStatus {
            approvals,
            ready_at: threshold_approval,
        }
    }
}

impl Doomslug {
    pub fn new(
        largest_sent_target_height: BlockHeight,
        endorsement_delay: Duration,
        min_delay: Duration,
        delay_step: Duration,
        max_delay: Duration,
        threshold_mode: DoomslugThresholdMode,
    ) -> Self {
        Doomslug {
            approval_tracking: HashMap::new(),
            largest_sent_target_height,
            largest_approval_target_height: 0,
            largest_final_height: 0,
            largest_threshold_approvals_height: 0,
            tip: DoomslugTip {
                block_hash: CryptoHash::default(),
                height: 0,
            },
            endorsement_pending: false,
            timer: DoomslugTimer {
                started: Instant::now(),
                last_endorsement_sent: Instant::now(),
                height: 0,
                // Config
                endorsement_delay,
                min_delay,
                delay_step,
                max_delay,
            },
            threshold_mode,
            history: VecDeque::new(),
        }
    }

    #[cfg(feature = "test_features")]
    pub fn adv_disable(&mut self) {
        self.threshold_mode = DoomslugThresholdMode::NoApprovals
    }

    /// Returns the `(hash, height)` of the current tip. Currently is only used by tests.
    pub fn get_tip(&self) -> (CryptoHash, BlockHeight) {
        (self.tip.block_hash, self.tip.height)
    }

    /// Returns the largest height for which we have enough approvals to be theoretically able to
    ///     produce a block (in practice a blocks might not be produceable yet if not enough time
    ///     passed since it accumulated enough approvals)
    pub fn get_largest_height_crossing_threshold(&self) -> BlockHeight {
        self.largest_threshold_approvals_height
    }

    /// Returns the largest height for which we've received an approval
    pub fn get_largest_approval_target_height(&self) -> BlockHeight {
        self.largest_approval_target_height
    }

    pub fn get_largest_final_height(&self) -> BlockHeight {
        self.largest_final_height
    }

    pub fn get_largest_sent_target_height(&self) -> BlockHeight {
        self.largest_sent_target_height
    }

    pub fn get_timer_height(&self) -> BlockHeight {
        self.timer.height
    }

    pub fn get_timer_start(&self) -> Instant {
        self.timer.started
    }

    /// Returns currently available approval history.
    pub fn get_approval_history(&self) -> Vec<ApprovalHistoryEntry> {
        self.history.iter().cloned().collect::<Vec<_>>()
    }

    /// Adds new approval to the history.
    fn update_history(&mut self, entry: ApprovalHistoryEntry) {
        while self.history.len() >= MAX_HISTORY_SIZE {
            self.history.pop_front();
        }
        self.history.push_back(entry);
    }

    /// Is expected to be called periodically and processed the timer (`start_timer` in the paper)
    /// If the `cur_time` way ahead of last time the `process_timer` was called, will only process
    /// a bounded number of steps, to avoid an infinite loop in case of some bugs.
    /// Processes sending delayed approvals or skip messages
    /// A major difference with the paper is that we process endorsement from the `process_timer`,
    /// not at the time of receiving a block. It is done to stagger blocks if the network is way
    /// too fast (e.g. during tests, or if a large set of validators have connection significantly
    /// better between themselves than with the rest of the validators)
    ///
    /// # Arguments
    /// * `cur_time` - is expected to receive `now`. Doesn't directly use `now` to simplify testing
    ///
    /// # Returns
    /// A vector of approvals that need to be sent to other block producers as a result of processing
    /// the timers
    #[must_use]
    pub fn process_timer(&mut self, cur_time: Instant) -> Vec<ApprovalContent> {
        let mut ret = vec![];

        for _ in 0..MAX_TIMER_ITERS {
            // The `skip_delay` is the time before sending the approval to BP of `timer_height + 1`,
            let skip_delay = self
                .timer
                .get_delay(self.timer.height.saturating_sub(self.largest_final_height));

            // The `endorsement_delay` is time to send approval to the block producer at `timer.height`,
            // while the `skip_delay` is the time before sending the approval to BP of `timer_height + 1`,
            // so it makes sense for them to be at least 2x apart
            debug_assert!(skip_delay >= 2 * self.timer.endorsement_delay);

            let tip_height = self.tip.height;

            // We've received a block recently that we need to endorse, we need to wait a minumum
            // of the last endorsement delay before sending
            if self.endorsement_pending
                && cur_time >= self.timer.last_endorsement_sent + self.timer.endorsement_delay
            {
                // We have the block for an endorsement we sent
                if tip_height >= self.largest_sent_target_height {
                    self.largest_sent_target_height = tip_height + 1;

                    if let Some(approval) = self.create_approval(tip_height + 1) {
                        ret.push(approval);
                    }

                    // Add to history (debug only)
                    self.update_history(ApprovalHistoryEntry {
                        parent_height: tip_height,
                        target_height: tip_height + 1,
                        timer_started_ago_millis: self
                            .timer
                            .last_endorsement_sent
                            .elapsed()
                            .as_millis() as u64,
                        expected_delay_millis: self.timer.endorsement_delay.as_millis() as u64,
                        approval_creation_time: chrono::Utc::now(),
                    });
                }

                self.timer.last_endorsement_sent = cur_time;
                self.endorsement_pending = false;
            }

            // Timeout waiting for the next block
            if cur_time >= self.timer.started + skip_delay {
                debug_assert!(!self.endorsement_pending);

                self.largest_sent_target_height =
                    std::cmp::max(self.timer.height + 1, self.largest_sent_target_height);

                if let Some(approval) = self.create_approval(self.timer.height + 1) {
                    ret.push(approval);
                }
                self.update_history(ApprovalHistoryEntry {
                    parent_height: tip_height,
                    target_height: self.timer.height + 1,
                    timer_started_ago_millis: self.timer.started.elapsed().as_millis() as u64,
                    expected_delay_millis: skip_delay.as_millis() as u64,
                    approval_creation_time: chrono::Utc::now(),
                });

                // Restart the timer
                self.timer.started += skip_delay;
                self.timer.height += 1;
            } else {
                break;
            }
        }

        ret
    }

    fn create_approval(&self, target_height: BlockHeight) -> Option<ApprovalContent> {
        Some(ApprovalContent::new(
            self.tip.block_hash,
            self.tip.height,
            target_height,
        ))
    }

    /// Determines whether a block has enough approvals to be produced.
    /// In production (with `mode == TwoThirds`) we require the total stake of all the approvals to
    /// be strictly more than half of the total stake. For many non-doomslug specific tests
    /// (with `mode == NoApprovals`) no approvals are needed.
    ///
    /// # Arguments
    /// * `mode`      - whether we want half of the total stake or just a single approval
    /// * `approvals` - the set of approvals in the current block
    /// * `stakes`    - the vector of validator stakes in the current epoch
    pub fn can_approved_block_be_produced(
        mode: DoomslugThresholdMode,
        approvals: &[Option<Box<bool>>],
        stakes: &[(Balance, Balance, bool)],
    ) -> bool {
        if mode == DoomslugThresholdMode::NoApprovals {
            return true;
        }

        let threshold1 = stakes.iter().map(|(x, _, _)| x).sum::<Balance>() * 2 / 3;
        let threshold2 = stakes.iter().map(|(_, x, _)| x).sum::<Balance>() * 2 / 3;

        let approved_stake1 = approvals
            .iter()
            .zip(stakes.iter())
            .filter(|(_, (_, _, is_slashed))| !*is_slashed)
            .map(|(approval, (stake, _, _))| if approval.is_some() { *stake } else { 0 })
            .sum::<Balance>();

        let approved_stake2 = approvals
            .iter()
            .zip(stakes.iter())
            .filter(|(_, (_, _, is_slashed))| !*is_slashed)
            .map(|(approval, (_, stake, _))| if approval.is_some() { *stake } else { 0 })
            .sum::<Balance>();

        (approved_stake1 > threshold1 || threshold1 == 0)
            && (approved_stake2 > threshold2 || threshold2 == 0)
    }

    pub fn get_witness(
        &self,
        prev_hash: &CryptoHash,
        parent_height: BlockHeight,
        target_height: BlockHeight,
    ) -> HashMap<Address, (ApprovalValidated, chrono::DateTime<chrono::Utc>)> {
        let hash_or_height = ApprovalInner::new(prev_hash, parent_height, target_height);
        if let Some(approval_trackers_at_height) = self.approval_tracking.get(&target_height) {
            let approvals_tracker = approval_trackers_at_height
                .approval_trackers
                .get(&hash_or_height);
            match approvals_tracker {
                None => HashMap::new(),
                Some(approvals_tracker) => approvals_tracker.witness.clone(),
            }
        } else {
            HashMap::new()
        }
    }

    /// Updates the current tip of the chain with a new finality height. Restarts the timer accordingly.
    /// Called when we receive a new block that would extend the chain. Block should be checked for validity
    /// before calling this method.
    ///
    /// # Arguments
    /// * `now`            - current time. Doesn't call to `Utc::now()` directly to simplify testing
    /// * `block_hash`     - the hash of the new tip
    /// * `height`         - the height of the tip
    /// * `last_ds_final_height` - last height at which a block in this chain has doomslug finality
    pub fn on_block(
        &mut self,
        now: Instant,
        block_hash: CryptoHash,
        height: BlockHeight,
        last_final_height: BlockHeight,
    ) {
        debug_assert!(height > self.tip.height || self.tip.height == 0);
        self.tip = DoomslugTip { block_hash, height };

        self.largest_final_height = last_final_height;
        self.timer.height = height + 1;
        self.timer.started = now;

        self.approval_tracking.retain(|h, _| {
            *h > height.saturating_sub(MAX_HEIGHTS_BEFORE_TO_STORE_APPROVALS)
                && *h <= height + MAX_HEIGHTS_AHEAD_TO_STORE_APPROVALS
        });

        self.endorsement_pending = true;
    }

    /// Processes single approval
    pub fn on_approval(
        &mut self,
        now: Instant,
        approval: &ApprovalValidated,
        stakes: &[(ApprovalStake, bool)],
    ) {
        if approval.content.target_height < self.tip.height
            || approval.content.target_height
                > self.tip.height + MAX_HEIGHTS_AHEAD_TO_STORE_APPROVALS
        {
            return;
        }

        let _ = self.on_approval_internal(now, approval, stakes);
    }

    /// Records an approval message, and return whether the block has passed the threshold / ready
    /// to be produced without waiting any further. See the comment for `DoomslugApprovalTracker::process_approval`
    /// for details
    #[must_use]
    fn on_approval_internal(
        &mut self,
        now: Instant,
        approval: &ApprovalValidated,
        stakes: &[(ApprovalStake, bool)],
    ) -> DoomslugBlockProductionReadiness {
        let threshold_mode = self.threshold_mode;
        let ret = self
            .approval_tracking
            .entry(approval.content.target_height)
            .or_insert_with(DoomslugApprovalsTrackersAtHeight::new)
            .process_approval(now, approval, stakes, threshold_mode);

        if approval.content.target_height > self.largest_approval_target_height {
            self.largest_approval_target_height = approval.content.target_height;
        }

        if ret != DoomslugBlockProductionReadiness::NotReady
            && approval.content.target_height > self.largest_threshold_approvals_height
        {
            self.largest_threshold_approvals_height = approval.content.target_height;
        }

        ret
    }

    /// Gets the current status of approvals for a given height.
    /// It will only work for heights that we have in memory, that is that are not older than MAX_HEIGHTS_BEFORE_TO_STORE_APPROVALS
    /// blocks from the head.
    pub fn approval_status_at_height(&self, height: &BlockHeight) -> ApprovalAtHeightStatus {
        self.approval_tracking
            .get(height)
            .map(|it| it.status())
            .unwrap_or_default()
    }

    /// Returns whether we can produce a block for this height. The check for whether `me` is the
    /// block producer for the height needs to be done by the caller.
    /// We can produce a block if:
    ///  - The block has 2/3 of approvals, doomslug-finalizing the previous block, and we have
    ///    enough chunks, or
    ///  - The block has 1/2 of approvals, and T(h' / 6) has passed since the block has had 1/2 of
    ///    approvals for the first time, where h' is time since the last ds-final block.
    /// Only the height is passed into the function, we use the tip known to `Doomslug` as the
    /// parent hash.
    ///
    /// # Arguments:
    /// * `now`               - current timestamp
    /// * `target_height`     - the height for which the readiness is checked
    /// * `has_enough_chunks` - if not, we will wait for T(h' / 6) even if we have 2/3 approvals &
    ///                         have the previous block ds-final.
    #[must_use]
    pub fn ready_to_produce_block(
        &mut self,
        now: Instant,
        target_height: BlockHeight,
        has_enough_chunks: bool,
        log_block_production_info: bool,
    ) -> bool {
        let span = debug_span!(
            target: "doomslug",
            "ready_to_produce_block",
            has_enough_chunks,
            target_height,
            enough_approvals_for = field::Empty,
            ready_to_produce_block = field::Empty,
            need_to_wait = field::Empty)
        .entered();
        let hash_or_height =
            ApprovalInner::new(&self.tip.block_hash, self.tip.height, target_height);
        if let Some(approval_trackers_at_height) = self.approval_tracking.get_mut(&target_height) {
            if let Some(approval_tracker) = approval_trackers_at_height
                .approval_trackers
                .get_mut(&hash_or_height)
            {
                let block_production_readiness =
                    approval_tracker.get_block_production_readiness(now);
                match block_production_readiness {
                    DoomslugBlockProductionReadiness::NotReady => false,
                    DoomslugBlockProductionReadiness::ReadySince(when) => {
                        let enough_approvals_for = now.saturating_duration_since(when);
                        span.record("enough_approvals_for", enough_approvals_for.as_secs_f64());
                        span.record("ready_to_produce_block", true);
                        if has_enough_chunks {
                            if log_block_production_info {
                                info!(
                                    target: "doomslug",
                                    target_height,
                                    ?enough_approvals_for,
                                    "ready to produce block, has enough approvals, has enough chunks");
                            }
                            true
                        } else {
                            let delay = self.timer.get_delay(
                                self.timer.height.saturating_sub(self.largest_final_height),
                            ) / 6;

                            let ready = now > when + delay;
                            span.record("need_to_wait", !ready);
                            if log_block_production_info {
                                if ready {
                                    info!(
                                        target: "doomslug",
                                        target_height,
                                        ?enough_approvals_for,
                                        "ready to produce block, but does not have enough chunks");
                                } else {
                                    info!(
                                        target: "doomslug",
                                        target_height,
                                        need_to_wait_for = ?(when + delay).saturating_duration_since(now),
                                        ?enough_approvals_for,
                                        "not ready to produce block, need to wait");
                                }
                            }
                            ready
                        }
                    }
                }
            } else {
                debug!(target: "doomslug", target_height, ?hash_or_height, "No approval tracker");
                false
            }
        } else {
            debug!(target: "doomslug", target_height, "No approval trackers at height");
            false
        }
    }
}
