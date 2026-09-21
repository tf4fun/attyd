use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde_json::Value;
use uuid::Uuid;

use crate::history_cache::{HistoryCache, SessionKey};
use crate::runtime_state::{
    PendingInteraction, RuntimeEffect, RuntimeJournal, SessionLifecycle, SessionLiveState, UrlFlow,
};
use crate::session_resources::{
    ElicitationResponder, PermissionResponder, SessionResourceOwner, SessionResources,
    UrlRegistration,
};
use crate::session_state::{MirrorError, SessionState};

/// The only map entry for a session: cloneable business data and non-cloneable
/// external handles have the same incarnation and lifetime, without another map.
pub(crate) struct SessionEntry {
    pub(crate) state: SessionState,
    pub(crate) resources: SessionResources,
}

pub(crate) struct SessionRef<'a> {
    pub(crate) state: &'a SessionState,
    pub(crate) live: &'a SessionLiveState,
}

impl SessionEntry {
    pub(crate) fn new(state: SessionState) -> Self {
        Self {
            state,
            resources: SessionResources::default(),
        }
    }
}

/// Owns each session once. History and live transitions operate on the same entry;
/// the journal retains committed delivery records without a second session map.
pub(crate) struct SessionRegistry {
    pub(crate) epoch: String,
    pub(crate) next_incarnation: u64,
    pub(crate) next_operation: u64,
    pub(crate) sessions: BTreeMap<String, SessionEntry>,
    /// Fully removed session IDs. The Agent may still own them (local
    /// retirement) or may report them again later, so late updates must not be
    /// mistaken for a pending creation's early replay.
    retired_ids: HashSet<String>,
    pub(crate) permission_owners: HashMap<String, SessionResourceOwner>,
    pub(crate) elicitation_owners: HashMap<String, SessionResourceOwner>,
    pub(crate) url_owners: HashMap<String, UrlRegistration>,
    pub(crate) history: HistoryCache,
    pub(crate) overlay_bytes: usize,
    pub(crate) retained_terminals: HashMap<SessionKey, BTreeMap<String, Value>>,
    pub(crate) retained_terminal_bytes: usize,
    pub(crate) connection_revision: u64,
    pub(crate) request_elicitations: BTreeMap<String, PendingInteraction>,
    pub(crate) request_url_flows: BTreeMap<String, UrlFlow>,
    pub(crate) resolved_request_elicitations: VecDeque<String>,
    pub(crate) resolved_request_url_flows: VecDeque<String>,
    pub(crate) effects: VecDeque<RuntimeEffect>,
    pub(crate) journal: RuntimeJournal,
}

impl SessionRegistry {
    pub(crate) fn session_ref(&self, session_id: &str) -> Option<SessionRef<'_>> {
        let state = &self.sessions.get(session_id)?.state;
        Some(SessionRef {
            state,
            live: state.live.as_ref()?,
        })
    }

    pub(crate) fn active_session(&self, session_id: &str) -> Option<SessionRef<'_>> {
        self.session_ref(session_id)
            .filter(|session| session.live.lifecycle == SessionLifecycle::Active)
    }

    pub(crate) fn resource_session(&self, session_id: &str) -> Option<SessionRef<'_>> {
        self.session_ref(session_id).filter(|session| {
            matches!(
                session.live.lifecycle,
                SessionLifecycle::Attaching
                    | SessionLifecycle::Active
                    | SessionLifecycle::Closing
                    | SessionLifecycle::ClosingForDelete
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn allocated_session_count(&self) -> usize {
        self.sessions
            .values()
            .filter(|entry| entry.state.live.is_some())
            .count()
    }

    pub(crate) fn resource_owner(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<SessionResourceOwner, MirrorError> {
        self.resources(session_id, incarnation)?;
        Ok(SessionResourceOwner::new(
            self.epoch.clone(),
            session_id,
            incarnation,
        ))
    }

    pub(crate) fn owns_resources(&self, owner: &SessionResourceOwner) -> bool {
        owner.epoch == self.epoch
            && self
                .sessions
                .get(&owner.session_id)
                .is_some_and(|entry| entry.state.incarnation == owner.incarnation)
    }

    /// Build the lightweight route from the canonical accepted flow's identity.
    pub(crate) fn register_url_route(
        &mut self,
        elicitation_id: &str,
        owner: Option<SessionResourceOwner>,
    ) -> Result<(), MirrorError> {
        if self.url_owners.contains_key(elicitation_id) {
            return Err(MirrorError::OperationMismatch);
        }
        let flow = match &owner {
            Some(owner) => {
                if !self.owns_resources(owner) {
                    return Err(MirrorError::StaleIncarnation);
                }
                self.sessions
                    .get(&owner.session_id)
                    .and_then(|entry| entry.state.live.as_ref())
                    .and_then(|live| live.url_flows.get(elicitation_id))
            }
            None => self.request_url_flows.get(elicitation_id),
        }
        .ok_or(MirrorError::OperationMismatch)?;
        let registration_id = flow.registration_id.clone();
        self.url_owners.insert(
            elicitation_id.to_string(),
            UrlRegistration {
                owner,
                registration_id,
            },
        );
        Ok(())
    }

    pub(crate) fn take_url_route(
        &mut self,
        elicitation_id: &str,
        expected: &UrlRegistration,
    ) -> Option<UrlRegistration> {
        if self.url_owners.get(elicitation_id) != Some(expected) {
            return None;
        }
        self.url_owners.remove(elicitation_id)
    }

    pub(crate) fn permission(
        &self,
        interaction_id: &str,
    ) -> Option<(&SessionResourceOwner, &PermissionResponder)> {
        let owner = self.permission_owners.get(interaction_id)?;
        if !self.owns_resources(owner) {
            return None;
        }
        let pending = self
            .resources(&owner.session_id, owner.incarnation)
            .ok()?
            .permissions
            .get(interaction_id)?;
        Some((owner, pending))
    }

    pub(crate) fn insert_permission(
        &mut self,
        owner: SessionResourceOwner,
        interaction_id: String,
        pending: PermissionResponder,
    ) -> Result<(), MirrorError> {
        if !self.owns_resources(&owner) {
            return Err(MirrorError::StaleIncarnation);
        }
        if self.permission_owners.contains_key(&interaction_id) {
            return Err(MirrorError::OperationMismatch);
        }
        self.resources_mut(&owner.session_id, owner.incarnation)?
            .permissions
            .insert(interaction_id.clone(), pending);
        self.permission_owners.insert(interaction_id, owner);
        Ok(())
    }

    pub(crate) fn take_permission(
        &mut self,
        interaction_id: &str,
        expected: &SessionResourceOwner,
    ) -> Option<PermissionResponder> {
        if self.permission_owners.get(interaction_id) != Some(expected) {
            return None;
        }
        self.permission_owners.remove(interaction_id);
        if !self.owns_resources(expected) {
            return None;
        }
        self.resources_mut(&expected.session_id, expected.incarnation)
            .ok()?
            .permissions
            .remove(interaction_id)
    }

    pub(crate) fn elicitation(
        &self,
        interaction_id: &str,
    ) -> Option<(&SessionResourceOwner, &ElicitationResponder)> {
        let owner = self.elicitation_owners.get(interaction_id)?;
        if !self.owns_resources(owner) {
            return None;
        }
        let pending = self
            .resources(&owner.session_id, owner.incarnation)
            .ok()?
            .elicitations
            .get(interaction_id)?;
        Some((owner, pending))
    }

    pub(crate) fn insert_elicitation(
        &mut self,
        owner: SessionResourceOwner,
        interaction_id: String,
        pending: ElicitationResponder,
    ) -> Result<(), MirrorError> {
        if !self.owns_resources(&owner) {
            return Err(MirrorError::StaleIncarnation);
        }
        if self.elicitation_owners.contains_key(&interaction_id) {
            return Err(MirrorError::OperationMismatch);
        }
        self.resources_mut(&owner.session_id, owner.incarnation)?
            .elicitations
            .insert(interaction_id.clone(), pending);
        self.elicitation_owners.insert(interaction_id, owner);
        Ok(())
    }

    pub(crate) fn take_elicitation(
        &mut self,
        interaction_id: &str,
        expected: &SessionResourceOwner,
    ) -> Option<ElicitationResponder> {
        if self.elicitation_owners.get(interaction_id) != Some(expected) {
            return None;
        }
        self.elicitation_owners.remove(interaction_id);
        if !self.owns_resources(expected) {
            return None;
        }
        self.resources_mut(&expected.session_id, expected.incarnation)
            .ok()?
            .elicitations
            .remove(interaction_id)
    }

    pub(crate) fn resources(
        &self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&SessionResources, MirrorError> {
        let entry = self
            .sessions
            .get(session_id)
            .ok_or(MirrorError::UnknownSession)?;
        if entry.state.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(&entry.resources)
    }

    pub(crate) fn resources_mut(
        &mut self,
        session_id: &str,
        incarnation: u64,
    ) -> Result<&mut SessionResources, MirrorError> {
        let entry = self
            .sessions
            .get_mut(session_id)
            .ok_or(MirrorError::UnknownSession)?;
        if entry.state.incarnation != incarnation {
            return Err(MirrorError::StaleIncarnation);
        }
        Ok(&mut entry.resources)
    }

    /// Final physical removal is centralized so pending senders are answered and
    /// observer/materialization cancellation tokens are never silently discarded.
    pub(crate) fn cancel_resources(&mut self, session_id: &str, incarnation: u64, reason: &str) {
        if self
            .sessions
            .get(session_id)
            .is_none_or(|entry| entry.state.incarnation != incarnation)
        {
            return;
        }
        let owner = SessionResourceOwner::new(self.epoch.clone(), session_id, incarnation);
        let permission_ids = self
            .permission_owners
            .iter()
            .filter(|(_, route)| *route == &owner)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let elicitation_ids = self
            .elicitation_owners
            .iter()
            .filter(|(_, route)| *route == &owner)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let url_ids = self
            .url_owners
            .iter()
            .filter(|(_, route)| route.owner.as_ref() == Some(&owner))
            .map(|(id, route)| (id.clone(), route.registration_id.clone()))
            .collect::<Vec<_>>();
        self.permission_owners.retain(|_, route| route != &owner);
        self.elicitation_owners.retain(|_, route| route != &owner);
        self.url_owners
            .retain(|_, route| route.owner.as_ref() != Some(&owner));
        if !permission_ids.is_empty() || !elicitation_ids.is_empty() || !url_ids.is_empty() {
            self.effects.push_back(RuntimeEffect::ResourcesCancelled {
                session_id: session_id.to_string(),
                incarnation,
                permission_ids,
                elicitation_ids,
                url_ids,
                reason: reason.to_string(),
            });
        }
        self.sessions
            .get_mut(session_id)
            .expect("validated resource owner")
            .resources
            .cancel(reason);
    }

    /// A session ID the bridge once owned and fully released. Retirement
    /// filtering uses it to keep late Agent updates out of other sessions'
    /// creation replay; it never bans the ID from rematerializing.
    pub(crate) fn is_retired(&self, session_id: &str) -> bool {
        self.retired_ids.contains(session_id)
    }

    pub(crate) fn retire_entry(&mut self, session_id: &str, incarnation: u64, reason: &str) {
        if self
            .sessions
            .get(session_id)
            .is_none_or(|entry| entry.state.incarnation != incarnation)
        {
            return;
        }
        self.cancel_resources(session_id, incarnation, reason);
        self.sessions.remove(session_id);
        self.retired_ids.insert(session_id.to_string());
    }

    pub(crate) fn new(epoch: impl Into<String>) -> Self {
        let epoch = epoch.into();
        Self {
            history: HistoryCache::new(epoch.clone()),
            epoch,
            next_incarnation: 0,
            next_operation: 0,
            sessions: BTreeMap::new(),
            retired_ids: HashSet::new(),
            permission_owners: HashMap::new(),
            elicitation_owners: HashMap::new(),
            url_owners: HashMap::new(),
            overlay_bytes: 0,
            retained_terminals: HashMap::new(),
            retained_terminal_bytes: 0,
            connection_revision: 0,
            request_elicitations: BTreeMap::new(),
            request_url_flows: BTreeMap::new(),
            resolved_request_elicitations: VecDeque::new(),
            resolved_request_url_flows: VecDeque::new(),
            effects: VecDeque::new(),
            journal: RuntimeJournal::new(),
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new(Uuid::new_v4().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;

    use agent_client_protocol::schema::v1::{
        CreateElicitationResponse, ElicitationAction, RequestPermissionOutcome,
        RequestPermissionResponse,
    };
    use serde_json::json;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use crate::bridge::{SessionViewError, SessionViewWaiter};
    use crate::runtime_state::SessionOperationKind;
    use crate::semantic::SessionUpdateSemanticState;
    use crate::session_observation::ObservationLease;
    use crate::session_resources::{
        ElicitationResponder, MaterializationResources, PermissionResponder,
        ReplayValidationBackup, SessionUpdateOwner,
    };

    #[test]
    fn canonical_access_distinguishes_attachment_active_and_cleanup_owners() {
        let mut registry = SessionRegistry::default();
        let epoch = registry.epoch().to_string();
        let incarnation = registry
            .start_attachment(
                &epoch,
                "session",
                "/workspace",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        assert!(registry.active_session("session").is_none());
        assert_eq!(
            registry.resource_session("session").unwrap().live.cwd,
            std::path::Path::new("/workspace")
        );
        registry
            .complete_attachment(
                &epoch,
                "session",
                incarnation,
                "load",
                SessionOperationKind::Load,
                json!({}),
            )
            .unwrap();
        registry.register_new("session", incarnation);
        assert_eq!(
            registry
                .active_session("session")
                .unwrap()
                .state
                .incarnation,
            incarnation
        );
        registry
            .start_reload(
                &epoch,
                "session",
                incarnation,
                "reload",
                SessionOperationKind::Load,
            )
            .unwrap();
        assert_eq!(
            registry.allocated_session_count(),
            1,
            "reload reuses its existing allocation"
        );
        registry
            .fail_reload(&epoch, "session", incarnation, "reload", json!({}))
            .unwrap();
        registry
            .start_operation(
                &epoch,
                "session",
                incarnation,
                "close",
                SessionOperationKind::Close,
                "closing",
            )
            .unwrap();
        assert!(registry.active_session("session").is_none());
        assert!(registry.resource_session("session").is_some());
        registry
            .close_session(&epoch, "session", incarnation, "close")
            .unwrap();
        assert!(registry.resource_session("session").is_none());
        assert_eq!(
            registry.allocated_session_count(),
            1,
            "physical cleanup still owns its allocation"
        );
        registry
            .finish_session_cleanup(
                "session",
                incarnation,
                crate::session_state::SessionAdmission::Close,
                "close",
            )
            .unwrap();
        assert_eq!(registry.allocated_session_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn native_workspace_path_is_preserved_until_projection() {
        use std::os::unix::ffi::OsStringExt;
        let mut registry = SessionRegistry::default();
        let cwd =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/workspace-\xff".to_vec()));
        let epoch = registry.epoch().to_string();
        registry
            .open_new(&epoch, "session", cwd.clone(), json!({}))
            .unwrap();
        assert_eq!(registry.active_session("session").unwrap().live.cwd, cwd);
        assert_eq!(
            registry.session("session").unwrap().cwd,
            cwd.to_string_lossy()
        );
    }

    struct WaitingResources {
        permission: oneshot::Receiver<RequestPermissionResponse>,
        elicitation: oneshot::Receiver<CreateElicitationResponse>,
        view: oneshot::Receiver<Result<Value, SessionViewError>>,
        materialization: CancellationToken,
        observer: ObservationLease,
        validation: Arc<()>,
    }

    fn populate(
        registry: &mut SessionRegistry,
        session_id: &str,
        incarnation: u64,
    ) -> WaitingResources {
        let resources = registry.resources_mut(session_id, incarnation).unwrap();
        let (permission_sender, permission) = oneshot::channel();
        resources.permissions.insert(
            "permission".to_string(),
            PermissionResponder {
                tool_call_id: "tool".to_string(),
                option_ids: HashSet::from(["allow".to_string()]),
                sender: permission_sender,
            },
        );
        let (elicitation_sender, elicitation) = oneshot::channel();
        resources.elicitations.insert(
            "elicitation".to_string(),
            ElicitationResponder {
                url_elicitation_id: Some("url".to_string()),
                request: json!({ "message": "input" }),
                sender: elicitation_sender,
            },
        );
        let (view_sender, view) = oneshot::channel();
        let materialization = CancellationToken::new();
        resources.materialization = Some(MaterializationResources {
            attempt_id: "materialization".to_string(),
            waiters: vec![SessionViewWaiter::View(view_sender)],
            cancellation: materialization.clone(),
        });
        let observer = ObservationLease::default();
        resources.observers.observe(1, observer.clone());
        let validation = SessionUpdateSemanticState::default();
        let allocation = validation.allocation.clone();
        resources.validation = Some(validation);
        resources.replay_validation_backup = Some(ReplayValidationBackup {
            owner: SessionUpdateOwner {
                incarnation: Some(incarnation),
                allocation: allocation.clone(),
            },
            previous: Some(SessionUpdateSemanticState::default()),
        });
        resources.attachment.subscriber = Some(7);
        resources
            .attachment
            .control_candidate
            .updates
            .push(json!({ "sessionUpdate": "current_mode_update", "currentModeId": "plan" }));
        resources.attachment.control_candidate.bytes = 12;
        let owner = registry.resource_owner(session_id, incarnation).unwrap();
        registry
            .permission_owners
            .insert("permission".to_string(), owner.clone());
        registry
            .elicitation_owners
            .insert("elicitation".to_string(), owner.clone());
        registry.url_owners.insert(
            "url".to_string(),
            UrlRegistration {
                owner: Some(owner),
                registration_id: "accepted-url".to_string(),
            },
        );
        WaitingResources {
            permission,
            elicitation,
            view,
            materialization,
            observer,
            validation: allocation,
        }
    }

    fn assert_waiting(waiting: &mut WaitingResources) {
        assert!(matches!(
            waiting.permission.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            waiting.elicitation.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            waiting.view.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(!waiting.materialization.is_cancelled());
        assert!(!waiting.observer.is_cancelled());
    }

    fn assert_cancelled(waiting: &mut WaitingResources, reason: &str) {
        assert_eq!(
            serde_json::to_value(waiting.permission.try_recv().unwrap()).unwrap(),
            serde_json::to_value(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled
            ))
            .unwrap()
        );
        assert_eq!(
            serde_json::to_value(waiting.elicitation.try_recv().unwrap()).unwrap(),
            serde_json::to_value(CreateElicitationResponse::new(ElicitationAction::Cancel))
                .unwrap()
        );
        assert!(
            matches!(waiting.view.try_recv().unwrap(), Err(SessionViewError::Unavailable(message)) if message.contains(reason))
        );
        assert!(waiting.materialization.is_cancelled());
        assert!(waiting.observer.is_cancelled());
    }

    #[test]
    fn same_incarnation_history_reset_keeps_all_non_cloneable_resources() {
        let mut registry = SessionRegistry::new("epoch");
        registry.register_new("session", 1);
        let mut waiting = populate(&mut registry, "session", 1);
        registry.register_cold("session", 1);
        registry.clear_history_for_replacement("session", 1);
        registry.register_new("session", 1);
        assert_waiting(&mut waiting);
        let resources = registry.resources("session", 1).unwrap();
        assert!(Arc::ptr_eq(
            &resources.validation.as_ref().unwrap().allocation,
            &waiting.validation
        ));
        assert!(Arc::ptr_eq(
            &resources
                .replay_validation_backup
                .as_ref()
                .unwrap()
                .owner
                .allocation,
            &waiting.validation
        ));
        assert_eq!(resources.attachment.subscriber, Some(7));
        assert_eq!(resources.attachment.control_candidate.updates.len(), 1);
        assert_eq!(resources.attachment.control_candidate.bytes, 12);
        assert_eq!(
            resources.materialization.as_ref().unwrap().attempt_id,
            "materialization"
        );
    }

    #[test]
    fn physical_removal_answers_waiters_and_responders_and_cancels_leases() {
        let mut registry = SessionRegistry::new("epoch");
        registry.register_new("session", 1);
        let mut waiting = populate(&mut registry, "session", 1);
        registry.remove("session", 2);
        assert_waiting(&mut waiting);
        registry.remove("session", 1);
        assert_waiting(&mut waiting);
        assert!(
            registry.state("session").is_some(),
            "materialization retains its owner until its specific result is delivered"
        );
        registry.cancel_resources("session", 1, "session was removed");
        registry.remove("session", 1);
        assert!(registry.state("session").is_none());
        assert!(registry.permission_owners.is_empty());
        assert!(registry.elicitation_owners.is_empty());
        assert!(registry.url_owners.is_empty());
        assert_cancelled(&mut waiting, "removed");
    }

    #[test]
    fn replacement_retires_old_resources_and_stale_cleanup_preserves_new_resources() {
        let mut registry = SessionRegistry::new("epoch");
        registry.register_new("session", 1);
        let mut old = populate(&mut registry, "session", 1);
        let incarnation = registry
            .start_attachment(
                "epoch",
                "session",
                "/work",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        assert!(incarnation > 1);
        assert_cancelled(&mut old, "replaced");
        assert!(
            registry
                .resources("session", incarnation)
                .unwrap()
                .validation
                .is_none()
        );
        let mut current = populate(&mut registry, "session", incarnation);
        registry.remove("session", 1);
        assert_waiting(&mut current);
        assert!(matches!(
            registry.resources("session", 1),
            Err(MirrorError::StaleIncarnation)
        ));
    }

    #[test]
    fn failed_live_attachment_preserves_waiters_until_explicit_owner_removal() {
        let mut registry = SessionRegistry::new("epoch");
        let incarnation = registry
            .start_attachment(
                "epoch",
                "session",
                "/work",
                "load",
                SessionOperationKind::Load,
            )
            .unwrap();
        let mut waiting = populate(&mut registry, "session", incarnation);
        registry
            .fail_attachment(
                "epoch",
                "session",
                incarnation,
                "load",
                SessionOperationKind::Load,
                json!({}),
            )
            .unwrap();
        assert!(registry.live("session").is_none());
        assert!(registry.state("session").is_some());
        assert_waiting(&mut waiting);
        registry.remove("session", incarnation);
        assert_waiting(&mut waiting);
        registry.cancel_resources("session", incarnation, "session was removed");
        registry.remove("session", incarnation);
        assert_cancelled(&mut waiting, "removed");
    }

    #[test]
    fn registry_drop_answers_outstanding_work_instead_of_dropping_senders() {
        let mut registry = SessionRegistry::new("epoch");
        registry.register_new("session", 1);
        let mut waiting = populate(&mut registry, "session", 1);
        drop(registry);
        assert_cancelled(&mut waiting, "dropped");
    }
}
