//! Engine-owned application services in the actual JavaScript realm.
//!
//! This first service binding installs the engine's real vault and adapter
//! identity, and the native uncoerced getBasePath selector. The complete App,
//! Vault and FileSystemAdapter surfaces, their constructors, event lifecycle
//! and SDK module exports remain required; these objects are partial services.
use mnemosyne_core::CONFIG_DIR;
use crate::{script_metadata_types::ScriptMetadataTypes, script_methods, Engine};
use mnemosyne_runtime::{
    EvalOutcome, NodeChild, NodeChildConfig, NodeHostDispatcher, NodeIdentity, RendererConfig,
    RuntimeError, StopReceipt,
};
use mnemosyne_runtime_protocol::{
    Delivery, HostBinding, HostClassSpec, HostDescriptor, HostDescriptorKind, HostError,
    HostFunctionShape, HostKey, HostObjectId, HostOperation, HostRequest, HostResult, HostValue,
    ObjectInspection, ObjectQuery,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

/// Explicit development installation of the pinned current runtime. Executable
/// and framework identities, versions and official remote files are verified
/// before launch. Profiles must be new; personal app profiles are never found.
/// Packaging and required other-platform installations remain separate work.
#[derive(serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstallation {
    pub executable: PathBuf,
    pub framework: PathBuf,
    /// Built Rust Node-API library packaged with the .node extension.
    pub addon: PathBuf,
    pub profile: PathBuf,
    pub remote_package: PathBuf,
}
impl RuntimeInstallation {
    pub fn resolve_relative_to(&mut self, directory: &Path) {
        for path in [
            &mut self.executable,
            &mut self.framework,
            &mut self.addon,
            &mut self.profile,
            &mut self.remote_package,
        ] {
            if path.is_relative() {
                *path = directory.join(&*path);
            }
        }
    }
}

// Service IDs are allocated once and never recycled. Consolidation with file
// and plugin instance allocation is required before the complete SDK is hosted.
static NEXT_SERVICE: AtomicU64 = AtomicU64::new(1 << 32);
fn reserve_service_ids(count: u64) -> Result<u64, RuntimeError> {
    NEXT_SERVICE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |first| {
            first.checked_add(count)
        })
        .map_err(|_| RuntimeError::Initialization("Application service IDs exhausted".into()))
}

struct Services {
    engine: Engine,
    adapter_dispatch: HostObjectId,
    app: HostValue,
    metadata: Option<ScriptMetadataTypes>,
}
struct Properties<'a> {
    runtime: &'a mut NodeChild,
    services: &'a mut Services,
}
impl script_methods::Properties for Properties<'_> {
    fn get(&mut self, receiver: &HostValue, name: &[u16]) -> HostResult {
        match self.runtime.inspect_with_dispatcher(
            ObjectQuery::GetValue(receiver.clone(), HostKey::Utf16(name.to_vec())),
            self.services,
        )? {
            ObjectInspection::Value(value) => Ok(value),
            _ => Err(HostError::unsupported(
                "Property inspection returned no value",
            )),
        }
    }
}

impl NodeHostDispatcher for Services {
    fn dispatch(&mut self, runtime: &mut NodeChild, request: HostRequest) {
        if let Some(service) = self.metadata.clone() {
            if request.call.binding.target == service.target {
                let inferred = self.engine.inferred_property_entries();
                let occurrences = self.engine.property_occurrence_entries_units();
                service.dispatch(runtime, self, request, inferred, occurrences);
                return;
            }
        }
        let result = if request.call.binding.target != self.adapter_dispatch {
            Err(HostError::unsupported(
                "Unknown application service dispatch identity",
            ))
        } else if request.call.binding.member.as_ref() == Some(&"getBasePath".into()) {
            script_methods::adapter_base_path(
                &request.call.receiver,
                &mut Properties {
                    runtime,
                    services: self,
                },
            )
        } else {
            // Do not silently construct an uninitialized adapter. The adapter
            // constructor's seven dependency arguments and lifecycle are still
            // required independently of the existing engine-owned instance.
            Err(HostError::unsupported(
                "FileSystemAdapter constructor binding is pending",
            ))
        };
        request.respond(result);
    }
    fn has_pending_jobs(&self) -> bool {
        self.metadata
            .as_ref()
            .is_some_and(ScriptMetadataTypes::has_pending_jobs)
    }
    fn poll(&mut self, runtime: &mut NodeChild) {
        if let Some(service) = self.metadata.clone() {
            let inferred = self.engine.inferred_property_entries();
            service.poll(runtime, self, inferred);
        }
    }
}

fn data(key: &str, value: HostValue) -> HostDescriptor {
    HostDescriptor {
        key: key.into(),
        enumerable: true,
        configurable: true,
        kind: HostDescriptorKind::Data {
            value,
            writable: true,
        },
    }
}
fn object(id: HostObjectId, class: &str) -> HostValue {
    HostValue::HostObject {
        id,
        class: class.encode_utf16().collect(),
    }
}
fn value(result: Result<ObjectInspection, HostError>) -> Result<HostValue, RuntimeError> {
    match result {
        Ok(ObjectInspection::Value(value)) => Ok(value),
        Ok(_) => Err(RuntimeError::Initialization(
            "Application installation expected a VM value".into(),
        )),
        Err(error) => Err(RuntimeError::Initialization(format!(
            "{}: {}",
            error.name, error.message
        ))),
    }
}

/// A real runtime and the engine whose vault its application namespace exposes.
/// Evaluation always uses the Rust service dispatcher; it never installs a
/// synthetic JavaScript implementation of product methods.
pub struct ScriptApplication {
    runtime: NodeChild,
    services: Services,
}
impl ScriptApplication {
    pub fn launch(engine: Engine, installation: RuntimeInstallation) -> Result<Self, RuntimeError> {
        if installation
            .addon
            .extension()
            .is_none_or(|extension| extension != "node")
        {
            return Err(RuntimeError::Initialization(
                "Package the Rust addon with the .node extension".into(),
            ));
        }
        let remote: serde_json::Value =
            serde_json::from_str(include_str!("runtime_remote_files.json"))
                .map_err(|error| RuntimeError::Initialization(error.to_string()))?;
        let remote_files_sha256 = remote["files"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(name, digest)| (name.clone(), digest.as_str().unwrap().to_owned()))
            .collect();
        let context = reserve_service_ids(1)?;
        let runtime = NodeChild::launch_renderer(
            NodeChildConfig {
                executable: installation.executable,
                framework: installation.framework,
                addon: installation.addon,
                profile: installation.profile,
                vault: engine.vault.root.clone(),
                identity: NodeIdentity {
                    executable_sha256:
                        "1121571fb0838d50519c2f002780b2b69f285d37993cf5622ee4b88c6f4b5ce4".into(),
                    framework_sha256:
                        "be1b9531ce2218021027aab470e1c188c975d1525b176992c160499cb860838c".into(),
                    versions: BTreeMap::from([
                        ("node".into(), "24.21.0".into()),
                        ("electron".into(), "43.7.7".into()),
                        ("v8".into(), "15.0.245.31-electron.0".into()),
                        ("icu".into(), "78.2".into()),
                    ]),
                },
                context,
                startup_timeout: Duration::from_secs(20),
            },
            RendererConfig {
                remote_package: installation.remote_package,
                remote_files_sha256,
            },
        )?;
        let mut application = Self::install(engine, runtime)?;
        application.install_metadata_types()?;
        Ok(application)
    }
    pub fn install(engine: Engine, mut runtime: NodeChild) -> Result<Self, RuntimeError> {
        let first = reserve_service_ids(4)?;
        let [app, vault, adapter, dispatch] =
            [0, 1, 2, 3].map(|offset| HostObjectId(first + offset));
        // Retain the engine's path spelling, including symlink components.
        // Resolving it through canonicalize would introduce an unobserved
        // change to the ordinary writable basePath property.
        let base_path = if engine.vault.root.is_absolute() {
            engine.vault.root.clone()
        } else {
            std::env::current_dir()
                .map_err(|error| RuntimeError::Initialization(error.to_string()))?
                .join(&engine.vault.root)
        };
        let base_path = base_path.to_str().ok_or_else(|| {
            RuntimeError::Initialization(
                "Non-UTF8 vault paths require the platform path bridge".into(),
            )
        })?;
        let mut services = Services {
            engine,
            adapter_dispatch: dispatch,
            app: object(app, "Object"),
            metadata: None,
        };
        let binding = |member: &str, operation| HostBinding {
            target: dispatch,
            operation,
            member: Some(member.into()),
            delivery: Delivery::Sync,
        };
        runtime.register_class(HostClassSpec {
            name: "FileSystemAdapter".encode_utf16().collect(),
            constructor_name: "e".encode_utf16().collect(),
            constructor_length: 7,
            parent: None,
            constructor_binding: binding("constructor", HostOperation::Construct),
            constructor_shape: HostFunctionShape::strict_function(),
            prototype_members: vec![HostDescriptor {
                key: "getBasePath".into(),
                enumerable: true,
                configurable: true,
                kind: HostDescriptorKind::Method {
                    binding: binding("getBasePath", HostOperation::Call),
                    name: Vec::new(),
                    length: 0,
                    writable: true,
                    shape: HostFunctionShape::strict_function(),
                },
            }],
            static_members: Vec::new(),
        })?;
        runtime.register_instance(
            adapter,
            "FileSystemAdapter",
            None,
            vec![data("basePath", base_path.into())],
        )?;
        runtime.register_instance(
            vault,
            "Object",
            None,
            vec![
                data("adapter", object(adapter, "FileSystemAdapter")),
                // This is the engine's current configuration directory. Full
                // configurable directory and mutation routing remain required.
                data("configDir", mnemosyne_core::CONFIG_DIR.into()),
            ],
        )?;
        runtime.register_instance(
            app,
            "Object",
            None,
            vec![data("vault", object(vault, "Object"))],
        )?;
        let global = value(runtime.inspect_with_dispatcher(
            ObjectQuery::GlobalValue("globalThis".into()),
            &mut services,
        ))?;
        value(runtime.inspect_with_dispatcher(
            ObjectQuery::DefineData {
                receiver: global,
                key: "app".into(),
                value: object(app, "Object"),
            },
            &mut services,
        ))?;
        Ok(Self { runtime, services })
    }

    /// Attach the live manager service to this application's actual receiver.
    /// This imports the completed engine snapshot. Full hot reload and shared
    /// CLI/SDK mutation authority remain required separately.
    pub fn install_metadata_types(&mut self) -> Result<(), RuntimeError> {
        if self.services.metadata.is_some() {
            return Err(RuntimeError::Initialization(
                "Metadata type service is already installed".into(),
            ));
        }
        let app = self.services.app.clone();
        let materialized = value(
            self.runtime
                .inspect_with_dispatcher(ObjectQuery::Materialize(app.clone()), &mut self.services),
        )?;
        let HostValue::Reference(reference) = materialized else {
            return Err(RuntimeError::Initialization(
                "Application receiver was not rooted".into(),
            ));
        };
        let context = reference.id.context;
        self.runtime.release_reference(reference.id)?;
        let first = reserve_service_ids(2)?;
        let assignments = self.services.engine.canonical_assignments_units();
        let inferred = self.services.engine.inferred_property_entries();
        let path = self.services.engine.vault.root.join(CONFIG_DIR).join("types.json");
        let service = ScriptMetadataTypes::install(
            &mut self.runtime,
            &mut self.services,
            context,
            HostObjectId(first),
            HostObjectId(first + 1),
            app.clone(),
            path,
            assignments,
            inferred,
        )?;
        let receiver = service.receiver.clone();
        self.services.metadata = Some(service);
        value(self.runtime.inspect_with_dispatcher(
            ObjectQuery::DefineData {
                receiver: app,
                key: "metadataTypeManager".into(),
                value: receiver,
            },
            &mut self.services,
        ))?;
        Ok(())
    }

    pub fn engine(&self) -> &Engine {
        &self.services.engine
    }
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.services.engine
    }
    pub fn evaluate(&mut self, code: Option<Vec<u16>>) -> Result<EvalOutcome, RuntimeError> {
        self.runtime
            .evaluate_with_dispatcher(code, &mut self.services, |_, _| {})
    }
    pub fn stop(&mut self) -> Result<StopReceipt, RuntimeError> {
        self.runtime.stop()
    }
}
