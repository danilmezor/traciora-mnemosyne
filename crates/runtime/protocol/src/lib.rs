//! Typed messages for the real Rust backend. Arbitrary JavaScript objects are
//! rooted references, never enumerated, coerced, or flattened at this boundary.
use futures_channel::oneshot;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostObjectId(pub u64);
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JsValueId {
    pub context: u64,
    pub index: u64,
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceKind {
    Object,
    Function,
    Symbol,
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsReference {
    pub id: JsValueId,
    pub kind: ReferenceKind,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum HostValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    Utf16(Vec<u16>),
    BigInt {
        negative: bool,
        words: Vec<u64>,
    },
    Reference(JsReference),
    /// A real backend object. The runtime must cache one wrapper per ID and
    /// attach the registered SDK class prototype; a record is not a substitute.
    HostObject {
        id: HostObjectId,
        class: Vec<u16>,
    },
    HostClass {
        key: Vec<u16>,
    },
    HostPrototype {
        key: Vec<u16>,
    },
    /// Newly authored Rust data, not a serialization of arbitrary JS objects.
    Array(Vec<HostValue>),
    Record(Vec<(Vec<u16>, HostValue)>),
}
impl From<&str> for HostValue {
    fn from(value: &str) -> Self {
        Self::Utf16(value.encode_utf16().collect())
    }
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum HostKey {
    Utf16(Vec<u16>),
    Symbol(JsValueId),
}
impl From<&str> for HostKey {
    fn from(value: &str) -> Self {
        Self::Utf16(value.encode_utf16().collect())
    }
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostOperation {
    Get,
    Set,
    Call,
    Construct,
    Subscribe,
    Unsubscribe,
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Sync,
    Promise,
    /// Finish the synchronous Rust prefix, then return undefined; no completion.
    VoidAsync,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HostBinding {
    pub target: HostObjectId,
    pub operation: HostOperation,
    pub member: Option<HostKey>,
    pub delivery: Delivery,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostCall {
    pub binding: HostBinding,
    /// Actual JavaScript receiver, separate from the registered dispatch target.
    pub receiver: HostValue,
    pub new_target: HostValue,
    pub arguments: Vec<HostValue>,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostError {
    pub name: String,
    pub message: String,
    /// Original JS exception, including primitive throws and object identity.
    /// Do not read/coerce it just to report a property-access failure.
    pub thrown: Option<Box<HostValue>>,
}
impl HostError {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
            thrown: None,
        }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self {
            name: "Error".into(),
            message: message.into(),
            thrown: None,
        }
    }
    pub fn thrown(value: HostValue) -> Self {
        Self {
            name: String::new(),
            message: String::new(),
            thrown: Some(Box::new(value)),
        }
    }
}
pub type HostResult = Result<HostValue, HostError>;
pub type HostCompletion = oneshot::Receiver<HostResult>;

/// The receiver can settle a Promise later by retaining completion. A void
/// asynchronous request has no completion; its JS return type stays undefined.
pub struct HostRequest {
    pub call: HostCall,
    pub completion: Option<oneshot::Sender<HostResult>>,
}
impl HostRequest {
    pub fn channel(call: HostCall) -> (Self, Option<HostCompletion>) {
        if call.binding.delivery == Delivery::VoidAsync {
            (
                Self {
                    call,
                    completion: None,
                },
                None,
            )
        } else {
            let (sender, receiver) = oneshot::channel();
            (
                Self {
                    call,
                    completion: Some(sender),
                },
                Some(receiver),
            )
        }
    }
    pub fn respond(self, result: HostResult) {
        if let Some(completion) = self.completion {
            let _ = completion.send(result);
        }
    }
}

/// Explicit future inspection protocol: descriptors preserve symbol keys,
/// accessors and prototype identities. Merely passing an object never invokes
/// a getter. The initial callback bridge retains roots; inspection execution
/// and JS-callback invocation are a separate required layer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ObjectQuery {
    OwnKeys(JsValueId),
    OwnDescriptor(JsValueId, HostKey),
    Prototype(JsValueId),
    Get(JsValueId, HostKey),
    GetValue(HostValue, HostKey),
    BackendIdentity(JsValueId),
    SetValue {
        receiver: HostValue,
        key: HostKey,
        value: HostValue,
    },
    Invoke {
        function: HostValue,
        receiver: HostValue,
        arguments: Vec<HostValue>,
    },
    Binary {
        operation: BinaryOperation,
        left: HostValue,
        right: HostValue,
    },
    InstanceOf {
        value: HostValue,
        constructor: HostValue,
    },
    /// Read the actual owning realm's current global property.
    GlobalValue(HostKey),
    /// Materialize authored data once into the owning realm, then root it.
    Materialize(HostValue),
    Construct {
        function: HostValue,
        arguments: Vec<HostValue>,
    },
    GetComputed {
        receiver: HostValue,
        key: HostValue,
    },
    SetComputed {
        receiver: HostValue,
        key: HostValue,
        value: HostValue,
    },
    Delete {
        receiver: HostValue,
        key: HostValue,
    },
    /// Intrinsic Object.keys inspection; source calls to mutable Object.keys
    /// must instead read/invoke the current global function.
    OwnEnumerableKeys(HostValue),
    /// Intrinsic own writable/enumerable/configurable data property creation.
    DefineData {
        receiver: HostValue,
        key: HostKey,
        value: HostValue,
    },
    BeginForIn(HostValue),
    NextForIn(JsValueId),
}
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOperation {
    Add,
    Subtract,
    LessThan,
    GreaterThan,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum JsDescriptorKind {
    Data { value: HostValue, writable: bool },
    Accessor { get: HostValue, set: HostValue },
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct JsOwnDescriptor {
    pub enumerable: bool,
    pub configurable: bool,
    pub kind: JsDescriptorKind,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ObjectInspection {
    Keys(Vec<HostKey>),
    Descriptor(Option<JsOwnDescriptor>),
    Value(HostValue),
    BackendIdentity(Option<HostObjectId>),
    ForInKey(Option<Vec<u16>>),
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleLevel {
    Log,
    Warn,
    Error,
}

pub const PROTOCOL_VERSION: u16 = 6;
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyFlags {
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FunctionPrototypeShape {
    pub flags: PropertyFlags,
    pub constructor_flags: PropertyFlags,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HostFunctionShape {
    pub constructible: bool,
    pub name_flags: PropertyFlags,
    pub length_flags: PropertyFlags,
    pub prototype: Option<FunctionPrototypeShape>,
}
impl HostFunctionShape {
    /// Standard strict ordinary function shape, also independently captured
    /// for these 15 SDK functions. Other SDK members require their own metadata.
    pub fn strict_function() -> Self {
        Self {
            constructible: true,
            name_flags: PropertyFlags {
                writable: false,
                enumerable: false,
                configurable: true,
            },
            length_flags: PropertyFlags {
                writable: false,
                enumerable: false,
                configurable: true,
            },
            prototype: Some(FunctionPrototypeShape {
                flags: PropertyFlags {
                    writable: true,
                    enumerable: false,
                    configurable: false,
                },
                constructor_flags: PropertyFlags {
                    writable: true,
                    enumerable: false,
                    configurable: true,
                },
            }),
        }
    }
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum HostDescriptorKind {
    Data {
        value: HostValue,
        writable: bool,
    },
    Method {
        binding: HostBinding,
        name: Vec<u16>,
        length: u32,
        writable: bool,
        shape: HostFunctionShape,
    },
    Accessor {
        get: Option<HostBinding>,
        set: Option<HostBinding>,
    },
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostDescriptor {
    pub key: HostKey,
    pub kind: HostDescriptorKind,
    pub enumerable: bool,
    pub configurable: bool,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct HostClassSpec {
    /// SDK export/registry key; it need not equal the minified function name.
    pub name: Vec<u16>,
    pub constructor_name: Vec<u16>,
    pub constructor_length: u32,
    pub parent: Option<Vec<u16>>,
    pub constructor_binding: HostBinding,
    pub constructor_shape: HostFunctionShape,
    pub prototype_members: Vec<HostDescriptor>,
    pub static_members: Vec<HostDescriptor>,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ParentMessage {
    Initialize {
        version: u16,
        context: u64,
    },
    Evaluate {
        id: u64,
        code: Option<Vec<u16>>,
    },
    Bind {
        id: u64,
        name: Vec<u16>,
        binding: HostBinding,
    },
    RegisterClass {
        id: u64,
        class: HostClassSpec,
    },
    UpdateInstance {
        id: u64,
        object: HostObjectId,
        properties: Vec<HostDescriptor>,
    },
    RegisterInstance {
        id: u64,
        object: HostObjectId,
        class: Vec<u16>,
        reference: Option<JsValueId>,
        properties: Vec<HostDescriptor>,
    },
    HostReply {
        id: u64,
        result: HostResult,
    },
    Release {
        id: u64,
        reference: JsValueId,
    },
    RetainReference {
        id: u64,
        reference: JsValueId,
    },
    ReleaseLease {
        id: u64,
        reference: JsValueId,
    },
    PrefixReady {
        id: u64,
    },
    RequestPoll {
        id: u64,
    },
    PollFinished {
        id: u64,
    },
    Inspect {
        id: u64,
        query: ObjectQuery,
    },
    Shutdown,
}
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum ChildMessage {
    Inspected {
        id: u64,
        result: Result<ObjectInspection, HostError>,
    },
    Ready {
        version: u16,
        context: u64,
        pid: u32,
        versions: std::collections::BTreeMap<String, String>,
    },
    HandlerStarted {
        id: u64,
        capture: u64,
    },
    HandlerFinished {
        id: u64,
        capture: u64,
    },
    Evaluated {
        id: u64,
        body: Vec<u16>,
    },
    Console {
        id: u64,
        level: ConsoleLevel,
        message: Vec<u16>,
    },
    HostCall {
        eval_id: u64,
        id: u64,
        call: HostCall,
    },
    HostPoll {
        id: u64,
    },
    Acknowledged {
        id: u64,
        result: Result<(), HostError>,
    },
    Failed {
        id: Option<u64>,
        error: HostError,
    },
}

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    Encoding(postcard::Error),
    LengthOverflow,
}
impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Encoding(error) => write!(f, "{error}"),
            Self::LengthOverflow => write!(f, "Runtime IPC frame length exceeds u32"),
        }
    }
}
impl std::error::Error for FrameError {}
pub fn write_frame(
    writer: &mut impl std::io::Write,
    value: &impl Serialize,
) -> Result<(), FrameError> {
    let bytes = postcard::to_stdvec(value).map_err(FrameError::Encoding)?;
    let length = u32::try_from(bytes.len()).map_err(|_| FrameError::LengthOverflow)?;
    writer
        .write_all(&length.to_le_bytes())
        .map_err(FrameError::Io)?;
    writer.write_all(&bytes).map_err(FrameError::Io)?;
    writer.flush().map_err(FrameError::Io)
}
pub fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::Read,
) -> Result<T, FrameError> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header).map_err(FrameError::Io)?;
    let mut bytes = vec![0; u32::from_le_bytes(header) as usize];
    reader.read_exact(&mut bytes).map_err(FrameError::Io)?;
    postcard::from_bytes(&bytes).map_err(FrameError::Encoding)
}
