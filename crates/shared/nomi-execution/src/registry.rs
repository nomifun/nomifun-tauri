use std::{
    collections::HashMap,
    ops::Deref,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::watch;
use uuid::Uuid;

use crate::{ExecutionOutcome, ExecutionOwner, SessionId, supervisor::Session};

pub(crate) struct Registry {
    state: Mutex<RegistryState>,
    changes: watch::Sender<u64>,
    max_sessions: usize,
}

struct RegistryState {
    phase: RegistryPhase,
    active: HashMap<SessionId, SessionEntry>,
    retiring: HashMap<SessionId, Arc<Retirement>>,
    reservations: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RegistryPhase {
    Open,
    ShuttingDown,
    Closed,
}

struct SessionEntry {
    owner: ExecutionOwner,
    session: Arc<Session>,
    lease: Duration,
    lease_expires_at: Instant,
    last_used: Instant,
    in_flight_actions: usize,
}

pub(crate) struct StartReservation {
    registry: Weak<Registry>,
    completed: bool,
}

pub(crate) struct SessionAction {
    registry: Weak<Registry>,
    session_id: SessionId,
    session: Arc<Session>,
}

pub(crate) struct Retirement {
    id: SessionId,
    owner: ExecutionOwner,
    session: Arc<Session>,
    driver_started: AtomicBool,
    outcome: watch::Sender<Option<ExecutionOutcome>>,
}

pub(crate) enum CommitResult {
    Active,
    Retiring(Arc<Retirement>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReserveError {
    Capacity,
    ShuttingDown,
}

pub(crate) enum LookupError {
    NotFound,
    OwnerMismatch,
}

pub(crate) struct ShutdownSnapshot {
    pub(crate) reservations: usize,
    pub(crate) retirements: Vec<Arc<Retirement>>,
}

impl Registry {
    pub(crate) fn new(max_sessions: usize) -> Self {
        let (changes, _receiver) = watch::channel(0);
        Self {
            state: Mutex::new(RegistryState {
                phase: RegistryPhase::Open,
                active: HashMap::new(),
                retiring: HashMap::new(),
                reservations: 0,
            }),
            changes,
            max_sessions,
        }
    }

    pub(crate) fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    pub(crate) fn reserve_start(self: &Arc<Self>) -> Result<StartReservation, ReserveError> {
        {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            if state.phase != RegistryPhase::Open {
                return Err(ReserveError::ShuttingDown);
            }
            if occupancy(&state) >= self.max_sessions {
                return Err(ReserveError::Capacity);
            }
            state.reservations = state
                .reservations
                .checked_add(1)
                .expect("execution reservation count overflowed");
        }
        self.bump_changes();
        Ok(StartReservation {
            registry: Arc::downgrade(self),
            completed: false,
        })
    }

    pub(crate) fn commit(
        &self,
        reservation: &mut StartReservation,
        session_id: SessionId,
        owner: ExecutionOwner,
        session: Arc<Session>,
        lease: Duration,
        now: Instant,
    ) -> CommitResult {
        let result = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            state.reservations = state
                .reservations
                .checked_sub(1)
                .expect("execution reservation committed more than once");
            reservation.completed = true;

            if state.phase == RegistryPhase::Open {
                let replaced = state.active.insert(
                    session_id,
                    SessionEntry {
                        owner,
                        session,
                        lease,
                        lease_expires_at: lease_deadline(now, lease),
                        last_used: now,
                        in_flight_actions: 0,
                    },
                );
                assert!(replaced.is_none(), "UUIDv7 execution session id collision");
                CommitResult::Active
            } else {
                let retirement = Arc::new(Retirement::new(session_id, owner, session));
                let replaced = state.retiring.insert(session_id, retirement.clone());
                assert!(replaced.is_none(), "UUIDv7 execution session id collision");
                CommitResult::Retiring(retirement)
            }
        };
        self.bump_changes();
        result
    }

    pub(crate) fn begin_action(
        self: &Arc<Self>,
        session_id: &SessionId,
        owner: &ExecutionOwner,
        now: Instant,
    ) -> Result<SessionAction, LookupError> {
        let session = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            let entry = state
                .active
                .get_mut(session_id)
                .ok_or(LookupError::NotFound)?;
            if &entry.owner != owner {
                return Err(LookupError::OwnerMismatch);
            }
            entry.in_flight_actions = entry
                .in_flight_actions
                .checked_add(1)
                .expect("execution action count overflowed");
            renew_entry(entry, now);
            Arc::clone(&entry.session)
        };
        session.touch_at(now);
        Ok(SessionAction {
            registry: Arc::downgrade(self),
            session_id: *session_id,
            session,
        })
    }

    pub(crate) fn heartbeat_run(&self, run_id: Uuid, now: Instant) -> usize {
        let sessions = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            state
                .active
                .values_mut()
                .filter_map(|entry| {
                    if entry.owner.run_id != run_id {
                        return None;
                    }
                    renew_entry(entry, now);
                    Some(Arc::clone(&entry.session))
                })
                .collect::<Vec<_>>()
        };
        for session in &sessions {
            session.touch_at(now);
        }
        sessions.len()
    }

    pub(crate) fn touch_session(&self, session_id: SessionId, now: Instant) -> bool {
        let session = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            let Some(entry) = state.active.get_mut(&session_id) else {
                return false;
            };
            renew_entry(entry, now);
            Arc::clone(&entry.session)
        };
        session.touch_at(now);
        true
    }

    pub(crate) fn claim_expired(&self, now: Instant) -> Vec<Arc<Retirement>> {
        let retirements = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            if state.phase != RegistryPhase::Open {
                return Vec::new();
            }
            let ids = state
                .active
                .iter()
                .filter_map(|(id, entry)| {
                    (entry.in_flight_actions == 0 && entry.lease_expires_at <= now)
                        .then_some(*id)
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| move_to_retiring(&mut state, id))
                .collect::<Vec<_>>()
        };
        if !retirements.is_empty() {
            self.bump_changes();
        }
        retirements
    }

    pub(crate) fn pending_retirements(&self) -> Vec<Arc<Retirement>> {
        self.state
            .lock()
            .expect("execution registry lock is poisoned")
            .retiring
            .values()
            .filter(|retirement| retirement.outcome().is_none())
            .cloned()
            .collect()
    }

    pub(crate) fn finish_retirement(
        &self,
        retirement: &Retirement,
        proven_reaped: bool,
    ) {
        let (removed, shutting_down) = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            if state.phase == RegistryPhase::Open && proven_reaped {
                (state.retiring.remove(&retirement.id).is_some(), false)
            } else {
                (false, state.phase != RegistryPhase::Open)
            }
        };
        if removed {
            self.bump_changes();
        }
        debug_assert!(
            removed || shutting_down || !proven_reaped,
            "completed reaped retirement was missing from the open registry"
        );
    }

    pub(crate) fn evict_oldest_finished(&self) -> bool {
        let removed = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            if state.phase != RegistryPhase::Open {
                return false;
            }
            let victim = state
                .active
                .iter()
                .filter(|(_, entry)| {
                    entry.in_flight_actions == 0 && entry.session.is_reclaimable_terminal()
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| *id);
            victim.and_then(|id| state.active.remove(&id)).is_some()
        };
        if removed {
            self.bump_changes();
        }
        removed
    }

    pub(crate) fn begin_shutdown(&self) {
        let terminals = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            if state.phase != RegistryPhase::Open {
                return;
            }
            state.phase = RegistryPhase::ShuttingDown;

            let removable = state
                .active
                .iter()
                .filter_map(|(id, entry)| {
                    (entry.in_flight_actions == 0
                        && entry.session.is_reclaimable_terminal())
                    .then_some(*id)
                })
                .collect::<Vec<_>>();
            let terminals = removable
                .into_iter()
                .filter_map(|id| state.active.remove(&id).map(|entry| entry.session))
                .collect::<Vec<_>>();

            let ids = state
                .active
                .iter()
                .filter_map(|(id, entry)| (entry.in_flight_actions == 0).then_some(*id))
                .collect::<Vec<_>>();
            for id in ids {
                let _ = move_to_retiring(&mut state, id);
            }
            terminals
        };
        drop(terminals);
        self.bump_changes();
    }

    pub(crate) fn shutdown_snapshot(&self) -> ShutdownSnapshot {
        let (snapshot, terminals) = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            let mut terminals = Vec::new();
            if state.phase != RegistryPhase::Open {
                let removable = state
                    .active
                    .iter()
                    .filter_map(|(id, entry)| {
                        (entry.in_flight_actions == 0
                            && entry.session.is_reclaimable_terminal())
                            .then_some(*id)
                    })
                    .collect::<Vec<_>>();
                for id in removable {
                    if let Some(entry) = state.active.remove(&id) {
                        terminals.push(entry.session);
                    }
                }
                let claimable = state
                    .active
                    .iter()
                    .filter_map(|(id, entry)| (entry.in_flight_actions == 0).then_some(*id))
                    .collect::<Vec<_>>();
                for id in claimable {
                    let _ = move_to_retiring(&mut state, id);
                }
            }
            (
                ShutdownSnapshot {
                    reservations: state.reservations.saturating_add(state.active.len()),
                    retirements: state.retiring.values().cloned().collect(),
                },
                terminals,
            )
        };
        drop(terminals);
        snapshot
    }

    pub(crate) fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn counts(&self) -> (usize, usize, usize) {
        let state = self
            .state
            .lock()
            .expect("execution registry lock is poisoned");
        (
            state.active.len(),
            state.retiring.len(),
            state.reservations,
        )
    }

    #[cfg(test)]
    pub(crate) fn begin_test_reservation(
        self: &Arc<Self>,
    ) -> Result<StartReservation, ReserveError> {
        self.reserve_start()
    }

    #[cfg(test)]
    pub(crate) fn test_commit(
        &self,
        reservation: &mut StartReservation,
        session_id: SessionId,
        owner: ExecutionOwner,
        session: Arc<Session>,
        lease: Duration,
        now: Instant,
    ) -> CommitResult {
        self.commit(
            reservation,
            session_id,
            owner,
            session,
            lease,
            now,
        )
    }

    #[cfg(test)]
    pub(crate) fn test_reserve_once(
        self: &Arc<Self>,
    ) -> Result<StartReservation, ReserveError> {
        self.reserve_start()
    }

    pub(crate) fn complete_shutdown(&self) {
        {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            assert_eq!(
                state.reservations, 0,
                "shutdown closed with in-flight start reservations"
            );
            assert!(
                state.active.is_empty(),
                "shutdown closed with active execution sessions"
            );
            state.phase = RegistryPhase::Closed;
        }
        self.bump_changes();
    }

    #[cfg(test)]
    pub(crate) fn get(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        self.state
            .lock()
            .expect("execution registry lock is poisoned")
            .active
            .get(session_id)
            .map(|entry| Arc::clone(&entry.session))
    }

    fn finish_action(&self, session_id: SessionId, now: Instant) -> Arc<Session> {
        let (session, changed) = {
            let mut state = self
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            let shutting_down = state.phase != RegistryPhase::Open;
            let (session, should_retire, should_remove) = {
                let entry = state
                    .active
                    .get_mut(&session_id)
                    .expect("execution action outlived its active session");
                entry.in_flight_actions = entry
                    .in_flight_actions
                    .checked_sub(1)
                    .expect("execution action guard dropped more than once");
                renew_entry(entry, now);
                (
                    Arc::clone(&entry.session),
                    shutting_down && entry.in_flight_actions == 0,
                    entry.session.is_reclaimable_terminal(),
                )
            };
            if should_retire {
                if should_remove {
                    state.active.remove(&session_id);
                } else {
                    let _ = move_to_retiring(&mut state, session_id);
                }
                (session, true)
            } else {
                (session, false)
            }
        };
        if changed {
            self.bump_changes();
        }
        session
    }

    fn bump_changes(&self) {
        self.changes
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.phase = RegistryPhase::Closed;
        state.active.clear();
        state.retiring.clear();
    }
}

impl SessionAction {
    pub(crate) fn session_arc(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }
}

impl Deref for SessionAction {
    type Target = Session;

    fn deref(&self) -> &Self::Target {
        &self.session
    }
}

impl Drop for SessionAction {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let now = Instant::now();
        registry.finish_action(self.session_id, now).touch_at(now);
    }
}

impl Retirement {
    fn new(id: SessionId, owner: ExecutionOwner, session: Arc<Session>) -> Self {
        let (outcome, _receiver) = watch::channel(None);
        Self {
            id,
            owner,
            session,
            driver_started: AtomicBool::new(false),
            outcome,
        }
    }

    pub(crate) fn id(&self) -> SessionId {
        self.id
    }

    pub(crate) fn owner(&self) -> &ExecutionOwner {
        &self.owner
    }

    pub(crate) fn session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    pub(crate) fn try_start_driver(&self) -> bool {
        self.driver_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn complete(&self, outcome: ExecutionOutcome) {
        self.outcome.send_replace(Some(outcome));
    }

    pub(crate) fn outcome(&self) -> Option<ExecutionOutcome> {
        self.outcome.borrow().clone()
    }

    pub(crate) async fn wait_outcome(&self) -> ExecutionOutcome {
        let mut outcome = self.outcome.subscribe();
        loop {
            if let Some(outcome) = outcome.borrow_and_update().clone() {
                return outcome;
            }
            outcome
                .changed()
                .await
                .expect("execution retirement outcome sender unexpectedly closed");
        }
    }
}

impl Drop for StartReservation {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        {
            let mut state = registry
                .state
                .lock()
                .expect("execution registry lock is poisoned");
            state.reservations = state
                .reservations
                .checked_sub(1)
                .expect("execution reservation dropped more than once");
        }
        registry.bump_changes();
    }
}

fn occupancy(state: &RegistryState) -> usize {
    state
        .active
        .len()
        .saturating_add(state.retiring.len())
        .saturating_add(state.reservations)
}

fn renew_entry(entry: &mut SessionEntry, now: Instant) {
    entry.last_used = now;
    entry.lease_expires_at = lease_deadline(now, entry.lease);
}

fn lease_deadline(now: Instant, lease: Duration) -> Instant {
    now.checked_add(lease).unwrap_or(now)
}

fn move_to_retiring(
    state: &mut RegistryState,
    session_id: SessionId,
) -> Option<Arc<Retirement>> {
    let entry = state.active.remove(&session_id)?;
    let retirement = Arc::new(Retirement::new(session_id, entry.owner, entry.session));
    let replaced = state.retiring.insert(session_id, retirement.clone());
    assert!(
        replaced.is_none(),
        "execution session was already retiring"
    );
    Some(retirement)
}
