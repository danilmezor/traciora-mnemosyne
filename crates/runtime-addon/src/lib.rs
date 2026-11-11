use napi::{
    JsValue,
    bindgen_prelude::{Unknown, Utf16String},
};
use napi_derive::napi;
mod evaluate;
#[cfg(unix)]
mod host;
#[cfg(unix)]
mod ipc;
#[cfg(unix)]
mod promise_flow;
#[cfg(unix)]
mod registry;
#[napi]
pub fn rust_runtime_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
#[napi]
pub fn echo_utf16(value: Utf16String) -> Utf16String {
    value
}
#[napi]
pub fn evaluate<'env>(
    env: &'env napi::Env,
    code: Option<Utf16String>,
) -> napi::Result<napi::bindgen_prelude::Unknown<'env>> {
    evaluate::evaluate(env, code)
}
/// Private transport integration entry: source handleCli's eval branch.
/// Does not install a handleCli global or expose a complete SDK module.
#[cfg(unix)]
#[napi]
pub fn handle_cli<'env>(env: &'env napi::Env, code: Option<Utf16String>) -> napi::Result<Unknown<'env>> {
    let value = promise_flow::current(env.raw(), promise_flow::Work::Handle(code.map(|value| value.to_vec())));
    match value {
        Ok(value) => unsafe { napi::bindgen_prelude::FromNapiValue::from_napi_value(env.raw(), value) },
        Err(()) => Err(napi::Error::new(napi::Status::PendingException, "Native CLI handler failed")),
    }
}
#[cfg(unix)]
#[napi]
pub fn start(
    env: &napi::Env,
    socket: String,
    require: Unknown<'_>,
    module: Unknown<'_>,
    filename: Utf16String,
    dirname: Utf16String,
    factory: Unknown<'_>,
    property_read: Unknown<'_>,
    property_write: Unknown<'_>,
    binary: Unknown<'_>,
    instance_of: Unknown<'_>,
    install_commonjs: Option<bool>,
    property_delete: Option<Unknown<'_>>,
    for_in: Option<Unknown<'_>>,
) -> napi::Result<()> {
    ipc::start(
        env,
        &socket,
        require.raw(),
        module.raw(),
        &filename,
        &dirname,
        factory.raw(),
        property_read.raw(),
        property_write.raw(),
        binary.raw(),
        instance_of.raw(),
        install_commonjs.unwrap_or(true),
        property_delete.map(|value| value.raw()),
        for_in.map(|value| value.raw()),
    )
}
