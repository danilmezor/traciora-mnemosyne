//! Retained bookmark data and the observed first-call 20ms save debounce.
//! Native event/lookup-cache bindings, migration and workspace actions are
//! separate lifecycle contracts; this module does not fabricate their success.
use mnemosyne_core::CONFIG_DIR;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
struct PendingSave {
    active: bool,
    deadline: Option<Instant>,
    document: Option<Value>,
    diagnostics: Vec<String>,
}
#[derive(Debug, Default)]
pub struct Bookmarks {
    items: Vec<Value>,
    pub has_valid_data: bool,
    path: PathBuf,
    pending: Arc<Mutex<PendingSave>>,
    workers: Vec<JoinHandle<()>>,
}
impl Bookmarks {
    pub fn open(root: &Path) -> Self {
        let mut store = Self {
            path: root.join(CONFIG_DIR).join("bookmarks.json"),
            items: Vec::new(),
            has_valid_data: false,
            pending: Arc::default(),
            workers: Vec::new(),
        };
        store.reload();
        store
    }
    /// Native loadData leaves existing items in place if no items array exists.
    /// Loading does not itself schedule a config write.
    pub fn reload(&mut self) {
        let data = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        self.has_valid_data = data.is_some();
        if let Some(items) = data
            .as_ref()
            .and_then(|v| v.get("items"))
            .and_then(Value::as_array)
        {
            self.items.clone_from(items);
        }
    }
    pub fn items(&self) -> &[Value] {
        &self.items
    }
    /// Visits groups before their children and retains duplicates.
    pub fn flattened(&self) -> Vec<&Value> {
        fn walk<'a>(items: &'a [Value], output: &mut Vec<&'a Value>) {
            for item in items {
                output.push(item);
                if item.get("type").and_then(Value::as_str) == Some("group") {
                    if let Some(children) = item.get("items").and_then(Value::as_array) {
                        walk(children, output);
                    }
                }
            }
        }
        let mut output = Vec::new();
        walk(&self.items, &mut output);
        output
    }
    pub fn add(&mut self, item: Value) {
        self.items.push(item);
        self.on_items_changed(true);
    }
    /// Latest argument replaces the prior argument within the first-call timer
    /// window. A false update can suppress an already scheduled save, like Gl.
    pub fn on_items_changed(&mut self, save: bool) {
        let mut pending = self.pending.lock().unwrap();
        pending.document = save.then(|| json!({"items":self.items}));
        if pending.deadline.is_none() {
            pending.deadline = Some(Instant::now() + Duration::from_millis(20));
        }
        if pending.active {
            return;
        }
        pending.active = true;
        drop(pending);
        let pending = Arc::clone(&self.pending);
        let path = self.path.clone();
        self.workers.push(std::thread::spawn(move || loop {
            let deadline = {
                let mut state = pending.lock().unwrap();
                let Some(deadline) = state.deadline else {
                    state.active = false;
                    break;
                };
                deadline
            };
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
            let document = {
                let mut state = pending.lock().unwrap();
                state.deadline = None;
                state.document.take()
            };
            if let Some(document) = document {
                let result = (|| -> Result<(), String> {
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    let bytes = serde_json::to_vec_pretty(&document).map_err(|e| e.to_string())?;
                    std::fs::write(&path, bytes).map_err(|e| e.to_string())
                })();
                if let Err(error) = result {
                    pending.lock().unwrap().diagnostics.push(error);
                }
            }
        }));
        // Reap completed windows so a long-lived vault does not retain handles.
        let mut active = Vec::new();
        for worker in self.workers.drain(..) {
            if worker.is_finished() {
                let _ = worker.join();
            } else {
                active.push(worker);
            }
        }
        self.workers = active;
    }
    /// Owned engine shutdown waits for its pending file write; it does not
    /// delete the user's bookmark data or cancel another engine's work.
    pub fn wait_for_pending_saves(&mut self) {
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
    pub fn take_save_diagnostics(&self) -> Vec<String> {
        std::mem::take(&mut self.pending.lock().unwrap().diagnostics)
    }
}
impl Drop for Bookmarks {
    fn drop(&mut self) {
        self.wait_for_pending_saves();
    }
}
