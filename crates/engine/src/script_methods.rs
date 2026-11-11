//! Rust SDK selectors over actual caller-owned JavaScript properties.
//! A runtime adapter must read properties in order and preserve rooted return
//! values and exceptions. This module does not install or fake an SDK class.
use mnemosyne_runtime_protocol::{HostError, HostResult, HostValue, ReferenceKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMethod {
    ToString,
    GetShortName,
    IsRoot,
}

pub trait Properties {
    /// JavaScript property read, including user getters and primitive boxing.
    fn get(&mut self, receiver: &HostValue, name: &[u16]) -> HostResult;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileClass {
    Abstract,
    File,
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arithmetic {
    Add,
    Subtract,
    LessThan,
    GreaterThan,
}

/// Generic VM operations, not a second implementation of SDK behavior in JS.
/// Assignments are strict; calls retain receivers and original thrown values.
pub trait Objects: Properties {
    fn set(&mut self, receiver: &HostValue, name: &[u16], value: HostValue) -> HostResult;
    fn invoke(
        &mut self,
        function: HostValue,
        receiver: HostValue,
        arguments: Vec<HostValue>,
    ) -> HostResult;
    fn arithmetic(
        &mut self,
        operation: Arithmetic,
        left: HostValue,
        right: HostValue,
    ) -> HostResult;
    fn instance_of(&mut self, value: HostValue, constructor: HostValue) -> HostResult;
}

fn write(
    objects: &mut impl Objects,
    receiver: &HostValue,
    name: &str,
    value: HostValue,
) -> HostResult {
    objects.set(receiver, &name.encode_utf16().collect::<Vec<_>>(), value)
}

fn invoke_member(
    objects: &mut impl Objects,
    receiver: &HostValue,
    name: &str,
    arguments: Vec<HostValue>,
    expression: &str,
) -> HostResult {
    let function = read(objects, receiver, name)?;
    let callable = matches!(
        function,
        HostValue::Reference(mnemosyne_runtime_protocol::JsReference {
            kind: ReferenceKind::Function,
            ..
        }) | HostValue::HostClass { .. }
    );
    if !callable {
        return Err(HostError::new(
            "TypeError",
            format!("{expression} is not a function"),
        ));
    }
    objects.invoke(function, receiver.clone(), arguments)
}

/// Constructor function bodies. JavaScript's ordinary function wrapper handles
/// `new` and derived prototypes; the body must invoke virtual `this.setPath`.
/// Detached constructors do not register files with a Vault.
pub fn initialize(
    class: FileClass,
    receiver: &HostValue,
    vault: HostValue,
    path: HostValue,
    objects: &mut impl Objects,
) -> HostResult {
    if class != FileClass::Abstract {
        // The transpiled derived constructors call the mutable base function's
        // .call property and select its truthy return value before field writes.
        let base = HostValue::HostClass {
            key: "TAbstractFile".encode_utf16().collect(),
        };
        let returned = invoke_member(
            objects,
            &base,
            "call",
            vec![receiver.clone(), vault, path],
            "e.call",
        )?;
        let instance = if truthy(&returned) {
            returned
        } else {
            receiver.clone()
        };
        if class == FileClass::File {
            write(objects, &instance, "saving", HostValue::Number(0.0))?;
            write(objects, &instance, "stat", HostValue::Null)?;
        } else {
            write(objects, &instance, "children", HostValue::Array(Vec::new()))?;
        }
        return Ok(instance);
    }
    write(objects, receiver, "parent", HostValue::Null)?;
    write(objects, receiver, "deleted", HostValue::Boolean(false))?;
    write(objects, receiver, "vault", vault)?;
    invoke_member(objects, receiver, "setPath", vec![path], "this.setPath")?;
    Ok(HostValue::Undefined)
}

fn filename(path: HostValue, objects: &mut impl Objects) -> HostResult {
    let index = invoke_member(
        objects,
        &path,
        "lastIndexOf",
        vec![HostValue::from("/")],
        "e.lastIndexOf",
    )?;
    if index == HostValue::Number(-1.0) {
        return Ok(path);
    }
    let start = objects.arithmetic(Arithmetic::Add, index, HostValue::Number(1.0))?;
    invoke_member(objects, &path, "slice", vec![start], "e.slice")
}

fn basename(name: HostValue, objects: &mut impl Objects) -> HostResult {
    let tail = filename(name, objects)?;
    let dot = invoke_member(
        objects,
        &tail,
        "lastIndexOf",
        vec![HostValue::from(".")],
        "t.lastIndexOf",
    )?;
    if dot == HostValue::Number(-1.0) {
        return Ok(tail);
    }
    let length = read(objects, &tail, "length")?;
    let last = objects.arithmetic(Arithmetic::Subtract, length, HostValue::Number(1.0))?;
    if dot == last || dot == HostValue::Number(0.0) {
        return Ok(tail);
    }
    invoke_member(
        objects,
        &tail,
        "substr",
        vec![HostValue::Number(0.0), dot],
        "t.substr",
    )
}

fn extension(name: HostValue, objects: &mut impl Objects) -> HostResult {
    let dot = invoke_member(
        objects,
        &name,
        "lastIndexOf",
        vec![HostValue::from(".")],
        "e.lastIndexOf",
    )?;
    if dot == HostValue::Number(-1.0) {
        return Ok(HostValue::from(""));
    }
    let length = read(objects, &name, "length")?;
    let last = objects.arithmetic(Arithmetic::Subtract, length, HostValue::Number(1.0))?;
    if dot == last || dot == HostValue::Number(0.0) {
        return Ok(HostValue::from(""));
    }
    let start = objects.arithmetic(Arithmetic::Add, dot, HostValue::Number(1.0))?;
    let suffix = invoke_member(objects, &name, "substr", vec![start], "e.substr")?;
    invoke_member(
        objects,
        &suffix,
        "toLowerCase",
        Vec::new(),
        "e.substr(...).toLowerCase",
    )
}

/// Preserve partial assignments on parsing failure, and observe each separate
/// read of `this.name`. String methods can be overridden by plugin code.
pub fn set_path(
    class: FileClass,
    receiver: &HostValue,
    path: HostValue,
    objects: &mut impl Objects,
) -> HostResult {
    if class == FileClass::File {
        // Keep the captured virtual base-prototype call observable, including
        // replacements of the base prototype, method or method.call property.
        let base = HostValue::HostClass {
            key: "TAbstractFile".encode_utf16().collect(),
        };
        let prototype = read(objects, &base, "prototype")?;
        let method = read(objects, &prototype, "setPath")?;
        invoke_member(
            objects,
            &method,
            "call",
            vec![receiver.clone(), path],
            "e.prototype.setPath.call",
        )?;
        let name = read(objects, receiver, "name")?;
        let base = basename(name, objects)?;
        write(objects, receiver, "basename", base)?;
        let name = read(objects, receiver, "name")?;
        let ext = extension(name, objects)?;
        write(objects, receiver, "extension", ext)?;
    } else {
        write(objects, receiver, "path", path.clone())?;
        let name = filename(path, objects)?;
        write(objects, receiver, "name", name)?;
    }
    Ok(HostValue::Undefined)
}

fn truthy(value: &HostValue) -> bool {
    match value {
        HostValue::Undefined | HostValue::Null => false,
        HostValue::Boolean(value) => *value,
        HostValue::Number(value) => *value != 0.0 && !value.is_nan(),
        HostValue::Utf16(value) => !value.is_empty(),
        HostValue::BigInt { words, .. } => words.iter().any(|word| *word != 0),
        _ => true,
    }
}

pub fn parent_prefix(receiver: &HostValue, objects: &mut impl Objects) -> HostResult {
    let root = invoke_member(objects, receiver, "isRoot", Vec::new(), "this.isRoot")?;
    if truthy(&root) {
        Ok(HostValue::from(""))
    } else {
        let path = read(objects, receiver, "path")?;
        objects.arithmetic(Arithmetic::Add, path, HostValue::from("/"))
    }
}

/// Live child arrays, dynamic length, instanceof/hasInstance and overridden
/// recursive methods are all observable. Do not count a copied Rust tree when
/// servicing caller-owned JS objects.
pub fn count_children(
    receiver: &HostValue,
    folders: bool,
    objects: &mut impl Objects,
) -> HostResult {
    let children = read(objects, receiver, "children")?;
    let mut index = 0.0;
    let mut count = HostValue::Number(0.0);
    loop {
        let length = read(objects, &children, "length")?;
        let within = objects.arithmetic(Arithmetic::LessThan, HostValue::Number(index), length)?;
        if !truthy(&within) {
            break;
        }
        let key = mnemosyne_core::vault_search::ecmascript_number(index);
        let child = read(objects, &children, &key)?;
        let is_folder = objects.instance_of(
            child.clone(),
            HostValue::HostClass {
                key: "TFolder".encode_utf16().collect(),
            },
        )?;
        let increment = if truthy(&is_folder) {
            let method = if folders {
                "getFolderCount"
            } else {
                "getFileCount"
            };
            let expression = if folders {
                "r.getFolderCount"
            } else {
                "r.getFileCount"
            };
            let nested = invoke_member(objects, &child, method, Vec::new(), expression)?;
            if folders {
                objects.arithmetic(Arithmetic::Add, HostValue::Number(1.0), nested)?
            } else {
                nested
            }
        } else if folders {
            index += 1.0;
            continue;
        } else {
            HostValue::Number(1.0)
        };
        count = objects.arithmetic(Arithmetic::Add, count, increment)?;
        index += 1.0;
    }
    Ok(count)
}

fn read(properties: &mut impl Properties, receiver: &HostValue, name: &str) -> HostResult {
    // The pinned methods are strict. Preserve the property-read exception,
    // rather than N-API's different ToObject error or sloppy global receiver.
    let nullish = match receiver {
        HostValue::Null => Some("null"),
        HostValue::Undefined => Some("undefined"),
        _ => None,
    };
    if let Some(value) = nullish {
        return Err(HostError::new(
            "TypeError",
            format!("Cannot read properties of {value} (reading '{name}')"),
        ));
    }
    properties.get(receiver, &name.encode_utf16().collect::<Vec<_>>())
}

pub fn call(
    method: FileMethod,
    receiver: &HostValue,
    properties: &mut impl Properties,
) -> HostResult {
    match method {
        FileMethod::ToString => read(properties, receiver, "path"),
        FileMethod::GetShortName => {
            let extension = read(properties, receiver, "extension")?;
            if extension == HostValue::from("md") {
                read(properties, receiver, "basename")
            } else {
                read(properties, receiver, "name")
            }
        }
        FileMethod::IsRoot => {
            let path = read(properties, receiver, "path")?;
            Ok(HostValue::Boolean(path == HostValue::from("/")))
        }
    }
}

/// FileSystemAdapter's native accessor method returns the live property
/// unchanged; initialization of that property belongs to the Engine adapter.
pub fn adapter_base_path(receiver: &HostValue, properties: &mut impl Properties) -> HostResult {
    read(properties, receiver, "basePath")
}
