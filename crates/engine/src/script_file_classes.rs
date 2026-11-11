//! Rust file-constructor and selector bindings, installed with native function
//! shapes. This is the implemented subset of the file SDK; Vault integration
//! and the remaining file methods must be added before full SDK certification.
use crate::script_methods::{self, FileClass, FileMethod, Objects};
use mnemosyne_runtime_protocol::{
    Delivery, HostBinding, HostCall, HostClassSpec, HostDescriptor, HostDescriptorKind, HostError,
    HostFunctionShape, HostKey, HostObjectId, HostOperation, HostResult, HostValue,
};

fn binding(target: HostObjectId, key: &str, operation: HostOperation) -> HostBinding {
    HostBinding {
        target,
        operation,
        member: Some(key.into()),
        delivery: Delivery::Sync,
    }
}

/// All product bodies attached here are implemented in Rust. `target` is a
/// caller-allocated dispatch identity, distinct from each JS receiver identity.
pub fn specifications(target: HostObjectId) -> Vec<HostClassSpec> {
    [
        ("TAbstractFile", "e", vec![("setPath", 1)]),
        (
            "TFile",
            "t",
            vec![("setPath", 1), ("getShortName", 0), ("toString", 0)],
        ),
        (
            "TFolder",
            "t",
            vec![
                ("isRoot", 0),
                ("getParentPrefix", 0),
                ("getFileCount", 0),
                ("getFolderCount", 0),
            ],
        ),
    ]
    .into_iter()
    .map(|(key, constructor, members)| {
        let mut shape = HostFunctionShape::strict_function();
        shape
            .prototype
            .as_mut()
            .unwrap()
            .constructor_flags
            .enumerable = key != "TAbstractFile";
        HostClassSpec {
            name: key.encode_utf16().collect(),
            constructor_name: constructor.encode_utf16().collect(),
            constructor_length: 2,
            parent: (key != "TAbstractFile").then(|| "TAbstractFile".encode_utf16().collect()),
            constructor_binding: binding(
                target,
                &format!("{key}::constructor"),
                HostOperation::Construct,
            ),
            constructor_shape: shape,
            prototype_members: members
                .into_iter()
                .map(|(member, length)| HostDescriptor {
                    key: member.into(),
                    enumerable: true,
                    configurable: true,
                    kind: HostDescriptorKind::Method {
                        binding: binding(target, &format!("{key}::{member}"), HostOperation::Call),
                        name: Vec::new(),
                        length,
                        writable: true,
                        shape: HostFunctionShape::strict_function(),
                    },
                })
                .collect(),
            static_members: Vec::new(),
        }
    })
    .collect()
}

pub fn dispatch(call: &HostCall, objects: &mut impl Objects) -> HostResult {
    let Some(HostKey::Utf16(key)) = &call.binding.member else {
        return Err(HostError::unsupported("Missing SDK file binding member"));
    };
    // Binding keys are authored Rust metadata, never arbitrary JS property
    // names. Do not coerce the caller's UTF16 strings to choose product behavior.
    let key = String::from_utf16(key)
        .map_err(|_| HostError::unsupported("Invalid SDK file binding member"))?;
    let argument = |n| {
        call.arguments
            .get(n)
            .cloned()
            .unwrap_or(HostValue::Undefined)
    };
    let constructor = match key.as_str() {
        "TAbstractFile::constructor" => Some(FileClass::Abstract),
        "TFile::constructor" => Some(FileClass::File),
        "TFolder::constructor" => Some(FileClass::Folder),
        _ => None,
    };
    if let Some(class) = constructor {
        return script_methods::initialize(
            class,
            &call.receiver,
            argument(0),
            argument(1),
            objects,
        );
    }
    match key.as_str() {
        "TAbstractFile::setPath" => {
            script_methods::set_path(FileClass::Abstract, &call.receiver, argument(0), objects)
        }
        "TFile::setPath" => {
            script_methods::set_path(FileClass::File, &call.receiver, argument(0), objects)
        }
        "TFile::getShortName" => {
            script_methods::call(FileMethod::GetShortName, &call.receiver, objects)
        }
        "TFile::toString" => script_methods::call(FileMethod::ToString, &call.receiver, objects),
        "TFolder::isRoot" => script_methods::call(FileMethod::IsRoot, &call.receiver, objects),
        "TFolder::getParentPrefix" => script_methods::parent_prefix(&call.receiver, objects),
        "TFolder::getFileCount" => script_methods::count_children(&call.receiver, false, objects),
        "TFolder::getFolderCount" => script_methods::count_children(&call.receiver, true, objects),
        _ => Err(HostError::unsupported(format!(
            "Unknown SDK file binding {key}"
        ))),
    }
}
