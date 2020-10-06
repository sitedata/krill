use std::collections::HashMap;
use std::fmt;

use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use chrono::Duration;

use rpki::x509::Time;

use crate::commons::api::{CommandHistory, CommandHistoryCriteria, CommandHistoryRecord, Handle, Label};
use crate::commons::eventsourcing::cmd::{Command, StoredCommandBuilder};
use crate::commons::eventsourcing::{
    Aggregate, Event, EventListener, KeyStoreKey, KeyValueError, KeyValueStore, StoredCommand, WithStorableDetails,
};

const SNAPSHOT_FREQ: u64 = 5;

pub type StoreResult<T> = Result<T, AggregateStoreError>;

//------------ Storable ------------------------------------------------------

pub trait Storable: Clone + Serialize + DeserializeOwned + Sized + 'static {}
impl<T: Clone + Serialize + DeserializeOwned + Sized + 'static> Storable for T {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StoredValueInfo {
    pub snapshot_version: u64,
    pub last_event: u64,
    pub last_command: u64,
    pub last_update: Time,
}

impl Default for StoredValueInfo {
    fn default() -> Self {
        StoredValueInfo {
            snapshot_version: 0,
            last_event: 0,
            last_command: 0,
            last_update: Time::now(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
// Do NOT EVER change the order.. this is used to check whether migrations are needed
pub enum KeyStoreVersion {
    Pre0_6,
    V0_6,
    V0_7,
    V0_8,
}

//------------ CommandKey ----------------------------------------------------

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandKey {
    pub sequence: u64,
    pub timestamp_secs: i64,
    pub label: Label,
}

impl CommandKey {
    pub fn new(sequence: u64, time: Time, label: Label) -> Self {
        CommandKey {
            sequence,
            timestamp_secs: time.timestamp(),
            label,
        }
    }

    pub fn for_stored<S: WithStorableDetails>(command: &StoredCommand<S>) -> CommandKey {
        CommandKey::new(command.sequence(), command.time(), command.details().summary().label)
    }

    pub fn matches_crit(&self, crit: &CommandHistoryCriteria) -> bool {
        crit.matches_timestamp_secs(self.timestamp_secs) && crit.matches_label(&self.label)
    }
}

impl fmt::Display for CommandKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "command--{}--{}--{}", self.timestamp_secs, self.sequence, self.label)
    }
}

impl FromStr for CommandKey {
    type Err = CommandKeyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split("--").collect();
        if parts.len() != 4 || parts[0] != "command" {
            Err(CommandKeyError(s.to_string()))
        } else {
            let timestamp_secs = i64::from_str(&parts[1]).map_err(|_| CommandKeyError(s.to_string()))?;
            let sequence = u64::from_str(&parts[2]).map_err(|_| CommandKeyError(s.to_string()))?;
            let end = parts[3].to_string();
            if !end.ends_with(".json") {
                Err(CommandKeyError(s.to_string()))
            } else {
                let label = (end[0..end.len() - 5]).to_string();
                Ok(CommandKey {
                    sequence,
                    timestamp_secs,
                    label,
                })
            }
        }
    }
}

//------------ CommandKeyError -----------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandKeyError(String);

impl fmt::Display for CommandKeyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid command key: {}", self.0)
    }
}

//------------ AggregateStore ------------------------------------------------

/// This type is responsible for persisting Aggregates.
pub struct AggregateStore<A: Aggregate> {
    kv: KeyValueStore,
    cache: RwLock<HashMap<Handle, Arc<A>>>,
    listeners: Vec<Arc<dyn EventListener<A>>>,
    outer_lock: RwLock<()>,
}

/// # Starting up
///
impl<A: Aggregate> AggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    pub fn new(work_dir: &PathBuf, name_space: &str) -> StoreResult<Self> {
        let mut path = work_dir.clone();
        path.push(name_space);
        let existed = path.exists();

        let kv = KeyValueStore::disk(work_dir, name_space)?;
        let cache = RwLock::new(HashMap::new());
        let listeners = vec![];
        let outer_lock = RwLock::new(());

        let store = AggregateStore {
            kv,
            cache,
            listeners,
            outer_lock,
        };

        if !existed {
            store.set_version(&KeyStoreVersion::V0_8)?;
        }

        Ok(store)
    }

    /// Warms up the cache, to be used after startup. Will fail if any aggregates fail to load.
    /// In that case the user may want to use the recover option to see what can be salvaged.
    pub fn warm(&self) -> StoreResult<()> {
        for handle in self.list()? {
            let _ = self
                .get_latest(&handle)
                .map_err(|e| AggregateStoreError::WarmupFailed(handle, e.to_string()))?;
        }
        Ok(())
    }

    /// Recovers the aggregates by verifying all commands, and the corresponding events.
    /// Use this in case the state on disk is found to be inconsistent. I.e. the `warm`
    /// function failed and Krill exited.
    pub fn recover(&self) -> StoreResult<()> {
        let criteria = CommandHistoryCriteria::default();
        for handle in self.list()? {
            info!("Will recover state for '{}'", &handle);

            // Check
            // - All commands, archive bad commands
            // - All events, archive bad events
            // - Keep track of last known good command and event
            // - Archive all commands and events after
            //
            // Rebuild state up to event:
            //   - use snapshot - archive if bad
            //   - use back-up snapshot if snapshot is no good - archive if bad
            //   - start from init event if back-up snapshot is bad, or if the version exceeds last good event
            //   - process events from (back-up) snapshot up to last good event
            //
            //  If still good:
            //   - save snapshot
            //   - save info

            let mut last_good_cmd = 0;
            let mut last_good_evt = 0;
            let mut last_update = Time::now();

            // Check all commands and associated events
            let mut hunkydory = true;
            for command_key in self.command_keys_ascending(&handle, &criteria)? {
                if hunkydory {
                    if let Ok(cmd) = self.get_command::<A::StorableCommandDetails>(&handle, &command_key) {
                        if let Some(events) = cmd.effect().events() {
                            for version in events {
                                // TODO: When archiving is in place, allow missing (archived) events as long as they are from before the snapshot or backup snapshot
                                if let Ok(Some(_)) = self.get_event::<A::Event>(&handle, *version) {
                                    last_good_evt = *version;
                                } else {
                                    hunkydory = false;
                                }
                            }
                        }
                        last_good_cmd = cmd.sequence();
                        last_update = cmd.time();
                    } else {
                        hunkydory = false;
                    }
                }
                if !hunkydory {
                    // Bad command or event encountered.. archive surplus commands
                    // note that we will clean surplus events later
                    self.archive_surplus_command(&handle, &command_key)?;
                }
            }

            self.archive_surplus_events(&handle, last_good_evt + 1)?;

            if !hunkydory {
                warn!(
                    "State for '{}' can only be recovered to version: {}. Check corrupt and surplus dirs",
                    &handle, last_good_evt
                );
            }

            let agg = self
                .get_aggregate(&handle, Some(last_good_evt))?
                .ok_or_else(|| AggregateStoreError::CouldNotRecover(handle.clone()))?;

            let snapshot_version = agg.version();

            let info = StoredValueInfo {
                last_event: last_good_evt,
                last_command: last_good_cmd,
                last_update,
                snapshot_version,
            };

            self.store_snapshot(&handle, &agg)?;

            self.cache_update(&handle, Arc::new(agg));

            self.save_info(&handle, &info)?;
        }

        Ok(())
    }

    /// Adds a listener that will receive a reference to all events as they
    /// are stored.
    pub fn add_listener<L: EventListener<A>>(&mut self, listener: Arc<L>) {
        let _lock = self.outer_lock.write().unwrap();
        self.listeners.push(listener)
    }
}

/// # Manage Aggregates
///
impl<A: Aggregate> AggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    /// Gets the latest version for the given aggregate. Returns
    /// an AggregateStoreError::UnknownAggregate in case the aggregate
    /// does not exist.
    pub fn get_latest(&self, handle: &Handle) -> StoreResult<Arc<A>> {
        let _lock = self.outer_lock.read().unwrap();
        self.get_latest_no_lock(handle)
    }

    /// Adds a new aggregate instance based on the init event.
    pub fn add(&self, init: A::InitEvent) -> StoreResult<Arc<A>> {
        let _lock = self.outer_lock.write().unwrap();

        self.store_event(&init)?;

        let handle = init.handle().clone();

        let aggregate = A::init(init).map_err(|_| AggregateStoreError::InitError(handle.clone()))?;
        self.store_snapshot(&handle, &aggregate)?;

        let info = StoredValueInfo::default();
        self.save_info(&handle, &info)?;

        let arc = Arc::new(aggregate);
        self.cache_update(&handle, arc.clone());

        Ok(arc)
    }

    /// Sends a command to the appropriate aggregate, and on
    /// success: save command and events, return aggregate
    /// no-op: do not save anything, return aggregate
    /// error: save command and error, return error
    pub fn command(&self, cmd: A::Command) -> Result<Arc<A>, A::Error> {
        let _lock = self.outer_lock.write().unwrap();

        // Get the latest arc.
        let handle = cmd.handle().clone();

        let mut info = self.get_info(&handle)?;
        info.last_update = Time::now();
        info.last_command += 1;

        let mut latest = self.get_latest_no_lock(&handle)?;

        if let Some(version) = cmd.version() {
            if version != latest.version() {
                error!(
                    "Version conflict updating '{}', expected version: {}, found: {}",
                    handle,
                    version,
                    latest.version()
                );

                return Err(A::Error::from(AggregateStoreError::ConcurrentModification(handle)));
            }
        }

        let stored_command_builder = StoredCommandBuilder::new(&cmd, latest.version(), info.last_command);

        let res = match latest.process_command(cmd) {
            Err(e) => {
                let stored_command = stored_command_builder.finish_with_error(&e);
                self.store_command(stored_command)?;
                Err(e)
            }
            Ok(events) => {
                if events.is_empty() {
                    return Ok(latest); // otherwise the version info will be updated
                } else {
                    let agg = Arc::make_mut(&mut latest);

                    // Using a lock on the hashmap here to ensure that all updates happen sequentially.
                    // It would be better to get a lock only for this specific aggregate. So it may be
                    // worth rethinking the structure.
                    //
                    // That said.. saving and applying events is really quick, so this should not hurt
                    // performance much.
                    //
                    // Also note that we don't need the lock to update the inner arc in the cache. We
                    // just need it to be in scope until we are done updating.
                    let mut cache = self.cache.write().unwrap();

                    // It should be impossible to get events for the wrong aggregate, and the wrong
                    // versions, because we are doing the update here inside the outer lock, and aggregates
                    // generally do not lie about who do they are.
                    //
                    // Still.. some defensive coding in case we do have some issue. Double check that the
                    // events are for this aggregate, and are a contiguous sequence of version starting with
                    // this version.
                    let version_before = agg.version();
                    let nr_events = events.len() as u64;

                    info.last_event += nr_events;

                    for i in 0..nr_events {
                        let event = &events[i as usize];
                        if event.version() != version_before + i || event.handle() != &handle {
                            return Err(A::Error::from(AggregateStoreError::WrongEventForAggregate));
                        }
                    }

                    // Time to start saving things.
                    let stored_command = stored_command_builder.finish_with_events(events.as_slice());

                    // If persistence fails, then complain loudly, and exit. Krill should not keep running, because this would
                    // result in discrepancies between state in memory and state on disk. Let Krill crash and an operator investigate.
                    // See issue: https://github.com/NLnetLabs/krill/issues/322
                    if let Err(e) = self.store_command(stored_command) {
                        error!("Cannot save state for '{}'. Got error: {}", handle, e);
                        error!("Will now exit Krill - please verify that the disk can be written to and is not full");
                        std::process::exit(1);
                    }

                    for event in &events {
                        self.store_event(event)?;

                        agg.apply(event.clone());
                        if agg.version() % SNAPSHOT_FREQ == 0 {
                            info.snapshot_version = agg.version();

                            self.store_snapshot(&handle, agg)?;
                        }
                    }

                    cache.insert(handle.clone(), Arc::new(agg.clone()));

                    // Only send this to listeners after everything has been saved.
                    for event in events {
                        for listener in &self.listeners {
                            listener.as_ref().listen(agg, &event);
                        }
                    }

                    Ok(latest)
                }
            }
        };

        self.save_info(&handle, &info)?;

        res
    }

    /// Returns true if an instance exists for the id
    pub fn has(&self, id: &Handle) -> Result<bool, AggregateStoreError> {
        let _lock = self.outer_lock.read().unwrap();
        self.kv
            .has_scope(id.to_string())
            .map_err(AggregateStoreError::KeyStoreError)
    }

    /// Lists all known ids.
    pub fn list(&self) -> Result<Vec<Handle>, AggregateStoreError> {
        let _lock = self.outer_lock.read().unwrap();
        self.aggregates()
    }
}

/// # Manage Commands
///
impl<A: Aggregate> AggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    /// Find all commands that fit the criteria and return history
    pub fn command_history(
        &self,
        id: &Handle,
        crit: CommandHistoryCriteria,
    ) -> Result<CommandHistory, AggregateStoreError> {
        let offset = crit.offset();
        let rows = crit.rows();

        let mut commands: Vec<CommandHistoryRecord> = Vec::with_capacity(rows);
        let mut skipped = 0;
        let mut total = 0;

        for command_key in self.command_keys_ascending(id, &crit)? {
            total += 1;
            if skipped < offset {
                skipped += 1;
            } else if commands.len() < rows {
                let key = Self::key_for_command(id, &command_key);
                let stored: StoredCommand<A::StorableCommandDetails> = self
                    .kv
                    .get(&key)?
                    .ok_or_else(|| AggregateStoreError::CommandNotFound(id.clone(), command_key))?;

                let stored = stored.into();
                commands.push(stored);
            }
        }
        Ok(CommandHistory::new(offset, total, commands))
    }

    /// Archive old commands if they are:
    /// - older than the backup snapshot
    /// - AND older then the threshold days
    /// - AND they are eligible for archiving
    pub fn archive_old_commands(&self, handle: &Handle, days: i64) -> StoreResult<()> {
        let mut crit = CommandHistoryCriteria::default();
        let before = (Time::now() - Duration::days(days)).timestamp();
        crit.set_before(before);
        crit.set_includes(&["cmd-ca-publish", "pubd-publish"]);

        let info = self
            .get_info(handle)
            .map_err(|e| AggregateStoreError::CouldNotArchive(handle.clone(), e.to_string()))?;

        let archivable = self.command_history(handle, crit)?;

        let commands = archivable.commands();

        for command in commands {
            let key = command
                .command_key()
                .map_err(|e| AggregateStoreError::CouldNotArchive(handle.clone(), e.to_string()))?;

            if command.resulting_version() < info.snapshot_version {
                info!("Archiving command {} for {}", command.key, handle);

                self.archive_command(handle, &key)
                    .map_err(|e| AggregateStoreError::CouldNotArchive(handle.clone(), e.to_string()))?;

                if let Some(evt_versions) = command.effect.events() {
                    for version in evt_versions {
                        info!("Archiving event {} for {}", version, handle);
                        self.archive_event(handle, *version)
                            .map_err(|e| AggregateStoreError::CouldNotArchive(handle.clone(), e.to_string()))?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Get the command for this key, if it exists
    pub fn get_command<D: WithStorableDetails>(
        &self,
        id: &Handle,
        command_key: &CommandKey,
    ) -> Result<StoredCommand<D>, AggregateStoreError> {
        let key = Self::key_for_command(id, command_key);
        match self.kv.get(&key) {
            Ok(Some(cmd)) => Ok(cmd),
            Ok(None) => Err(AggregateStoreError::CommandNotFound(id.clone(), command_key.clone())),
            Err(e) => {
                error!(
                    "Found corrupt command at: {}, will try to archive. Error was: {}",
                    key, e
                );
                self.kv.archive_corrupt(&key)?;
                Err(AggregateStoreError::CommandCorrupt(id.clone(), command_key.clone()))
            }
        }
    }

    /// Get the value for this key, if any exists.
    pub fn get_event<V: Event>(&self, id: &Handle, version: u64) -> Result<Option<V>, AggregateStoreError> {
        let key = Self::key_for_event(id, version);
        match self.kv.get(&key) {
            Ok(res_opt) => Ok(res_opt),
            Err(e) => {
                error!(
                    "Found corrupt event for {}, version {}, archiving. Error: {}",
                    id, version, e
                );
                self.kv.archive_corrupt(&key)?;
                Err(AggregateStoreError::EventCorrupt(id.clone(), version))
            }
        }
    }
}

impl<A: Aggregate> AggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    fn has_updates(&self, id: &Handle, aggregate: &A) -> StoreResult<bool> {
        Ok(self.get_event::<A::Event>(id, aggregate.version())?.is_some())
    }

    fn cache_get(&self, id: &Handle) -> Option<Arc<A>> {
        self.cache.read().unwrap().get(id).cloned()
    }

    fn cache_update(&self, id: &Handle, arc: Arc<A>) {
        self.cache.write().unwrap().insert(id.clone(), arc);
    }

    fn get_latest_no_lock(&self, handle: &Handle) -> StoreResult<Arc<A>> {
        trace!("Trying to load aggregate id: {}", handle);
        match self.cache_get(handle) {
            None => match self.get_aggregate(handle, None)? {
                None => {
                    error!("Could not load aggregate with id: {} from disk", handle);
                    Err(AggregateStoreError::UnknownAggregate(handle.clone()))
                }
                Some(agg) => {
                    let arc: Arc<A> = Arc::new(agg);
                    self.cache_update(handle, arc.clone());
                    trace!("Loaded aggregate id: {} from disk", handle);
                    Ok(arc)
                }
            },
            Some(mut arc) => {
                if self.has_updates(handle, &arc)? {
                    let agg = Arc::make_mut(&mut arc);
                    self.update_aggregate(handle, agg, None)?;
                }
                trace!("Loaded aggregate id: {} from memory", handle);
                Ok(arc)
            }
        }
    }
}

/// # Manage values in the KeyValue store
///
impl<A: Aggregate> AggregateStore<A>
where
    A::Error: From<AggregateStoreError>,
{
    fn key_version() -> KeyStoreKey {
        KeyStoreKey::simple("version".to_string())
    }

    fn key_for_info(agg: &Handle) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), "info.json".to_string())
    }

    fn key_for_snapshot(agg: &Handle) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), "snapshot.json".to_string())
    }

    fn key_for_backup_snapshot(agg: &Handle) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), "snapshot-bk.json".to_string())
    }

    fn key_for_new_snapshot(agg: &Handle) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), "snapshot-new.json".to_string())
    }

    fn key_for_event(agg: &Handle, version: u64) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), format!("delta-{}.json", version))
    }

    fn key_for_command(agg: &Handle, command: &CommandKey) -> KeyStoreKey {
        KeyStoreKey::scoped(agg.to_string(), format!("{}.json", command))
    }

    pub fn get_version(&self) -> Result<KeyStoreVersion, AggregateStoreError> {
        match self.kv.get::<KeyStoreVersion>(&Self::key_version())? {
            Some(version) => Ok(version),
            None => Ok(KeyStoreVersion::Pre0_6),
        }
    }

    pub fn set_version(&self, version: &KeyStoreVersion) -> Result<(), AggregateStoreError> {
        self.kv.store(&Self::key_version(), version)?;
        Ok(())
    }

    fn command_keys_ascending(
        &self,
        id: &Handle,
        crit: &CommandHistoryCriteria,
    ) -> Result<Vec<CommandKey>, AggregateStoreError> {
        let mut command_keys = vec![];

        for key in self.kv.keys(Some(id.to_string()), "command--")? {
            match CommandKey::from_str(key.name()) {
                Ok(command_key) => {
                    if command_key.matches_crit(crit) {
                        command_keys.push(command_key);
                    }
                }
                Err(_) => {
                    warn!("Found strange command-like key in disk key-value store: {}", key.name());
                }
            }
        }

        command_keys.sort_by(|a, b| a.sequence.cmp(&b.sequence));

        Ok(command_keys)
    }

    /// Private, should be called through `list` which takes care of locking.
    fn aggregates(&self) -> Result<Vec<Handle>, AggregateStoreError> {
        let mut res = vec![];

        for scope in self.kv.scopes()? {
            if let Ok(handle) = Handle::from_str(&scope) {
                res.push(handle)
            }
        }

        Ok(res)
    }

    /// Clean surplus events
    fn archive_surplus_events(&self, id: &Handle, from: u64) -> Result<(), AggregateStoreError> {
        for key in self.kv.keys(Some(id.to_string()), "delta-")? {
            let name = key.name();
            if name.starts_with("delta-") && name.ends_with(".json") {
                let start = 6;
                let end = name.len() - 5;
                if end > start {
                    if let Ok(v) = u64::from_str(&name[start..end]) {
                        if v >= from {
                            let key = Self::key_for_event(id, v);
                            self.kv
                                .archive_surplus(&key)
                                .map_err(AggregateStoreError::KeyStoreError)?
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Archive an event
    fn archive_event(&self, id: &Handle, version: u64) -> Result<(), AggregateStoreError> {
        let key = Self::key_for_event(id, version);
        self.kv.archive(&key).map_err(AggregateStoreError::KeyStoreError)
    }

    /// Archive a command
    fn archive_command(&self, id: &Handle, command: &CommandKey) -> Result<(), AggregateStoreError> {
        let key = Self::key_for_command(id, command);
        self.kv.archive(&key).map_err(AggregateStoreError::KeyStoreError)
    }

    /// Archive a surplus value for a key
    fn archive_surplus_command(&self, id: &Handle, key: &CommandKey) -> Result<(), AggregateStoreError> {
        let key = Self::key_for_command(id, key);
        self.kv
            .archive_surplus(&key)
            .map_err(AggregateStoreError::KeyStoreError)
    }

    /// MUST check if the event already exists and return an error if it does.
    fn store_event<V: Event>(&self, event: &V) -> Result<(), AggregateStoreError> {
        let id = event.handle();
        let version = event.version();
        let key = Self::key_for_event(id, version);
        self.kv.store_new(&key, event)?;
        Ok(())
    }

    fn store_command<S: WithStorableDetails>(&self, command: StoredCommand<S>) -> Result<(), AggregateStoreError> {
        let id = command.handle();

        let command_key = CommandKey::for_stored(&command);
        let key = Self::key_for_command(id, &command_key);

        self.kv.store_new(&key, &command)?;
        Ok(())
    }

    /// Get the latest aggregate
    fn get_aggregate(&self, id: &Handle, limit: Option<u64>) -> Result<Option<A>, AggregateStoreError> {
        // 1) Try to get a snapshot.
        // 2) If that fails try the backup
        // 3) If that fails, try to get the init event.
        //
        // Then replay all newer events that can be found up to the version (or latest if version is None)
        trace!("Getting aggregate for '{}'", id);

        let mut aggregate_opt: Option<A> = None;

        let snapshot_key = Self::key_for_snapshot(id);

        match self.kv.get::<A>(&snapshot_key) {
            Err(e) => {
                // snapshot file was present and corrupt
                error!(
                    "Could not parse snapshot for '{}', archiving as corrupt. Error was: {}",
                    id, e
                );
                self.kv.archive_corrupt(&snapshot_key)?;
            }
            Ok(Some(agg)) => {
                // snapshot present and okay
                trace!("Found snapshot for '{}'", id);
                if let Some(limit) = limit {
                    if limit >= agg.version() {
                        aggregate_opt = Some(agg)
                    } else {
                        trace!("Discarding snapshot after limit '{}'", id);
                        self.kv.archive_surplus(&snapshot_key)?;
                    }
                } else {
                    debug!("Found valid snapshot for '{}'", id);
                    aggregate_opt = Some(agg)
                }
            }
            Ok(None) => {}
        }

        if aggregate_opt.is_none() {
            warn!("No snapshot found for '{}' will try backup snapshot", id);
            let backup_snapshot_key = Self::key_for_backup_snapshot(id);
            match self.kv.get::<A>(&backup_snapshot_key) {
                Err(e) => {
                    // backup snapshot present and corrupt
                    error!(
                        "Could not parse backup snapshot for '{}', archiving as corrupt. Error: {}",
                        id, e
                    );
                    self.kv.archive_corrupt(&backup_snapshot_key)?;
                }
                Ok(Some(agg)) => {
                    trace!("Found backup snapshot for '{}'", id);
                    if let Some(limit) = limit {
                        if limit >= agg.version() {
                            aggregate_opt = Some(agg)
                        } else {
                            trace!("Discarding backup snapshot after limit '{}'", id);
                            self.kv.archive_surplus(&backup_snapshot_key)?;
                        }
                    } else {
                        debug!("Found valid backup snapshot for '{}'", id);
                        aggregate_opt = Some(agg)
                    }
                }
                Ok(None) => {}
            }
        }

        if aggregate_opt.is_none() {
            warn!("No snapshots found for '{}' will try from initialisation event.", id);
            let init_key = Self::key_for_event(id, 0);
            aggregate_opt = match self.kv.get::<A::InitEvent>(&init_key)? {
                Some(e) => {
                    trace!("Rebuilding aggregate {} from init event", id);
                    Some(A::init(e).map_err(|_| AggregateStoreError::InitError(id.clone()))?)
                }
                None => None,
            }
        }

        match aggregate_opt {
            None => Ok(None),
            Some(mut aggregate) => {
                self.update_aggregate(id, &mut aggregate, limit)?;
                Ok(Some(aggregate))
            }
        }
    }

    fn update_aggregate(&self, id: &Handle, aggregate: &mut A, limit: Option<u64>) -> Result<(), AggregateStoreError> {
        let limit = if let Some(limit) = limit {
            limit
        } else if let Ok(info) = self.get_info(id) {
            info.last_event
        } else {
            let nr_events = self.kv.keys(Some(id.to_string()), "delta-")?.len();
            if nr_events < 1 {
                return Err(AggregateStoreError::InfoMissing(id.clone()));
            } else {
                (nr_events - 1) as u64
            }
        };

        if limit == aggregate.version() - 1 {
            // already at version, done
            // note that an event has the version of the aggregate it *affects*. So delta 10 results in version 11.
            return Ok(());
        }

        let start = aggregate.version();
        if start > limit {
            return Err(AggregateStoreError::ReplayError(id.clone(), limit, start));
        }

        for version in start..limit + 1 {
            if let Some(e) = self.get_event(id, version)? {
                if aggregate.version() != version {
                    error!("Trying to apply event to wrong version of aggregate in replay");
                    return Err(AggregateStoreError::ReplayError(id.clone(), limit, version));
                }
                aggregate.apply(e);
                trace!("Applied event nr {} to aggregate {}", version, id);
            } else {
                return Err(AggregateStoreError::ReplayError(id.clone(), limit, version));
            }
        }

        Ok(())
    }

    /// Saves the latest snapshot - overwrites any previous snapshot.
    fn store_snapshot<V: Aggregate>(&self, id: &Handle, aggregate: &V) -> Result<(), AggregateStoreError> {
        let snapshot_new = Self::key_for_new_snapshot(id);
        let snapshot_current = Self::key_for_snapshot(id);
        let snapshot_backup = Self::key_for_backup_snapshot(id);

        self.kv.store(&snapshot_new, aggregate)?;

        if self.kv.has(&snapshot_backup)? {
            self.kv.drop(&snapshot_backup)?;
        }
        if self.kv.has(&snapshot_current)? {
            self.kv.move_key(&snapshot_current, &snapshot_backup)?;
        }
        self.kv.move_key(&snapshot_new, &snapshot_current)?;

        Ok(())
    }

    fn get_info(&self, id: &Handle) -> Result<StoredValueInfo, AggregateStoreError> {
        let key = Self::key_for_info(id);
        let info = self
            .kv
            .get(&key)
            .map_err(|_| AggregateStoreError::InfoCorrupt(id.clone()))?;
        info.ok_or_else(|| AggregateStoreError::InfoMissing(id.clone()))
    }

    fn save_info(&self, id: &Handle, info: &StoredValueInfo) -> Result<(), AggregateStoreError> {
        let key = Self::key_for_info(id);
        self.kv.store(&key, info).map_err(AggregateStoreError::KeyStoreError)
    }
}

//------------ AggregateStoreError -------------------------------------------

/// This type defines possible Errors for the AggregateStore
#[derive(Debug, Display)]
pub enum AggregateStoreError {
    #[display(fmt = "{}", _0)]
    IoError(io::Error),

    #[display(fmt = "KeyStore Error: {}", _0)]
    KeyStoreError(KeyValueError),

    #[display(fmt = "This aggregate store is not initialised")]
    NotInitialised,

    #[display(fmt = "unknown entity: {}", _0)]
    UnknownAggregate(Handle),

    #[display(fmt = "Init event exists for '{}', but cannot be applied", _0)]
    InitError(Handle),

    #[display(fmt = "Cannot reconstruct '{}' to version '{}', failed at version {}", _0, _1, _2)]
    ReplayError(Handle, u64, u64),

    #[display(fmt = "Missing stored value info for '{}'", _0)]
    InfoMissing(Handle),

    #[display(fmt = "Corrupt stored value info for '{}'", _0)]
    InfoCorrupt(Handle),

    #[display(fmt = "event not applicable to entity, id or version is off")]
    WrongEventForAggregate,

    #[display(fmt = "concurrent modification attempt for entity: '{}'", _0)]
    ConcurrentModification(Handle),

    #[display(fmt = "Aggregate '{}' does not have command with sequence '{}'", _0, _1)]
    UnknownCommand(Handle, u64),

    #[display(fmt = "Offset '{}' exceeds total '{}'", _0, _1)]
    CommandOffsetTooLarge(u64, u64),

    #[display(fmt = "Could not rebuild state for '{}': {}", _0, _1)]
    WarmupFailed(Handle, String),

    #[display(fmt = "Could not recover state for '{}', aborting recover. Use backup!!", _0)]
    CouldNotRecover(Handle),

    #[display(fmt = "Could not archive commands and events for '{}'. Error: {}", _0, _1)]
    CouldNotArchive(Handle, String),

    #[display(fmt = "StoredCommand '{}' for '{}' was corrupt", _1, _0)]
    CommandCorrupt(Handle, CommandKey),

    #[display(fmt = "StoredCommand '{}' for '{}' cannot be found", _1, _0)]
    CommandNotFound(Handle, CommandKey),

    #[display(fmt = "Stored event '{}' for '{}' was corrupt", _1, _0)]
    EventCorrupt(Handle, u64),
}

impl From<KeyValueError> for AggregateStoreError {
    fn from(e: KeyValueError) -> Self {
        AggregateStoreError::KeyStoreError(e)
    }
}
