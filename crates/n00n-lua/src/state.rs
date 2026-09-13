use n00n_agent::tools::SessionIdentity;
use n00n_storage::{
    id::n00nId,
    sessions::{
        MAX_PLUGIN_STATE_BYTES, SessionStateError, StoredSessionStateSnapshot, StoredStateScope,
    },
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, MutexGuard},
};

pub(crate) const PLUGIN_STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PluginStateScope {
    Session,
    Root,
}

impl PluginStateScope {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "session" => Some(Self::Session),
            "root" => Some(Self::Root),
            _ => None,
        }
    }

    const fn stored(self) -> StoredStateScope {
        match self {
            Self::Session => StoredStateScope::Session,
            Self::Root => StoredStateScope::Root,
        }
    }
}

impl From<StoredStateScope> for PluginStateScope {
    fn from(scope: StoredStateScope) -> Self {
        match scope {
            StoredStateScope::Session => Self::Session,
            StoredStateScope::Root => Self::Root,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PluginStateIdentity {
    session_id: n00nId,
    root_session_id: n00nId,
}

impl PluginStateIdentity {
    pub(crate) fn owner(&self, scope: PluginStateScope) -> n00nId {
        match scope {
            PluginStateScope::Session => self.session_id,
            PluginStateScope::Root => self.root_session_id,
        }
    }

    fn owns_scope(&self, scope: PluginStateScope) -> bool {
        matches!(scope, PluginStateScope::Session) || self.is_root()
    }

    pub(crate) fn is_root(&self) -> bool {
        self.session_id == self.root_session_id
    }

    fn scope_identity(&self, scope: PluginStateScope) -> Self {
        match scope {
            PluginStateScope::Session => self.clone(),
            PluginStateScope::Root => Self {
                session_id: self.root_session_id,
                root_session_id: self.root_session_id,
            },
        }
    }
}

impl From<&SessionIdentity> for PluginStateIdentity {
    fn from(identity: &SessionIdentity) -> Self {
        Self {
            session_id: identity.session_id().id(),
            root_session_id: identity.root_session_id().id(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StateKey {
    plugin: String,
    scope: PluginStateScope,
    owner: n00nId,
}

impl StateKey {
    fn new(plugin: &str, scope: PluginStateScope, identity: &PluginStateIdentity) -> Self {
        Self {
            plugin: plugin.to_owned(),
            scope,
            owner: identity.owner(scope),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PluginStateError {
    #[error("plugin state is {bytes} bytes (maximum {maximum})")]
    ValueTooLarge { bytes: usize, maximum: usize },
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Snapshot(#[from] SessionStateError),
    #[error("plugin state lock poisoned")]
    Poisoned,
}

#[derive(Default)]
struct StateInner {
    values: HashMap<StateKey, Value>,
    managed: HashSet<StateKey>,
    bases: HashMap<PluginStateIdentity, StoredSessionStateSnapshot>,
}

#[derive(Default)]
pub(crate) struct PluginStateStore {
    inner: Mutex<StateInner>,
}

impl PluginStateStore {
    fn lock(&self) -> Result<MutexGuard<'_, StateInner>, PluginStateError> {
        self.inner.lock().map_err(|_| {
            tracing::warn!("plugin state lock poisoned");
            PluginStateError::Poisoned
        })
    }

    /// Acquire the lock through a poison: a panic while a previous caller held
    /// the guard cannot corrupt `StateInner` (each `HashMap`/`HashSet` op is
    /// panic-atomic per collection), so reads and clears may proceed. Clearing
    /// the poison flag here restores the store for subsequent strict writes.
    fn lock_recover(&self) -> MutexGuard<'_, StateInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("plugin state lock poisoned; recovering");
                self.inner.clear_poison();
                poisoned.into_inner()
            }
        }
    }

    #[must_use]
    pub(crate) fn get(
        &self,
        plugin: &str,
        scope: PluginStateScope,
        identity: &PluginStateIdentity,
    ) -> Option<Value> {
        self.lock_recover()
            .values
            .get(&StateKey::new(plugin, scope, identity))
            .cloned()
    }

    pub(crate) fn replace(
        &self,
        plugin: &str,
        scope: PluginStateScope,
        identity: &PluginStateIdentity,
        value: Value,
    ) -> Result<Option<Value>, PluginStateError> {
        validate_value_size(&value)?;
        let key = StateKey::new(plugin, scope, identity);
        let mut inner = self.lock()?;
        validate_replacement(&inner, &identity.scope_identity(scope), &key, &value)?;
        inner.managed.insert(key.clone());
        Ok(inner.values.insert(key, value))
    }

    pub(crate) fn remove(
        &self,
        plugin: &str,
        scope: PluginStateScope,
        identity: &PluginStateIdentity,
    ) -> Result<Option<Value>, PluginStateError> {
        let key = StateKey::new(plugin, scope, identity);
        let mut inner = self.lock()?;
        validate_removal(&inner, &identity.scope_identity(scope), &key)?;
        inner.managed.insert(key.clone());
        Ok(inner.values.remove(&key))
    }

    pub(crate) fn hydrate(
        &self,
        identity: PluginStateIdentity,
        mut snapshot: Option<StoredSessionStateSnapshot>,
    ) -> Result<(), PluginStateError> {
        let entries = match snapshot.as_ref() {
            Some(snapshot) => snapshot_entries(snapshot)?,
            None => Vec::new(),
        };
        if !identity.owns_scope(PluginStateScope::Root)
            && let Some(snapshot) = snapshot.as_mut()
        {
            let root_plugins = snapshot
                .plugin_names_with_scope(StoredStateScope::Root)?
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            for plugin in root_plugins {
                snapshot.remove_plugin_state(&plugin, StoredStateScope::Root)?;
            }
        }

        let mut inner = self.lock()?;
        clear_identity_runtime(&mut inner, &identity, false);
        for (plugin, scope, payload) in entries {
            if !identity.owns_scope(scope) {
                continue;
            }
            let key = StateKey::new(&plugin, scope, &identity);
            inner.managed.insert(key.clone());
            inner.values.insert(key, payload);
        }
        if let Some(snapshot) = snapshot {
            inner.bases.insert(identity, snapshot);
        } else {
            inner.bases.remove(&identity);
        }
        Ok(())
    }

    pub(crate) fn capture(
        &self,
        identity: &PluginStateIdentity,
        revision: u64,
    ) -> Result<StoredSessionStateSnapshot, PluginStateError> {
        let mut inner = self.lock()?;
        let mut candidate = candidate_for(&inner, identity, None)?;
        candidate.set_state_revision(revision)?;
        inner.bases.insert(identity.clone(), candidate.clone());
        Ok(candidate)
    }

    pub(crate) fn reset(&self, identity: &PluginStateIdentity) {
        let mut guard = self.lock_recover();
        clear_identity_runtime(&mut guard, identity, true);
    }

    pub(crate) fn drop_owner(&self, owner: n00nId) {
        let mut inner = self.lock_recover();
        inner.values.retain(|key, _| key.owner != owner);
        inner.managed.retain(|key| key.owner != owner);
        inner.bases.retain(|identity, _| {
            identity.session_id != owner && identity.root_session_id != owner
        });
    }
}

fn validate_replacement(
    inner: &StateInner,
    identity: &PluginStateIdentity,
    replacement_key: &StateKey,
    replacement_value: &Value,
) -> Result<(), PluginStateError> {
    let mut candidate = candidate_for(inner, identity, Some(replacement_key))?;
    candidate.set_plugin_state(
        &replacement_key.plugin,
        PLUGIN_STATE_SCHEMA_VERSION,
        replacement_key.scope.stored(),
        replacement_value.clone(),
    )?;
    Ok(())
}

fn candidate_for(
    inner: &StateInner,
    identity: &PluginStateIdentity,
    skipped: Option<&StateKey>,
) -> Result<StoredSessionStateSnapshot, PluginStateError> {
    let mut candidate = inner
        .bases
        .get(identity)
        .cloned()
        .unwrap_or_else(|| StoredSessionStateSnapshot::new(0));
    for key in inner
        .managed
        .iter()
        .filter(|key| identity.owns_scope(key.scope) && key.owner == identity.owner(key.scope))
    {
        if skipped.map_or(false, |s| key == s) {
            continue;
        }
        if let Some(value) = inner.values.get(key) {
            candidate.set_plugin_state(
                &key.plugin,
                PLUGIN_STATE_SCHEMA_VERSION,
                key.scope.stored(),
                value.clone(),
            )?;
        } else {
            candidate.remove_plugin_state(&key.plugin, key.scope.stored())?;
        }
    }
    Ok(candidate)
}

fn validate_value_size(value: &Value) -> Result<(), PluginStateError> {
    let bytes = serde_json::to_vec(value)?.len();
    if bytes > MAX_PLUGIN_STATE_BYTES {
        return Err(PluginStateError::ValueTooLarge {
            bytes,
            maximum: MAX_PLUGIN_STATE_BYTES,
        });
    }

    Ok(())
}

fn validate_removal(
    inner: &StateInner,
    identity: &PluginStateIdentity,
    removal_key: &StateKey,
) -> Result<(), PluginStateError> {
    let mut candidate = candidate_for(inner, identity, Some(removal_key))?;
    candidate.remove_plugin_state(&removal_key.plugin, removal_key.scope.stored())?;
    Ok(())
}

fn snapshot_entries(
    snapshot: &StoredSessionStateSnapshot,
) -> Result<Vec<(String, PluginStateScope, Value)>, PluginStateError> {
    snapshot
        .plugin_entries_for_apply(PLUGIN_STATE_SCHEMA_VERSION)?
        .into_iter()
        .map(|entry| {
            validate_value_size(entry.payload)?;
            Ok((
                entry.plugin.to_owned(),
                entry.scope.into(),
                entry.payload.clone(),
            ))
        })
        .collect()
}

fn clear_identity_runtime(
    inner: &mut StateInner,
    identity: &PluginStateIdentity,
    mark_managed: bool,
) {
    let keys = inner
        .values
        .keys()
        .filter(|key| identity.owns_scope(key.scope) && key.owner == identity.owner(key.scope))
        .cloned()
        .collect::<Vec<_>>();
    inner
        .values
        .retain(|key, _| !identity.owns_scope(key.scope) || key.owner != identity.owner(key.scope));
    if mark_managed {
        inner.managed.extend(keys);
    } else {
        inner.managed.retain(|key| {
            !identity.owns_scope(key.scope) || key.owner != identity.owner(key.scope)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PluginStateError, PluginStateIdentity, PluginStateScope, PluginStateStore, StateKey,
    };
    use n00n_agent::tools::SessionIdentity;
    use n00n_storage::{
        id::{SessionRef, n00nId},
        sessions::{
            MAX_PLUGIN_STATE_BYTES, SESSION_STATE_SCHEMA_VERSION, SessionMeta,
            StoredSessionStateSnapshot, StoredStateScope,
        },
    };
    use serde_json::json;
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    fn identity() -> PluginStateIdentity {
        PluginStateIdentity::from(&SessionIdentity::child(
            SessionRef::generate(),
            SessionRef::generate(),
        ))
    }

    #[test]
    fn identity_uses_canonical_ids() {
        let raw = "550e8400-e29b-41d4-a716-446655440000";
        let id = raw.parse::<n00nId>().unwrap();
        let legacy = SessionIdentity::root(raw.parse::<SessionRef>().unwrap());
        let canonical = SessionIdentity::root(SessionRef::from_id(id));

        assert_eq!(
            PluginStateIdentity::from(&legacy),
            PluginStateIdentity::from(&canonical)
        );
    }

    #[test]
    fn plugin_scope_and_owner_are_all_part_of_the_key() {
        let store = PluginStateStore::default();
        let root = SessionRef::generate();
        let a = PluginStateIdentity::from(&SessionIdentity::child(
            SessionRef::generate(),
            root.clone(),
        ));
        let b = PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root));
        store
            .replace("one", PluginStateScope::Session, &a, json!(1))
            .unwrap();
        store
            .replace("two", PluginStateScope::Session, &a, json!(2))
            .unwrap();
        store
            .replace("one", PluginStateScope::Session, &b, json!(3))
            .unwrap();
        store
            .replace("one", PluginStateScope::Root, &a, json!(4))
            .unwrap();

        assert_eq!(
            store.get("one", PluginStateScope::Session, &a),
            Some(json!(1))
        );
        assert_eq!(
            store.get("two", PluginStateScope::Session, &a),
            Some(json!(2))
        );
        assert_eq!(
            store.get("one", PluginStateScope::Session, &b),
            Some(json!(3))
        );
        assert_eq!(store.get("one", PluginStateScope::Root, &b), Some(json!(4)));
    }

    #[test]
    fn oversized_replace_is_atomic() {
        let store = PluginStateStore::default();
        let identity = identity();
        store
            .replace(
                "plugin",
                PluginStateScope::Session,
                &identity,
                json!("kept"),
            )
            .unwrap();

        let error = store
            .replace(
                "plugin",
                PluginStateScope::Session,
                &identity,
                json!("x".repeat(MAX_PLUGIN_STATE_BYTES)),
            )
            .unwrap_err();

        assert!(matches!(error, PluginStateError::ValueTooLarge { .. }));
        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &identity),
            Some(json!("kept"))
        );
    }

    #[test]
    fn capture_preserves_opaque_data_and_advances_revision() {
        let store = PluginStateStore::default();
        let identity = identity();
        let snapshot = serde_json::from_value(json!({
            "schema_version": SESSION_STATE_SCHEMA_VERSION,
            "state_revision": 3,
            "future": {"kept": true},
            "plugins": {"unknown": {"future": {"raw": "kept"}}}
        }))
        .unwrap();
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();
        store
            .replace(
                "known",
                PluginStateScope::Session,
                &identity,
                json!({"v": 1}),
            )
            .unwrap();

        let captured = store.capture(&identity, 4).unwrap();
        let raw = serde_json::to_value(captured).unwrap();
        assert_eq!(raw["state_revision"], json!(4));
        assert_eq!(raw["future"], json!({"kept": true}));
        assert_eq!(raw["plugins"]["unknown"]["future"], json!({"raw": "kept"}));
    }

    #[test]
    fn future_and_malformed_hydrates_leave_current_state_unchanged() {
        let store = PluginStateStore::default();
        let identity = identity();
        store
            .replace(
                "plugin",
                PluginStateScope::Session,
                &identity,
                json!("kept"),
            )
            .unwrap();
        let future = serde_json::from_value(json!({
            "schema_version": SESSION_STATE_SCHEMA_VERSION + 1,
            "state_revision": 7,
            "opaque": true
        }))
        .unwrap();
        assert!(store.hydrate(identity.clone(), Some(future)).is_err());
        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &identity),
            Some(json!("kept"))
        );

        let meta: SessionMeta = serde_json::from_value(json!({
            "state_snapshot": {
                "schema_version": SESSION_STATE_SCHEMA_VERSION,
                "plugins": {}
            }
        }))
        .unwrap();
        assert!(
            store
                .hydrate(identity.clone(), meta.state_snapshot)
                .is_err()
        );
        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &identity),
            Some(json!("kept"))
        );
    }

    #[test]
    fn hydrate_replaces_pending_runtime_removals() {
        let store = PluginStateStore::default();
        let identity = identity();
        store
            .replace("plugin", PluginStateScope::Session, &identity, json!("old"))
            .unwrap();
        store
            .remove("plugin", PluginStateScope::Session, &identity)
            .unwrap();

        let mut snapshot = StoredSessionStateSnapshot::new(2);
        snapshot
            .set_plugin_state("plugin", 1, StoredStateScope::Session, json!("hydrated"))
            .unwrap();
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();

        let captured = store.capture(&identity, 3).unwrap();
        assert_eq!(
            captured
                .plugin_payload_for_apply("plugin", 1, StoredStateScope::Session)
                .unwrap(),
            Some(&json!("hydrated"))
        );
    }

    #[test]
    fn failed_revision_regression_does_not_replace_base() {
        let store = PluginStateStore::default();
        let identity = identity();
        let snapshot = StoredSessionStateSnapshot::new(5);
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();

        assert!(store.capture(&identity, 4).is_err());
        assert_eq!(
            store.capture(&identity, 6).unwrap().state_revision(),
            Some(6)
        );
    }

    #[test]
    fn reset_removes_managed_state_but_preserves_opaque_base() {
        let store = PluginStateStore::default();
        let identity = identity();
        let mut snapshot = StoredSessionStateSnapshot::new(1);
        snapshot
            .set_plugin_state("known", 1, StoredStateScope::Session, json!("old"))
            .unwrap();
        snapshot
            .set_plugin_state("future", 2, StoredStateScope::Session, json!("opaque"))
            .unwrap();
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();

        store.reset(&identity);
        let captured = store.capture(&identity, 2).unwrap();
        assert_eq!(
            captured
                .plugin_payload_for_apply("known", 1, StoredStateScope::Session)
                .unwrap(),
            None
        );
        assert_eq!(
            captured
                .plugin_payload_for_apply("future", 2, StoredStateScope::Session)
                .unwrap(),
            Some(&json!("opaque"))
        );
    }

    #[test]
    fn replacement_rejects_uncapturable_names_and_entry_counts_atomically() {
        let store = PluginStateStore::default();
        let identity = identity();
        assert!(
            store
                .replace(
                    "bad/name",
                    PluginStateScope::Session,
                    &identity,
                    json!(true),
                )
                .is_err()
        );
        assert_eq!(
            store.get("bad/name", PluginStateScope::Session, &identity),
            None
        );

        for index in 0..64 {
            store
                .replace(
                    &format!("plugin_{index}"),
                    PluginStateScope::Session,
                    &identity,
                    json!(index),
                )
                .unwrap();
        }
        assert!(
            store
                .replace("plugin_64", PluginStateScope::Session, &identity, json!(64),)
                .is_err()
        );
        assert_eq!(
            store.get("plugin_64", PluginStateScope::Session, &identity),
            None
        );
    }

    #[test]
    fn child_hydration_cannot_clobber_live_root_state() {
        let store = PluginStateStore::default();
        let root_ref = SessionRef::generate();
        let root = PluginStateIdentity::from(&SessionIdentity::root(root_ref.clone()));
        let child =
            PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root_ref));
        store
            .replace("plugin", PluginStateScope::Root, &root, json!("live"))
            .unwrap();
        let mut child_snapshot = StoredSessionStateSnapshot::new(1);
        child_snapshot
            .set_plugin_state("plugin", 1, StoredStateScope::Root, json!("stale"))
            .unwrap();

        store.hydrate(child.clone(), Some(child_snapshot)).unwrap();

        assert_eq!(
            store.get("plugin", PluginStateScope::Root, &child),
            Some(json!("live"))
        );
    }

    #[test]
    fn child_root_replacement_uses_root_snapshot_limits() {
        let store = PluginStateStore::default();
        let root_ref = SessionRef::generate();
        let root = PluginStateIdentity::from(&SessionIdentity::root(root_ref.clone()));
        let child =
            PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root_ref));
        for index in 0..64 {
            store
                .replace(
                    &format!("plugin_{index}"),
                    PluginStateScope::Root,
                    &root,
                    json!(index),
                )
                .unwrap();
        }

        assert!(
            store
                .replace("plugin_64", PluginStateScope::Root, &child, json!(64),)
                .is_err()
        );
        assert_eq!(store.get("plugin_64", PluginStateScope::Root, &root), None);
    }

    #[test]
    fn removal_from_malformed_container_is_atomic() {
        let store = PluginStateStore::default();
        let root_ref = SessionRef::generate();
        let identity = PluginStateIdentity::from(&SessionIdentity::root(root_ref));
        let snapshot: StoredSessionStateSnapshot = serde_json::from_value(json!({
            "schema_version": SESSION_STATE_SCHEMA_VERSION,
            "state_revision": 1,
            "plugins": {"plugin": null}
        }))
        .unwrap();
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();

        assert!(
            store
                .remove("plugin", PluginStateScope::Root, &identity)
                .is_err()
        );
        let captured = store.capture(&identity, 2).unwrap();
        assert_eq!(
            serde_json::to_value(captured).unwrap()["plugins"]["plugin"],
            json!(null)
        );
    }

    #[test]
    fn capture_rejects_candidate_mutation_failure_without_replacing_base() {
        let store = PluginStateStore::default();
        let identity = identity();
        let snapshot: StoredSessionStateSnapshot = serde_json::from_value(json!({
            "schema_version": SESSION_STATE_SCHEMA_VERSION,
            "state_revision": 1,
            "plugins": {"plugin": null}
        }))
        .unwrap();
        store.hydrate(identity.clone(), Some(snapshot)).unwrap();

        let key = StateKey::new("plugin", PluginStateScope::Session, &identity);
        {
            let mut inner = store.lock().unwrap();
            inner.managed.insert(key.clone());
            inner.values.insert(key, json!("new"));
        }

        assert!(store.capture(&identity, 2).is_err());
        let inner = store.lock().unwrap();
        let base = &inner.bases[&identity];
        assert_eq!(base.state_revision(), Some(1));
        assert_eq!(
            serde_json::to_value(base).unwrap()["plugins"]["plugin"],
            json!(null)
        );
    }

    #[test]
    fn concurrent_child_mutations_remain_isolated() {
        let store = Arc::new(PluginStateStore::default());
        let root_ref = SessionRef::generate();
        let root = PluginStateIdentity::from(&SessionIdentity::root(root_ref.clone()));
        let first = PluginStateIdentity::from(&SessionIdentity::child(
            SessionRef::generate(),
            root_ref.clone(),
        ));
        let second =
            PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root_ref));
        store
            .replace("plugin", PluginStateScope::Root, &root, json!("root"))
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let workers = [first.clone(), second.clone()].map(|identity| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for value in 0..100 {
                    store
                        .replace("plugin", PluginStateScope::Session, &identity, json!(value))
                        .unwrap();
                }
            })
        });
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &first),
            Some(json!(99))
        );
        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &second),
            Some(json!(99))
        );
        assert_eq!(
            store.get("plugin", PluginStateScope::Root, &first),
            Some(json!("root"))
        );
    }

    #[test]
    fn dropping_child_owner_preserves_root_and_sibling_state() {
        let store = PluginStateStore::default();
        let root_ref = SessionRef::generate();
        let root = PluginStateIdentity::from(&SessionIdentity::root(root_ref.clone()));
        let child = PluginStateIdentity::from(&SessionIdentity::child(
            SessionRef::generate(),
            root_ref.clone(),
        ));
        let sibling =
            PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root_ref));
        store
            .replace("plugin", PluginStateScope::Root, &root, json!("root"))
            .unwrap();
        store
            .replace("plugin", PluginStateScope::Session, &child, json!("child"))
            .unwrap();
        store
            .replace(
                "plugin",
                PluginStateScope::Session,
                &sibling,
                json!("sibling"),
            )
            .unwrap();

        store.drop_owner(child.session_id);

        assert_eq!(store.get("plugin", PluginStateScope::Session, &child), None);
        assert_eq!(
            store.get("plugin", PluginStateScope::Session, &sibling),
            Some(json!("sibling"))
        );
        assert_eq!(
            store.get("plugin", PluginStateScope::Root, &child),
            Some(json!("root"))
        );
    }

    #[test]
    fn child_capture_does_not_emit_supported_root_state() {
        let store = PluginStateStore::default();
        let root_ref = SessionRef::generate();
        let child =
            PluginStateIdentity::from(&SessionIdentity::child(SessionRef::generate(), root_ref));
        let mut snapshot = StoredSessionStateSnapshot::new(1);
        snapshot
            .set_plugin_state("plugin", 1, StoredStateScope::Root, json!("stale"))
            .unwrap();
        snapshot
            .set_plugin_state("plugin", 1, StoredStateScope::Session, json!("session"))
            .unwrap();
        snapshot
            .set_plugin_state("future", 2, StoredStateScope::Root, json!("opaque"))
            .unwrap();
        store.hydrate(child.clone(), Some(snapshot)).unwrap();

        let captured = store.capture(&child, 2).unwrap();
        assert_eq!(
            captured
                .plugin_payload_for_apply("plugin", 1, StoredStateScope::Root)
                .unwrap(),
            None
        );
        assert_eq!(
            captured
                .plugin_payload_for_apply("future", 2, StoredStateScope::Root)
                .unwrap(),
            None
        );
        assert_eq!(
            captured
                .plugin_payload_for_apply("plugin", 1, StoredStateScope::Session)
                .unwrap(),
            Some(&json!("session"))
        );
    }
}
