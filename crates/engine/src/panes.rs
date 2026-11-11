//! Nonvisual workspace topology. IDs identify state nodes; dimensions and other
//! imported visual fields are retained as opaque data, without rendering them.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    #[default]
    Vertical,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PaneRequest {
    #[default]
    Reuse,
    Tab,
    Split(SplitDirection),
    Window,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayoutNode {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<LayoutNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<SplitDirection>,
    #[serde(default, rename = "currentTab", skip_serializing_if = "is_zero")]
    pub current_tab: usize,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
fn is_zero(value: &usize) -> bool {
    *value == 0
}
impl LayoutNode {
    pub(crate) fn new(id: String, kind: &str) -> Self {
        Self {
            id,
            kind: kind.into(),
            children: Vec::new(),
            direction: None,
            current_tab: 0,
            extra: Map::new(),
        }
    }
    pub(crate) fn contains(&self, id: &str) -> bool {
        self.id == id || self.children.iter().any(|child| child.contains(id))
    }
    pub(crate) fn parent_mut(&mut self, id: &str) -> Option<&mut Self> {
        if self.children.iter().any(|child| child.id == id) {
            return Some(self);
        }
        self.children
            .iter_mut()
            .find_map(|child| child.parent_mut(id))
    }
    pub(crate) fn group(&self, leaf: &str) -> Option<&Self> {
        if self.kind == "tabs" && self.children.iter().any(|child| child.id == leaf) {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.group(leaf))
    }
    pub(crate) fn group_mut(&mut self, leaf: &str) -> Option<&mut Self> {
        if self.kind == "tabs" && self.children.iter().any(|child| child.id == leaf) {
            return Some(self);
        }
        self.children
            .iter_mut()
            .find_map(|child| child.group_mut(leaf))
    }
    pub(crate) fn leaf_ids(&self, output: &mut Vec<String>) {
        if self.kind == "leaf" {
            output.push(self.id.clone());
        }
        for child in &self.children {
            child.leaf_ids(output);
        }
    }
    pub(crate) fn visible_ids(&self, output: &mut Vec<String>) {
        if self.kind == "tabs" {
            if self.extra.get("stacked").and_then(Value::as_bool) == Some(true) {
                for child in &self.children {
                    child.leaf_ids(output);
                }
            } else if let Some(child) = self.children.get(self.current_tab) {
                child.leaf_ids(output);
            }
        } else if self.kind == "leaf" {
            output.push(self.id.clone());
        } else {
            for child in &self.children {
                child.visible_ids(output);
            }
        }
    }
    pub(crate) fn select(&mut self, id: &str) {
        if self.kind == "tabs" {
            if let Some(index) = self.children.iter().position(|child| child.id == id) {
                self.current_tab = index;
            }
        }
        for child in &mut self.children {
            child.select(id);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceLayout {
    pub main: LayoutNode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating: Option<LayoutNode>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
impl WorkspaceLayout {
    fn container(&self, leaf: Option<&str>) -> &LayoutNode {
        if let Some(window) = leaf.and_then(|id| {
            self.floating
                .as_ref()?
                .children
                .iter()
                .find(|window| window.contains(id))
        }) {
            window
        } else {
            &self.main
        }
    }
    pub(crate) fn group(&self, leaf: &str) -> Option<&LayoutNode> {
        self.main
            .group(leaf)
            .or_else(|| self.floating.as_ref()?.group(leaf))
    }
    pub(crate) fn group_mut(&mut self, leaf: &str) -> Option<&mut LayoutNode> {
        if self.main.contains(leaf) {
            self.main.group_mut(leaf)
        } else {
            self.floating.as_mut()?.group_mut(leaf)
        }
    }
    pub(crate) fn parent_mut(&mut self, id: &str) -> Option<&mut LayoutNode> {
        if self.main.contains(id) {
            self.main.parent_mut(id)
        } else {
            self.floating.as_mut()?.parent_mut(id)
        }
    }
    pub(crate) fn leaf_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        self.main.leaf_ids(&mut ids);
        if let Some(floating) = &self.floating {
            floating.leaf_ids(&mut ids);
        }
        ids
    }
    pub(crate) fn visible_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        self.main.visible_ids(&mut ids);
        if let Some(floating) = &self.floating {
            floating.visible_ids(&mut ids);
        }
        ids
    }
    pub(crate) fn select(&mut self, id: &str) {
        self.main.select(id);
        if let Some(floating) = &mut self.floating {
            floating.select(id);
        }
    }
}

// PS starts navigation=false; Empty and FileView set true. Main/floating
// Graph, release notes, browser and Bases override true explicitly. Auxiliary
// follow-file views (including local graph) and missing/unknown ItemViews stay
// false. Arbitrary custom registered view overrides require their own backend.
fn can_navigate(leaf: &crate::LeafState) -> bool {
    !leaf.pinned
        && matches!(
            leaf.view_type.as_str(),
            "empty"
                | "markdown"
                | "canvas"
                | "bases"
                | "pdf"
                | "image"
                | "audio"
                | "video"
                | "file"
                | "graph"
                | "release-notes"
                | "webviewer"
        )
}

impl crate::Engine {
    pub(crate) fn next_workspace_id(&mut self) -> String {
        loop {
            let id = format!("mnemosyne-{}", self.session.next_leaf_id);
            self.session.next_leaf_id += 1;
            if !self.session.leaves.iter().any(|leaf| leaf.id == id)
                && !self.session.pane_layout.as_ref().is_some_and(|layout| {
                    layout.main.contains(&id)
                        || layout
                            .floating
                            .as_ref()
                            .is_some_and(|node| node.contains(&id))
                })
            {
                return id;
            }
        }
    }
    pub(crate) fn ensure_pane_layout(&mut self) {
        if self.session.pane_layout.is_some() {
            return;
        }
        let mut group = LayoutNode::new("mnemosyne-tabs".into(), "tabs");
        group.children = self
            .session
            .leaves
            .iter()
            .map(|leaf| LayoutNode::new(leaf.id.clone(), "leaf"))
            .collect();
        group.current_tab = self
            .session
            .leaves
            .iter()
            .position(|leaf| Some(&leaf.id) == self.session.active_leaf.as_ref())
            .unwrap_or(0);
        let mut main = LayoutNode::new("mnemosyne-main".into(), "split");
        main.direction = Some(SplitDirection::Vertical);
        main.children.push(group);
        self.session.pane_layout = Some(WorkspaceLayout {
            main,
            floating: None,
            extra: Map::new(),
        });
    }
    /// Select an attached leaf without changing its editor or file.
    pub fn activate_leaf(&mut self, id: &str) -> Result<(), crate::EngineError> {
        let leaf = self
            .session
            .leaves
            .iter_mut()
            .find(|leaf| leaf.id == id)
            .ok_or_else(|| crate::EngineError::InvalidState("active leaf is missing".into()))?;
        if self.session.active_leaf.as_deref() != Some(id) {
            leaf.active_order = self.session.next_activation;
            self.session.next_activation = self.session.next_activation.saturating_add(1);
        }
        self.session.active_leaf = Some(id.into());
        self.ensure_pane_layout();
        self.session.pane_layout.as_mut().unwrap().select(id);
        Ok(())
    }
    fn most_recent_pane_leaf(&self) -> Option<String> {
        let layout = self.session.pane_layout.as_ref()?;
        let visible = layout.visible_ids();
        let mut newest = None;
        for id in &visible {
            if let Some(leaf) = self.session.leaves.iter().find(|leaf| &leaf.id == id) {
                if newest.as_ref().is_none_or(|previous: &&crate::LeafState| {
                    previous.active_order < leaf.active_order
                }) {
                    newest = Some(leaf);
                }
            }
        }
        newest
            .map(|leaf| leaf.id.clone())
            .or_else(|| layout.leaf_ids().first().cloned())
    }
    /// Source Workspace.getLeaf request mapping, without DOM creation.
    pub fn get_leaf(&mut self, pane: PaneRequest) -> Result<String, crate::EngineError> {
        self.ensure_pane_layout();
        if pane == PaneRequest::Reuse {
            if let Some(active) = self.session.active_leaf.as_ref().and_then(|id| {
                self.session
                    .leaves
                    .iter()
                    .find(|leaf| &leaf.id == id && can_navigate(leaf))
            }) {
                return Ok(active.id.clone());
            }
            let container = self
                .session
                .pane_layout
                .as_ref()
                .unwrap()
                .container(self.session.active_leaf.as_deref());
            let mut visible = Vec::new();
            container.visible_ids(&mut visible);
            let mut newest = None;
            for id in visible {
                if let Some(leaf) = self
                    .session
                    .leaves
                    .iter()
                    .find(|leaf| leaf.id == id && can_navigate(leaf))
                {
                    if newest.as_ref().is_none_or(|previous: &&crate::LeafState| {
                        previous.active_order < leaf.active_order
                    }) {
                        newest = Some(leaf);
                    }
                }
            }
            if let Some(leaf) = newest {
                let id = leaf.id.clone();
                self.activate_leaf(&id)?;
                return Ok(id);
            }
        }
        let recent = if pane == PaneRequest::Reuse {
            let container = self
                .session
                .pane_layout
                .as_ref()
                .unwrap()
                .container(self.session.active_leaf.as_deref());
            let mut visible = Vec::new();
            container.visible_ids(&mut visible);
            let mut newest: Option<&crate::LeafState> = None;
            for id in visible {
                if let Some(leaf) = self.session.leaves.iter().find(|leaf| leaf.id == id) {
                    if newest.is_none_or(|previous| previous.active_order < leaf.active_order) {
                        newest = Some(leaf);
                    }
                }
            }
            newest.map(|leaf| leaf.id.clone()).or_else(|| {
                let mut ids = Vec::new();
                container.leaf_ids(&mut ids);
                ids.first().cloned()
            })
        } else {
            self.most_recent_pane_leaf()
        };
        if pane == PaneRequest::Tab {
            if let Some(group) = recent
                .as_deref()
                .and_then(|id| self.session.pane_layout.as_ref()?.group(id))
            {
                let mut latest = group
                    .children
                    .last()
                    .and_then(|node| self.session.leaves.iter().find(|leaf| leaf.id == node.id));
                for node in &group.children {
                    if let Some(leaf) = self.session.leaves.iter().find(|leaf| leaf.id == node.id) {
                        if latest.is_none_or(|previous| previous.active_order < leaf.active_order) {
                            latest = Some(leaf);
                        }
                    }
                }
                if let Some(leaf) = latest.filter(|leaf| leaf.view_type == "empty") {
                    return Ok(leaf.id.clone());
                }
            }
        }
        let id = self.next_workspace_id();
        let node = LayoutNode::new(id.clone(), "leaf");
        match pane {
            PaneRequest::Tab | PaneRequest::Reuse => {
                let recent = recent.ok_or_else(|| {
                    crate::EngineError::InvalidState("No tab group found.".into())
                })?;
                let layout = self.session.pane_layout.as_mut().unwrap();
                let group = layout.group_mut(&recent).ok_or_else(|| {
                    crate::EngineError::InvalidState("No tab group found.".into())
                })?;
                let latest = group
                    .children
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, node)| {
                        self.session
                            .leaves
                            .iter()
                            .find(|leaf| leaf.id == node.id)
                            .map_or(0, |leaf| leaf.active_order)
                    })
                    .map_or(group.children.len(), |(index, _)| index + 1);
                let index = if pane == PaneRequest::Reuse {
                    group.children.len()
                } else {
                    latest
                };
                group.children.insert(index, node);
            }
            PaneRequest::Split(direction) => {
                let recent = recent.ok_or_else(|| {
                    crate::EngineError::InvalidState("No tab group found.".into())
                })?;
                let group_id = self
                    .session
                    .pane_layout
                    .as_ref()
                    .unwrap()
                    .group(&recent)
                    .ok_or_else(|| crate::EngineError::InvalidState("No tab group found.".into()))?
                    .id
                    .clone();
                let mut group = LayoutNode::new(self.next_workspace_id(), "tabs");
                group.children.push(node);
                let split_id = self.next_workspace_id();
                let parent = self
                    .session
                    .pane_layout
                    .as_mut()
                    .unwrap()
                    .parent_mut(&group_id)
                    .unwrap();
                let index = parent
                    .children
                    .iter()
                    .position(|child| child.id == group_id)
                    .unwrap();
                if parent.direction == Some(direction) {
                    parent.children.insert(index + 1, group);
                } else {
                    let old = parent.children.remove(index);
                    let mut split = LayoutNode::new(split_id, "split");
                    split.direction = Some(direction);
                    split.children = vec![old, group];
                    parent.children.insert(index, split);
                }
            }
            PaneRequest::Window => {
                let mut group = LayoutNode::new(self.next_workspace_id(), "tabs");
                group.children.push(node);
                let mut window = LayoutNode::new(self.next_workspace_id(), "window");
                window.direction = Some(SplitDirection::Vertical);
                window.children.push(group);
                let floating_id = self.next_workspace_id();
                let layout = self.session.pane_layout.as_mut().unwrap();
                let floating = layout
                    .floating
                    .get_or_insert_with(|| LayoutNode::new(floating_id, "floating"));
                floating.children.push(window);
            }
        }
        self.session.leaves.push(crate::LeafState {
            id: id.clone(),
            pinned: false,
            active_order: 0,
            file: None,
            view_type: "empty".into(),
            back_history: Vec::new(),
            markdown: None,
        });
        // Keep the compatibility flat leaf list in native tree traversal order.
        let order = self.session.pane_layout.as_ref().unwrap().leaf_ids();
        self.session.leaves.sort_by_key(|leaf| {
            order
                .iter()
                .position(|id| id == &leaf.id)
                .unwrap_or(usize::MAX)
        });
        if matches!(pane, PaneRequest::Reuse | PaneRequest::Split(_))
            || (pane == PaneRequest::Tab
                && crate::settings::js_truthy(self.get_config("focusNewTab")))
        {
            self.activate_leaf(&id)?;
        }
        Ok(id)
    }
    /// Current workspace state with live view/file fields. Imported visual
    /// fields are retained; new windows carry only nonvisual topology.
    pub fn workspace_layout(&self) -> Value {
        let Some(layout) = &self.session.pane_layout else {
            return Value::Null;
        };
        let mut value = serde_json::to_value(layout).expect("JSON-backed layout serialization");
        fn update(node: &mut Value, engine: &crate::Engine) {
            if node["type"] == "leaf" {
                if let Some(leaf) = node["id"]
                    .as_str()
                    .and_then(|id| engine.session.leaves.iter().find(|leaf| leaf.id == id))
                {
                    let mut state = Map::new();
                    if let Some(path) = &leaf.file {
                        state.insert("file".into(), Value::String(path.clone()));
                    }
                    if let Some(markdown) = &leaf.markdown {
                        state.insert("mode".into(), serde_json::to_value(markdown.mode).unwrap());
                        state.insert("source".into(), Value::Bool(markdown.source));
                        state.insert("backlinks".into(), Value::Bool(markdown.backlinks));
                    }
                    let mut view = serde_json::json!({"type":leaf.view_type,"state":state});
                    if leaf.pinned {
                        view["pinned"] = Value::Bool(true);
                    }
                    node["state"] = view;
                }
            }
            if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
                for child in children {
                    update(child, engine);
                }
            }
        }
        value.as_object_mut().unwrap().remove("lastOpenFiles");
        update(&mut value["main"], self);
        if let Some(floating) = value.get_mut("floating") {
            update(floating, self);
        }
        if let Some(id) = &self.session.active_leaf {
            value["active"] = Value::String(id.clone());
        } else {
            value.as_object_mut().unwrap().remove("active");
        }
        value
    }
}

/// Native deserialize inserts a tab group around a direct leaf in a split.
pub(crate) fn normalize_imported_layout(layout: &mut WorkspaceLayout, next: &mut u64) {
    fn normalize(node: &mut LayoutNode, next: &mut u64) {
        for child in &mut node.children {
            normalize(child, next);
        }
        if matches!(node.kind.as_str(), "split" | "window") {
            for child in &mut node.children {
                if child.kind == "leaf" {
                    let leaf = child.clone();
                    let mut group = LayoutNode::new(format!("mnemosyne-pane-{}", *next), "tabs");
                    *next += 1;
                    group.children.push(leaf);
                    *child = group;
                }
            }
        }
    }
    fn prune(node: &mut LayoutNode) -> bool {
        node.children.retain_mut(prune);
        !matches!(node.kind.as_str(), "split" | "tabs" | "window") || !node.children.is_empty()
    }
    layout.main.children.retain_mut(prune);
    layout.main.direction = Some(SplitDirection::Vertical);
    normalize(&mut layout.main, next);
    if let Some(floating) = &mut layout.floating {
        floating.children.retain_mut(prune);
        normalize(floating, next);
    }
}
