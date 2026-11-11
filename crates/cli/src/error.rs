//! Preserve JavaScript exception names at the application output boundary.
#[derive(Debug, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct ScriptException {
    pub kind: &'static str,
    pub message: String,
}

pub fn render(error: &anyhow::Error) -> String {
    if let Some(exception) = error.downcast_ref::<ScriptException>() {
        format!("{exception}\n")
    } else {
        format!("Error: {error}\n")
    }
}
