//! No MIT crate of the workspace builds in a GPL one, however indirectly: `kicad-text` (KiCad's
//! font, GPL) is a program the reader runs, never a library it links.

use std::collections::{HashMap, HashSet};

#[test]
fn no_mit_crate_depends_on_a_gpl_one() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml");
    let out = std::process::Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--offline", "--manifest-path", manifest])
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids = |v: &serde_json::Value| v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect::<Vec<_>>();
    let members: HashSet<String> = ids(&meta["workspace_members"]).into_iter().collect();
    let license: HashMap<&str, (&str, &str)> = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), (p["name"].as_str().unwrap(), p["license"].as_str().unwrap_or(""))))
        .collect();
    let gpl: HashSet<&str> = members.iter().map(String::as_str).filter(|id| license[id].1.contains("GPL")).collect();
    assert!(gpl.iter().any(|id| license[id].0 == "kicad-text"), "kicad-text is the workspace's GPL crate");
    let deps: HashMap<&str, Vec<&str>> = meta["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n["deps"].as_array().unwrap().iter().map(|d| d["pkg"].as_str().unwrap()).collect()))
        .collect();
    for mit in members.iter().map(String::as_str).filter(|id| !gpl.contains(id)) {
        // Every dependency of every kind (normal, build, dev), all the way down.
        let (mut seen, mut todo) = (HashSet::new(), vec![mit]);
        while let Some(id) = todo.pop() {
            for &d in deps.get(id).into_iter().flatten() {
                assert!(!gpl.contains(d), "{} ({}) reaches {} ({}) through {}", license[mit].0, license[mit].1, license[d].0, license[d].1, license[id].0);
                if seen.insert(d) {
                    todo.push(d);
                }
            }
        }
    }
}
