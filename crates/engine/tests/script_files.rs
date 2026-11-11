use mnemosyne_engine::script_files::{FileObjects, Stat};
use mnemosyne_runtime_protocol::{HostDescriptorKind, HostObjectId, HostValue};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn units(value: &Value) -> String {
    String::from_utf16(&serde_json::from_value::<Vec<u16>>(value.clone()).unwrap()).unwrap()
}
fn own(snapshot: &Value, key: &str) -> Value {
    snapshot["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| units(&row["key"]) == key)
        .unwrap()["value"]
        .clone()
}
fn stat(snapshot: &Value) -> Stat {
    Stat {
        ctime: own(snapshot, "ctime")["value"].as_f64().unwrap(),
        mtime: own(snapshot, "mtime")["value"].as_f64().unwrap(),
        size: own(snapshot, "size")["value"].as_f64().unwrap(),
    }
}
fn object_id(value: &HostValue) -> HostObjectId {
    match value {
        HostValue::HostObject { id, .. } => *id,
        _ => panic!("not an object"),
    }
}
fn property(graph: &FileObjects, id: HostObjectId, key: &str) -> HostValue {
    graph
        .own_descriptors(id)
        .unwrap()
        .into_iter()
        .find_map(|row| {
            if row.key == key.into() {
                match row.kind {
                    HostDescriptorKind::Data { value, .. } => Some(value),
                    _ => None,
                }
            } else {
                None
            }
        })
        .unwrap()
}
fn observed(value: HostValue, ids: &BTreeMap<u64, u64>) -> Value {
    match value {
        HostValue::Null => json!({"type":"null"}),
        HostValue::Boolean(value) => json!({"type":"boolean","value":value}),
        HostValue::Number(value) => json!({"type":"number","value":value}),
        HostValue::Utf16(units) => json!({"type":"string","units":units}),
        HostValue::HostObject { id, class } => {
            json!({"type":"object","id":ids[&id.0],"array":String::from_utf16(&class).unwrap()=="Array"})
        }
        other => panic!("unexpected own value {other:?}"),
    }
}
fn compare(graph: &FileObjects, id: HostObjectId, snapshot: &Value, ids: &BTreeMap<u64, u64>) {
    let descriptors = graph.own_descriptors(id).unwrap();
    let actual = descriptors
        .into_iter()
        .map(|row| {
            let HostDescriptorKind::Data { value, writable } = row.kind else {
                panic!("not data")
            };
            let mnemosyne_runtime_protocol::HostKey::Utf16(key) = row.key else {
                panic!("not string key")
            };
            json!({"key":key,"enumerable":row.enumerable,"configurable":row.configurable,
            "writable":writable,"value":observed(value,ids)})
        })
        .collect::<Vec<_>>();
    let mut expected = snapshot["own"].clone();
    for row in expected.as_array_mut().unwrap() {
        if row["value"]["type"] == "number" {
            // Native JSON.stringify writes integral JS numbers without `.0`;
            // the IPC representation is an IEEE-754 double in both cases.
            row["value"]["value"] = json!(row["value"]["value"].as_f64().unwrap());
        }
    }
    assert_eq!(json!(actual), expected);
}
#[test]
fn retained_file_descriptors_and_lifecycle_match_native_sdk_instances() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/plugin_api_instances.json")).unwrap();
    let inventory = &fixture["instances"]["inventory"];
    let mut graph = FileObjects::new(HostObjectId(1), 2);
    let initial_stat = stat(&inventory["stat"]);
    for file in fixture["instances"]["methods"]["loadedFiles"]
        .as_array()
        .unwrap()
    {
        let path = file["path"].as_str().unwrap();
        if path == "/" {
            continue;
        }
        if fixture["initial_files"].get(path).is_some() {
            graph.insert_file(path, initial_stat).unwrap();
        } else {
            graph.insert_folder(path).unwrap();
        }
    }
    let file = graph.lookup_file("Root.md").unwrap();
    let folder = graph.lookup_folder("Folder").unwrap();
    let old_stat = object_id(&property(&graph, file, "stat"));
    let children = object_id(&property(&graph, folder, "children"));
    let mut ids = BTreeMap::from([(1, own(&inventory["file"], "vault")["id"].as_u64().unwrap())]);
    for item in fixture["instances"]["methods"]["loadedFiles"]
        .as_array()
        .unwrap()
    {
        ids.insert(
            graph.lookup(item["path"].as_str().unwrap()).unwrap().0,
            item["identity"]["id"].as_u64().unwrap(),
        );
    }
    ids.insert(
        old_stat.0,
        inventory["stat"]["identity"]["id"].as_u64().unwrap(),
    );
    ids.insert(
        children.0,
        inventory["children"]["identity"]["id"].as_u64().unwrap(),
    );
    ids.insert(
        object_id(&property(&graph, graph.root(), "children")).0,
        own(&inventory["root"], "children")["id"].as_u64().unwrap(),
    );
    for (id, key) in [
        (file, "file"),
        (folder, "folder"),
        (graph.root(), "root"),
        (old_stat, "stat"),
        (children, "children"),
    ] {
        compare(&graph, id, &inventory[key], &ids);
    }
    for row in fixture["instances"]["methods"]["lookups"]
        .as_array()
        .unwrap()
    {
        let path = row["path"].as_str().unwrap();
        for (kind, found) in [
            ("abstract", graph.lookup(path)),
            ("file", graph.lookup_file(path)),
            ("folder", graph.lookup_folder(path)),
        ] {
            let value = found
                .map(|id| graph.value(id).unwrap())
                .unwrap_or(HostValue::Null);
            assert_eq!(observed(value, &ids), row[kind], "{path:?} {kind}");
        }
    }
    let methods = &fixture["instances"]["methods"];
    for (id, method, expected) in [
        (file, "getShortName", "fileShortName"),
        (file, "toString", "fileToString"),
        (folder, "isRoot", "folderIsRoot"),
        (graph.root(), "isRoot", "rootIsRoot"),
        (folder, "getFileCount", "folderFileCount"),
        (folder, "getFolderCount", "folderFolderCount"),
        (graph.root(), "getFileCount", "rootFileCount"),
        (graph.root(), "getFolderCount", "rootFolderCount"),
    ] {
        match graph.call(id, method).unwrap() {
            HostValue::Utf16(units) => assert_eq!(
                String::from_utf16(&units).unwrap(),
                methods[expected].as_str().unwrap()
            ),
            HostValue::Boolean(value) => assert_eq!(value, methods[expected].as_bool().unwrap()),
            HostValue::Number(value) => assert_eq!(value, methods[expected].as_f64().unwrap()),
            other => panic!("unexpected method result {other:?}"),
        }
    }
    // The native receipt verifies replacement identity and retained old values.
    // New stat fields below are authored notification inputs, not a claim about
    // native dynamic filesystem timestamps.
    graph
        .modified(
            file,
            Stat {
                ctime: initial_stat.ctime,
                mtime: initial_stat.mtime + 1.,
                size: 8.,
            },
        )
        .unwrap();
    let new_stat = object_id(&property(&graph, file, "stat"));
    assert_ne!(new_stat, old_stat);
    ids.insert(
        new_stat.0,
        own(&fixture["modify"]["file"], "stat")["id"]
            .as_u64()
            .unwrap(),
    );
    compare(&graph, file, &fixture["modify"]["file"], &ids);
    compare(&graph, old_stat, &fixture["modify"]["oldStat"], &ids);
    assert_eq!(object_id(&property(&graph, folder, "children")), children);
    graph.renamed_file(file, "Folder/Renamed.md").unwrap();
    assert_eq!(graph.lookup_file("Folder/Renamed.md"), Some(file));
    assert_eq!(graph.lookup_file("Root.md"), None);
    compare(&graph, file, &fixture["rename"]["file"], &ids);
    graph.deleted_file(file).unwrap();
    assert_eq!(graph.lookup_file("Folder/Renamed.md"), None);
    compare(&graph, file, &fixture["delete"]["file"], &ids);
    assert_eq!(object_id(&property(&graph, folder, "children")), children);
    // A retained deleted wrapper cannot erase a newly allocated live object.
    let replacement = graph
        .insert_file("Folder/Renamed.md", initial_stat)
        .unwrap();
    assert_ne!(replacement, file);
    graph.deleted_file(file).unwrap();
    assert_eq!(graph.lookup_file("Folder/Renamed.md"), Some(replacement));
}
