//! Rust controller for MetadataTypeManager's live JavaScript value boundary.
//!
//! Unlike `metadata_types`, this module never flattens widgets into JSON data.
//! The assigned/properties containers and info records live in an owning realm.
//! A runtime adapter must supply actual object operations and owned root leases.
//! The opt-in script_metadata_types service supplies the normal installed
//! receiver/emitter/async boundary. Full lifecycle and arbitrary receiver
//! overrides remain required independently of this controller.
use crate::json_units::{units, JsonUnits};
use crate::metadata_types::lower_units;
use mnemosyne_core::collation::UnicodeCaseError;
use mnemosyne_runtime_protocol::{HostValue, JsReference};
use std::sync::{Arc, Mutex, MutexGuard};

/// Normal filesystem persistence for one vault's type-config path. Submissions
/// are FIFO, initiated during a synchronous prefix; only bytes/time enter the
/// worker. No JS root, controller borrow, VM or listener runs on its I/O thread.
pub struct WidgetFileWriter {
    submitted: Option<std::sync::mpsc::Sender<WidgetWriteRequest>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
struct WidgetWriteRequest {
    bytes: Vec<u8>,
    when: f64,
    completion: std::sync::mpsc::Sender<std::io::Result<()>>,
    ready: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for WidgetWriteRequest {
    fn drop(&mut self) {
        // Also wake the owning pump if an unexpected worker unwind disconnects
        // this ticket instead of producing an I/O result.
        self.ready.store(true, std::sync::atomic::Ordering::Release);
    }
}
pub struct WidgetWriteTicket {
    completion: std::sync::mpsc::Receiver<std::io::Result<()>>,
    ready: Arc<std::sync::atomic::AtomicBool>,
}
impl WidgetWriteTicket {
    /// Readiness only; a service need not schedule busy VM turns while I/O waits.
    pub fn is_ready(&self) -> bool {
        self.ready.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Poll from the owning event loop. Empty does not mean successful save.
    pub fn try_complete(&self) -> std::io::Result<Option<std::io::Result<()>>> {
        match self.completion.try_recv() {
            Ok(result) => Ok(Some(result)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "Type-config writer disconnected",
            )),
        }
    }
}
impl WidgetFileWriter {
    pub fn new(path: std::path::PathBuf) -> std::io::Result<Self> {
        let (submitted, pending) = std::sync::mpsc::channel::<WidgetWriteRequest>();
        let worker = std::thread::Builder::new()
            .name("type-config-writer".into())
            .spawn(move || {
                use std::io::Write;
                for request in pending {
                    let result = (|| {
                        let mut file = std::fs::File::create(&path)?;
                        file.write_all(&request.bytes)?;
                        // Native writeConfigJson passes the sampled millisecond
                        // mtime to the adapter. Keep that snapshot's time, not a
                        // fresh worker completion time or a later nested save time.
                        let seconds = request.when / 1000.0;
                        if !seconds.is_finite() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "Non-finite type-config modification time",
                            ));
                        }
                        let duration = std::time::Duration::try_from_secs_f64(seconds.abs())
                            .map_err(|error| {
                                std::io::Error::new(std::io::ErrorKind::InvalidInput, error)
                            })?;
                        let epoch = std::time::UNIX_EPOCH;
                        let modified = if seconds >= 0.0 {
                            epoch.checked_add(duration)
                        } else {
                            epoch.checked_sub(duration)
                        }
                        .ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "Type-config modification time is out of range",
                            )
                        })?;
                        file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
                        Ok(())
                    })();
                    let _ = request.completion.send(result);
                    request
                        .ready
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            })?;
        Ok(Self {
            submitted: Some(submitted),
            worker: Some(worker),
        })
    }
    pub fn submit(&self, text: &[u16], when: f64) -> std::io::Result<WidgetWriteTicket> {
        let (completion, result) = std::sync::mpsc::channel();
        let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.submitted
            .as_ref()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Type-config writer is closed",
                )
            })?
            .send(WidgetWriteRequest {
                // Node UTF8 string writes replace unpaired UTF16 units. Standard
                // JSON.stringify escapes them; this also preserves the I/O boundary.
                bytes: String::from_utf16_lossy(text).into_bytes(),
                when,
                completion,
                ready: ready.clone(),
            })
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "Type-config writer disconnected",
                )
            })?;
        Ok(WidgetWriteTicket {
            completion: result,
            ready,
        })
    }
    /// Drain initiated writes and join the owned worker; no orphan service.
    pub fn close(&mut self) -> std::io::Result<()> {
        self.submitted.take();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| std::io::Error::other("Type-config writer thread failed"))?;
        }
        Ok(())
    }
}
impl Drop for WidgetFileWriter {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Clones share ownership of one retained runtime root. A DTO containing only a
/// JsValueId cannot implement the ownership contract of this trait.
pub trait WidgetRoot: Clone {
    fn reference(&self) -> JsReference;
}

/// Primitive values have normal JS value semantics; references hold a lease.
/// Authored Record/Array data must be materialized once by the owning realm.
#[derive(Clone, Debug)]
pub enum WidgetValue<L> {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    Utf16(Vec<u16>),
    BigInt { negative: bool, words: Vec<u64> },
    Reference(L),
}
impl<L: WidgetRoot> WidgetValue<L> {
    pub fn host_value(&self) -> HostValue {
        match self {
            Self::Undefined => HostValue::Undefined,
            Self::Null => HostValue::Null,
            Self::Boolean(value) => HostValue::Boolean(*value),
            Self::Number(value) => HostValue::Number(*value),
            Self::Utf16(value) => HostValue::Utf16(value.clone()),
            Self::BigInt { negative, words } => HostValue::BigInt {
                negative: *negative,
                words: words.clone(),
            },
            Self::Reference(root) => HostValue::Reference(root.reference()),
        }
    }
    fn nullish(&self) -> bool {
        matches!(self, Self::Undefined | Self::Null)
    }
    fn truthy(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Boolean(value) => *value,
            Self::Number(value) => *value != 0.0 && !value.is_nan(),
            Self::Utf16(value) => !value.is_empty(),
            Self::BigInt { words, .. } => words.iter().any(|word| *word != 0),
            Self::Reference(_) => true,
        }
    }
}

/// Every returned reference, including a thrown exception, must own its root.
/// Adapter errors must retain original JS throws without coercing their values.
/// Object operations run in this realm and preserve getters/prototypes/order.
pub trait WidgetRealm {
    type Root: WidgetRoot;
    type Error: std::error::Error;

    fn context(&self) -> u64;
    /// Only real JS exceptions are caught at JSON.stringify's native boundary.
    /// Transport/closed-realm failures are not fake successful persistence.
    fn is_javascript_exception(error: &Self::Error) -> bool;
    fn validate_root(&mut self, root: &Self::Root) -> Result<(), Self::Error>;
    /// Promote a borrowed callback value or materialize authored data once.
    fn retain(&mut self, value: HostValue) -> Result<WidgetValue<Self::Root>, Self::Error>;
    fn ordinary_object(&mut self) -> Result<Self::Root, Self::Error>;
    fn get(
        &mut self,
        receiver: &WidgetValue<Self::Root>,
        key: &[u16],
    ) -> Result<WidgetValue<Self::Root>, Self::Error>;
    /// Ordinary strict assignment, including inherited setters. Do not replace
    /// this with DefineProperty: `__proto__` has different source behavior.
    fn set(
        &mut self,
        receiver: &WidgetValue<Self::Root>,
        key: &[u16],
        value: &WidgetValue<Self::Root>,
    ) -> Result<(), Self::Error>;
    /// Strict computed assignment performs actual ToPropertyKey in the realm.
    /// info.name is mutable and need not remain a string after SDK exposure.
    fn set_computed(
        &mut self,
        receiver: &Self::Root,
        key: &WidgetValue<Self::Root>,
        value: &WidgetValue<Self::Root>,
    ) -> Result<(), Self::Error>;
    /// Own writable/enumerable/configurable data property (object literal).
    fn define_data(
        &mut self,
        object: &Self::Root,
        key: &[u16],
        value: &WidgetValue<Self::Root>,
    ) -> Result<(), Self::Error>;
    fn delete(&mut self, object: &Self::Root, key: &[u16]) -> Result<(), Self::Error>;
    fn own_enumerable_keys(&mut self, object: &Self::Root) -> Result<Vec<Vec<u16>>, Self::Error>;
    /// Actual language for-in iterator, retaining the enumerated object and
    /// checking deletion/prototype effects as iteration resumes after callbacks.
    /// A snapshot Object.keys plus another hasOwn call is observably different.
    fn begin_for_in(&mut self, object: &Self::Root) -> Result<Self::Root, Self::Error>;
    fn next_for_in(&mut self, iterator: &Self::Root) -> Result<Option<Vec<u16>>, Self::Error>;
    /// The static Object.hasOwn used by rebuild/getters.
    fn has_own(&mut self, object: &Self::Root, key: &[u16]) -> Result<bool, Self::Error>;
    /// Dynamic `object.hasOwnProperty(key)` used by save/loadData; may throw if
    /// a caller has shadowed that method. It is not interchangeable with has_own.
    fn invoke_has_own(&mut self, object: &Self::Root, key: &[u16]) -> Result<bool, Self::Error>;
    /// The owning realm's JSON.stringify(value, undefined, 2). Run actual JS
    /// getters/toJSON, omissions, BigInt/cycle failure, and constructor behavior.
    fn stringify(&mut self, object: &Self::Root) -> Result<Vec<u16>, Self::Error>;
}

#[derive(Debug, thiserror::Error)]
pub enum LiveMetadataError<E: std::error::Error> {
    #[error("{0}")]
    Casing(#[from] UnicodeCaseError),
    #[error("{0}")]
    Realm(E),
    #[error("Widget belongs to context {actual}, expected {expected}")]
    WrongRealm { actual: u64, expected: u64 },
    #[error("Runtime materialization did not create a reference")]
    InvalidMaterialization,
}

/// Both failures resolve at the native writeJson boundary. They remain explicit
/// evidence for diagnostics/tests; neither prevents setType's changed event.
#[derive(Debug)]
pub enum WidgetSaveOutcome<E> {
    Written,
    StringifyFailed(E),
    WriteFailed(E),
}

/// The synchronous save prefix has already read fields, sampled Date.now and
/// attempted real JSON.stringify. File persistence/event completion is separate.
/// Retain the source snapshot until completion; nested saves cannot rebuild it.
pub struct PendingWidgetSave<L, E> {
    snapshot: L,
    when: f64,
    serialization: Result<Vec<u16>, E>,
}
impl<L, E> PendingWidgetSave<L, E> {
    pub fn text(&self) -> Option<&[u16]> {
        self.serialization.as_deref().ok()
    }
    pub fn when(&self) -> f64 {
        self.when
    }
    pub fn snapshot(&self) -> &L {
        &self.snapshot
    }
    /// A skipped stringify still awaits the resolved writeJson/save promise in
    /// the native method. It must not deliver changed during the sync prefix.
    pub fn finish(self, write: Result<(), E>) -> WidgetSaveOutcome<E> {
        match self.serialization {
            Err(error) => WidgetSaveOutcome::StringifyFailed(error),
            Ok(_) => match write {
                Ok(()) => WidgetSaveOutcome::Written,
                Err(error) => WidgetSaveOutcome::WriteFailed(error),
            },
        }
    }
}
pub struct PendingTypeChange<L, E> {
    key: Vec<u16>,
    save: PendingWidgetSave<L, E>,
}
impl<L, E> PendingTypeChange<L, E> {
    pub fn key(&self) -> &[u16] {
        &self.key
    }
    /// Consume only the save continuation. An installed emitter delivers its
    /// registered rebuild listener through the actual public event container.
    pub fn finish_save(self, write: Result<(), E>) -> (Vec<u16>, WidgetSaveOutcome<E>) {
        (self.key, self.save.finish(write))
    }
    pub fn save(&self) -> &PendingWidgetSave<L, E> {
        &self.save
    }
}

#[derive(Clone, Debug)]
pub struct InferredProperty {
    pub name: Vec<u16>,
    pub nonnull: bool,
    pub widget: &'static str,
}

/// A controller attached to exactly one retained vault realm. Container getters
/// clone root leases, never recreate an object. Caller-held old containers stay
/// alive after rebuild/unset; the manager releases only its own root ownership.
struct LiveState<L> {
    assigned: L,
    properties: L,
    last_save: f64,
    receiver: Option<L>,
}
/// Cloned handles share state. No state guard is held across a VM operation;
/// a recursive host callback can enter another clone without a mutable borrow
/// of the outer controller or an engine-wide lock.
#[derive(Clone)]
pub struct LiveMetadataTypes<L: WidgetRoot> {
    context: u64,
    state: Arc<Mutex<LiveState<L>>>,
}
impl<L: WidgetRoot> LiveMetadataTypes<L> {
    fn state(&self) -> MutexGuard<'_, LiveState<L>> {
        // No user code/VM operation runs under this guard. A recovered state
        // remains structurally valid even if caller-side panic poisoned a lock.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    /// Empty constructor state, before loadData/cache-finished processing.
    /// This does not mark a plugin/backend ready or emit manager events.
    pub fn new<R: WidgetRealm<Root = L>>(
        realm: &mut R,
    ) -> Result<Self, LiveMetadataError<R::Error>> {
        let assigned = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        let properties = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        Ok(Self {
            context: realm.context(),
            state: Arc::new(Mutex::new(LiveState {
                assigned,
                properties,
                last_save: 0.0,
                receiver: None,
            })),
        })
    }
    /// Explicit import of a completed canonical assigned state. Each widget is
    /// materialized once and then shared. This is not loadData's hot event loop:
    /// caller must separately deliver the actual rebuild/cache lifecycle.
    pub fn import_canonical_assignments<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        entries: impl IntoIterator<Item = crate::metadata_types::PropertyInfoUnits>,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        self.check_context(realm, self.context)?;
        let assigned = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        for entry in entries {
            let key = lower_units(&entry.name)?;
            let widget = self.materialize_widget(realm, &entry.widget)?;
            let info = self.info_object(realm, WidgetValue::Utf16(entry.name), widget)?;
            self.set_root_field(realm, &assigned, &key, &Self::root_value(&info))?;
        }
        self.commit_container(realm, true, assigned)?;
        Ok(())
    }
    /// Bind ordinary public manager fields. Each source statement reads the
    /// actual receiver again; a getter or nested call may replace its container.
    pub fn bind_receiver<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        receiver: L,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(&receiver))?;
        self.state().receiver = Some(receiver);
        Ok(())
    }
    /// A method's actual object receiver, including borrowed getter calls.
    /// The temporary handle has no VM-global receiver slot: nested calls can
    /// independently use another receiver without altering the outer handle.
    pub fn for_receiver<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        receiver: L,
    ) -> Result<Self, LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(&receiver))?;
        let state = self.state();
        Ok(Self {
            context: self.context,
            state: Arc::new(Mutex::new(LiveState {
                assigned: state.assigned.clone(),
                properties: state.properties.clone(),
                last_save: state.last_save,
                receiver: Some(receiver),
            })),
        })
    }
    fn container<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        assigned: bool,
    ) -> Result<L, LiveMetadataError<R::Error>> {
        let receiver = self.state().receiver.clone();
        if let Some(receiver) = receiver {
            match realm
                .get(
                    &Self::root_value(&receiver),
                    &units(if assigned {
                        "assignedWidgets"
                    } else {
                        "properties"
                    }),
                )
                .map_err(LiveMetadataError::Realm)?
            {
                WidgetValue::Reference(root) => Ok(root),
                _ => Err(LiveMetadataError::InvalidMaterialization),
            }
        } else {
            Ok(if assigned {
                self.assigned_container()
            } else {
                self.properties_container()
            })
        }
    }
    fn commit_container<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        assigned: bool,
        root: L,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        let receiver = self.state().receiver.clone();
        if let Some(receiver) = receiver {
            realm
                .set(
                    &Self::root_value(&receiver),
                    &units(if assigned {
                        "assignedWidgets"
                    } else {
                        "properties"
                    }),
                    &Self::root_value(&root),
                )
                .map_err(LiveMetadataError::Realm)?;
        }
        if assigned {
            self.state().assigned = root;
        } else {
            self.state().properties = root;
        }
        Ok(())
    }
    pub fn assigned_container(&self) -> L {
        self.state().assigned.clone()
    }
    pub fn properties_container(&self) -> L {
        self.state().properties.clone()
    }
    /// Internal hook for a source-observed container replacement. An installed
    /// SDK adapter must read the actual receiver's mutable fields rather than
    /// presenting this hook as a substitute for ordinary JS data descriptors.
    pub fn replace_assigned_container<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        assigned: L,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(&assigned))?;
        self.commit_container(realm, true, assigned)?;
        Ok(())
    }
    pub fn replace_properties_container<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        properties: L,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(&properties))?;
        self.state().properties = properties;
        Ok(())
    }
    pub fn last_save(&self) -> f64 {
        self.state().last_save
    }

    /// Promote first, before the caller's borrowed callback root is released.
    pub fn retain_widget<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        value: HostValue,
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        self.check_context(realm, self.context)?;
        if let HostValue::Reference(reference) = &value {
            self.check_context(realm, reference.id.context)?;
        }
        let value = realm.retain(value).map_err(LiveMetadataError::Realm)?;
        self.validate_value(realm, &value)?;
        Ok(value)
    }
    /// Loaded canonical config references become one rooted realm object. The
    /// owning adapter materializes its children once; getters reuse that root.
    pub fn materialize_widget<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        data: &JsonUnits,
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        self.check_context(realm, self.context)?;
        // Keep the transport flat: recursive HostValue::Record/Array conversion
        // would merely move deep JSON recursion into serialization/Clone/Drop.
        enum Work<'a> {
            Value(&'a JsonUnits),
            Array(usize),
            Object(Vec<Vec<u16>>),
        }
        let mut work = vec![Work::Value(data)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Value(value) => match value {
                    JsonUnits::Null => values.push(WidgetValue::Null),
                    JsonUnits::Bool(value) => values.push(WidgetValue::Boolean(*value)),
                    JsonUnits::Number(value) => values.push(WidgetValue::Number(*value)),
                    JsonUnits::String(value) => values.push(WidgetValue::Utf16(value.clone())),
                    JsonUnits::Array(children) => {
                        work.push(Work::Array(children.len()));
                        work.extend(children.iter().rev().map(Work::Value));
                    }
                    JsonUnits::Object(fields) => {
                        work.push(Work::Object(
                            fields.iter().map(|(key, _)| key.clone()).collect(),
                        ));
                        work.extend(fields.iter().rev().map(|(_, value)| Work::Value(value)));
                    }
                },
                Work::Array(count) => {
                    let children = values.split_off(values.len() - count);
                    let value = self.retain_widget(realm, HostValue::Array(Vec::new()))?;
                    let WidgetValue::Reference(array) = value else {
                        return Err(LiveMetadataError::InvalidMaterialization);
                    };
                    for (index, value) in children.into_iter().enumerate() {
                        realm
                            .define_data(&array, &units(&index.to_string()), &value)
                            .map_err(LiveMetadataError::Realm)?;
                    }
                    values.push(WidgetValue::Reference(array));
                }
                Work::Object(keys) => {
                    let children = values.split_off(values.len() - keys.len());
                    let object = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
                    for (key, value) in keys.into_iter().zip(children) {
                        realm
                            .define_data(&object, &key, &value)
                            .map_err(LiveMetadataError::Realm)?;
                    }
                    values.push(WidgetValue::Reference(object));
                }
            }
        }
        Ok(values
            .pop()
            .expect("JsonUnits materialization produces one value"))
    }
    fn check_context<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        context: u64,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        let context = if realm.context() != self.context {
            realm.context()
        } else {
            context
        };
        if context != self.context {
            Err(LiveMetadataError::WrongRealm {
                actual: context,
                expected: self.context,
            })
        } else {
            Ok(())
        }
    }
    fn validate_value<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        value: &WidgetValue<L>,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        if let WidgetValue::Reference(root) = value {
            self.check_context(realm, root.reference().id.context)?;
            realm
                .validate_root(root)
                .map_err(LiveMetadataError::Realm)?;
        }
        Ok(())
    }
    fn root_value(root: &L) -> WidgetValue<L> {
        WidgetValue::Reference(root.clone())
    }
    fn get_root_field<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        root: &L,
        key: &[u16],
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(root))?;
        realm
            .get(&Self::root_value(root), key)
            .map_err(LiveMetadataError::Realm)
    }
    fn set_root_field<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        root: &L,
        key: &[u16],
        value: &WidgetValue<L>,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(root))?;
        self.validate_value(realm, value)?;
        realm
            .set(&Self::root_value(root), key, value)
            .map_err(LiveMetadataError::Realm)
    }
    fn info_object<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        name: WidgetValue<L>,
        widget: WidgetValue<L>,
    ) -> Result<L, LiveMetadataError<R::Error>> {
        self.check_context(realm, self.context)?;
        let info = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        realm
            .define_data(&info, &units("name"), &name)
            .map_err(LiveMetadataError::Realm)?;
        realm
            .define_data(&info, &units("widget"), &widget)
            .map_err(LiveMetadataError::Realm)?;
        Ok(info)
    }
    fn has_own_container<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        assigned: bool,
        key: &[u16],
    ) -> Result<bool, LiveMetadataError<R::Error>> {
        let root = self.container(realm, assigned)?;
        realm.has_own(&root, key).map_err(LiveMetadataError::Realm)
    }
    fn invoke_own_assigned<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        key: &[u16],
    ) -> Result<bool, LiveMetadataError<R::Error>> {
        let root = self.container(realm, true)?;
        realm
            .invoke_has_own(&root, key)
            .map_err(LiveMetadataError::Realm)
    }
    fn get_container_field<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        assigned: bool,
        key: &[u16],
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        let root = self.container(realm, assigned)?;
        self.get_root_field(realm, &root, key)
    }
    pub fn assigned_widget<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        name: &[u16],
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        let key = lower_units(name)?;
        if !self.has_own_container(realm, true, &key)? {
            return Ok(WidgetValue::Null);
        }
        let info = self.get_container_field(realm, true, &key)?;
        realm
            .get(&info, &units("widget"))
            .map_err(LiveMetadataError::Realm)
    }
    pub fn property_info<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        name: &[u16],
    ) -> Result<WidgetValue<L>, LiveMetadataError<R::Error>> {
        let key = lower_units(name)?;
        if self.has_own_container(realm, false, &key)? {
            self.get_container_field(realm, false, &key)
        } else {
            Ok(WidgetValue::Reference(self.info_object(
                realm,
                WidgetValue::Utf16(name.to_vec()),
                WidgetValue::Utf16(units("text")),
            )?))
        }
    }
    /// Save does not emit changed or rebuild. Construct/access failures before
    /// writeJson are propagated; stringify and file-write failures are swallowed.
    pub fn save<R: WidgetRealm<Root = L>, W>(
        &self,
        realm: &mut R,
        now: f64,
        write: W,
    ) -> Result<WidgetSaveOutcome<R::Error>, LiveMetadataError<R::Error>>
    where
        W: FnMut(&[u16], f64) -> Result<(), R::Error>,
    {
        let document = self.prepare_save(realm)?;
        self.save_prepared(realm, &document, now, write)
    }
    /// Assemble the snapshot before sampling Date.now. User-mutated info fields
    /// and computed names may execute getters/conversions here and may throw.
    /// This phase does not update lastSave and does not emit changed.
    pub fn prepare_save<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
    ) -> Result<L, LiveMetadataError<R::Error>> {
        self.check_context(realm, self.context)?;
        let types = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        let enumerated = self.container(realm, true)?;
        let iterator = realm
            .begin_for_in(&enumerated)
            .map_err(LiveMetadataError::Realm)?;
        while let Some(key) = realm
            .next_for_in(&iterator)
            .map_err(LiveMetadataError::Realm)?
        {
            if !self.invoke_own_assigned(realm, &key)? {
                continue;
            }
            let info = self.get_container_field(realm, true, &key)?;
            let name = realm
                .get(&info, &units("name"))
                .map_err(LiveMetadataError::Realm)?;
            let widget = realm
                .get(&info, &units("widget"))
                .map_err(LiveMetadataError::Realm)?;
            realm
                .set_computed(&types, &name, &widget)
                .map_err(LiveMetadataError::Realm)?;
        }
        let document = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        realm
            .define_data(&document, &units("types"), &Self::root_value(&types))
            .map_err(LiveMetadataError::Realm)?;
        Ok(document)
    }
    /// Source save boundary after payload construction and Date.now sampling.
    /// An async service may use prepare_save before yielding and retain its
    /// snapshot until the actual write. This synchronous convenience does not
    /// settle SDK promises or deliver listeners itself.
    pub fn save_prepared<R: WidgetRealm<Root = L>, W>(
        &self,
        realm: &mut R,
        document: &L,
        now: f64,
        mut write: W,
    ) -> Result<WidgetSaveOutcome<R::Error>, LiveMetadataError<R::Error>>
    where
        W: FnMut(&[u16], f64) -> Result<(), R::Error>,
    {
        let pending = self.begin_save_prepared(realm, document.clone(), now)?;
        let result = match pending.text() {
            Some(text) => write(text, now),
            None => Ok(()),
        };
        Ok(pending.finish(result))
    }
    /// No event or promise completion happens here. Initiate real file I/O using
    /// returned text before sending PrefixReady, then finish on the owning thread
    /// after save's await boundary. lastSave precedes stringify, including errors.
    pub fn begin_save_prepared<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        snapshot: L,
        now: f64,
    ) -> Result<PendingWidgetSave<L, R::Error>, LiveMetadataError<R::Error>> {
        self.validate_value(realm, &Self::root_value(&snapshot))?;
        let receiver = self.state().receiver.clone();
        if let Some(receiver) = receiver {
            realm
                .set(
                    &Self::root_value(&receiver),
                    &units("lastSave"),
                    &WidgetValue::Number(now),
                )
                .map_err(LiveMetadataError::Realm)?;
        }
        self.state().last_save = now;
        let serialization = match realm.stringify(&snapshot) {
            Err(error) if R::is_javascript_exception(&error) => Err(error),
            Err(error) => return Err(LiveMetadataError::Realm(error)),
            Ok(text) => Ok(text),
        };
        Ok(PendingWidgetSave {
            snapshot,
            when: now,
            serialization,
        })
    }
    pub fn begin_set_type<R: WidgetRealm<Root = L>, C>(
        &self,
        realm: &mut R,
        name: &[u16],
        widget: WidgetValue<L>,
        clock: C,
    ) -> Result<PendingTypeChange<L, R::Error>, LiveMetadataError<R::Error>>
    where
        C: FnOnce(&mut R) -> Result<f64, R::Error>,
    {
        let key = self.assign_type(realm, name, widget)?;
        let snapshot = self.prepare_save(realm)?;
        let now = clock(realm).map_err(LiveMetadataError::Realm)?;
        let save = self.begin_save_prepared(realm, snapshot, now)?;
        Ok(PendingTypeChange { key, save })
    }
    pub fn begin_unset_type<R: WidgetRealm<Root = L>, C>(
        &self,
        realm: &mut R,
        name: &[u16],
        clock: C,
    ) -> Result<PendingTypeChange<L, R::Error>, LiveMetadataError<R::Error>>
    where
        C: FnOnce(&mut R) -> Result<f64, R::Error>,
    {
        let key = self.remove_assignment(realm, name)?;
        let snapshot = self.prepare_save(realm)?;
        let now = clock(realm).map_err(LiveMetadataError::Realm)?;
        let save = self.begin_save_prepared(realm, snapshot, now)?;
        Ok(PendingTypeChange { key, save })
    }
    /// Owning-thread continuation after actual save completion. The built-in
    /// rebuild listener precedes later registered listeners in the captured app.
    /// Caller supplies actual event delivery; no emitter/promise is fabricated.
    pub fn finish_type_change<R: WidgetRealm<Root = L>, F>(
        &self,
        realm: &mut R,
        pending: PendingTypeChange<L, R::Error>,
        write: Result<(), R::Error>,
        inferred: impl IntoIterator<Item = InferredProperty>,
        mut changed: F,
    ) -> Result<WidgetSaveOutcome<R::Error>, LiveMetadataError<R::Error>>
    where
        F: FnMut(&mut R, &[u16]) -> Result<(), R::Error>,
    {
        let PendingTypeChange { key, save } = pending;
        let outcome = save.finish(write);
        self.rebuild(realm, inferred)?;
        changed(realm, &key).map_err(LiveMetadataError::Realm)?;
        Ok(outcome)
    }
    /// Internal normal-receiver operation. Return the changed key for actual
    /// event integration; a successful result does not claim listener delivery.
    pub fn set_type<R: WidgetRealm<Root = L>, W>(
        &self,
        realm: &mut R,
        name: &[u16],
        widget: WidgetValue<L>,
        now: f64,
        write: W,
        inferred: impl IntoIterator<Item = InferredProperty>,
    ) -> Result<(Vec<u16>, WidgetSaveOutcome<R::Error>), LiveMetadataError<R::Error>>
    where
        W: FnMut(&[u16], f64) -> Result<(), R::Error>,
    {
        let key = self.assign_type(realm, name, widget)?;
        let saved = self.save(realm, now, write)?;
        self.rebuild(realm, inferred)?;
        Ok((key, saved))
    }
    /// Mutation-only phase for an installed async/event adapter. Source order:
    /// lowercase first, mutate the real assigned record, then call save. The
    /// adapter must await save before trigger(changed,key); no event is faked.
    pub fn assign_type<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        name: &[u16],
        widget: WidgetValue<L>,
    ) -> Result<Vec<u16>, LiveMetadataError<R::Error>> {
        let key = lower_units(name)?;
        self.validate_value(realm, &widget)?;
        let old = self.get_container_field(realm, true, &key)?;
        if old.truthy() {
            realm
                .set(&old, &units("widget"), &widget)
                .map_err(LiveMetadataError::Realm)?;
        } else {
            let info = self.info_object(realm, WidgetValue::Utf16(name.to_vec()), widget)?;
            let container = self.container(realm, true)?;
            self.set_root_field(realm, &container, &key, &Self::root_value(&info))?;
        }
        Ok(key)
    }
    pub fn unset_type<R: WidgetRealm<Root = L>, W>(
        &self,
        realm: &mut R,
        name: &[u16],
        now: f64,
        write: W,
        inferred: impl IntoIterator<Item = InferredProperty>,
    ) -> Result<(Vec<u16>, WidgetSaveOutcome<R::Error>), LiveMetadataError<R::Error>>
    where
        W: FnMut(&[u16], f64) -> Result<(), R::Error>,
    {
        let key = self.remove_assignment(realm, name)?;
        let saved = self.save(realm, now, write)?;
        self.rebuild(realm, inferred)?;
        Ok((key, saved))
    }
    pub fn remove_assignment<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        name: &[u16],
    ) -> Result<Vec<u16>, LiveMetadataError<R::Error>> {
        let key = lower_units(name)?;
        let container = self.container(realm, true)?;
        realm
            .delete(&container, &key)
            .map_err(LiveMetadataError::Realm)?;
        Ok(key)
    }
    /// Caller applies eligibility (metadata exists, not external/ignored).
    /// Existing inferred records mutate in place; source spelling follows the
    /// latest nonnull cache field. Removed fields remain until a full rebuild.
    pub fn cache_changed<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        fields: impl IntoIterator<Item = InferredProperty>,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        for field in fields {
            let key = lower_units(&field.name)?;
            if self.has_own_container(realm, true, &key)? {
                continue;
            }
            let info = if self.has_own_container(realm, false, &key)? {
                self.get_container_field(realm, false, &key)?
            } else {
                WidgetValue::Null
            };
            if info.truthy() {
                if field.nonnull {
                    realm
                        .set(&info, &units("name"), &WidgetValue::Utf16(field.name))
                        .map_err(LiveMetadataError::Realm)?;
                    realm
                        .set(
                            &info,
                            &units("widget"),
                            &WidgetValue::Utf16(units(field.widget)),
                        )
                        .map_err(LiveMetadataError::Realm)?;
                }
            } else {
                let widget = if field.nonnull { field.widget } else { "text" };
                let info = self.info_object(
                    realm,
                    WidgetValue::Utf16(field.name),
                    WidgetValue::Utf16(units(widget)),
                )?;
                let container = self.container(realm, false)?;
                self.set_root_field(realm, &container, &key, &Self::root_value(&info))?;
            }
        }
        Ok(())
    }
    /// First nonnull inferred type wins during rebuild. Assigned widget
    /// references are shallow-shared. Commit replaces the properties root only
    /// after successful construction, preserving previous records on failure.
    pub fn rebuild<R: WidgetRealm<Root = L>>(
        &self,
        realm: &mut R,
        inferred: impl IntoIterator<Item = InferredProperty>,
    ) -> Result<(), LiveMetadataError<R::Error>> {
        let temporary = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        let enumerated = self.container(realm, true)?;
        let iterator = realm
            .begin_for_in(&enumerated)
            .map_err(LiveMetadataError::Realm)?;
        while let Some(key) = realm
            .next_for_in(&iterator)
            .map_err(LiveMetadataError::Realm)?
        {
            if !realm
                .has_own(&enumerated, &key)
                .map_err(LiveMetadataError::Realm)?
            {
                continue;
            }
            let assigned = self.get_root_field(realm, &enumerated, &key)?;
            let name = realm
                .get(&assigned, &units("name"))
                .map_err(LiveMetadataError::Realm)?;
            let widget = realm
                .get(&assigned, &units("widget"))
                .map_err(LiveMetadataError::Realm)?;
            let info = self.info_object(realm, name, widget)?;
            self.set_root_field(realm, &temporary, &key, &Self::root_value(&info))?;
        }
        for field in inferred {
            let key = lower_units(&field.name)?;
            if realm
                .has_own(&temporary, &key)
                .map_err(LiveMetadataError::Realm)?
            {
                let info = self.get_root_field(realm, &temporary, &key)?;
                let widget = realm
                    .get(&info, &units("widget"))
                    .map_err(LiveMetadataError::Realm)?;
                if matches!(widget, WidgetValue::Undefined) && field.nonnull {
                    realm
                        .set(
                            &info,
                            &units("widget"),
                            &WidgetValue::Utf16(units(field.widget)),
                        )
                        .map_err(LiveMetadataError::Realm)?;
                }
            } else {
                let widget = if field.nonnull {
                    WidgetValue::Utf16(units(field.widget))
                } else {
                    WidgetValue::Undefined
                };
                let info = self.info_object(realm, WidgetValue::Utf16(field.name), widget)?;
                self.set_root_field(realm, &temporary, &key, &Self::root_value(&info))?;
            }
        }
        let properties = realm.ordinary_object().map_err(LiveMetadataError::Realm)?;
        for key in realm
            .own_enumerable_keys(&temporary)
            .map_err(LiveMetadataError::Realm)?
        {
            let info = self.get_root_field(realm, &temporary, &key)?;
            let name = realm
                .get(&info, &units("name"))
                .map_err(LiveMetadataError::Realm)?;
            let widget = realm
                .get(&info, &units("widget"))
                .map_err(LiveMetadataError::Realm)?;
            let widget = if widget.nullish() {
                WidgetValue::Utf16(units("text"))
            } else {
                widget
            };
            let info = self.info_object(realm, name, widget)?;
            realm
                .define_data(&properties, &key, &Self::root_value(&info))
                .map_err(LiveMetadataError::Realm)?;
        }
        self.commit_container(realm, false, properties)?;
        Ok(())
    }
}
