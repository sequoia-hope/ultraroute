use serde_json::json;
use wasm_bindgen::prelude::*;

use crate::Scene;

#[wasm_bindgen]
pub struct Studio {
    scene: Scene,
}

#[wasm_bindgen]
impl Studio {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Studio {
        Studio { scene: Scene::default() }
    }

    pub fn apply(&mut self, line: &str) -> bool {
        self.scene.apply(line)
    }

    pub fn mesh(&self, px: f32) -> Vec<f32> {
        self.scene.mesh(px)
    }

    /// The mesh for the layers whose bits are set in `mask`, at time `now` in milliseconds.
    pub fn mesh_view(&self, px: f32, mask: u32, now: f64) -> Vec<f32> {
        self.scene.mesh_with(px, u64::from(mask), now)
    }

    /// Events after the first `seen` ever, as JSON: `{"total": n, "events": [...]}`.
    pub fn events_since(&self, seen: f64) -> String {
        json!({"total": self.scene.event_total, "events": self.scene.events_since(seen as u64)}).to_string()
    }

    pub fn animating(&self, now: f64) -> bool {
        self.scene.animating(now)
    }

    pub fn set_active_layer(&mut self, layer: usize) {
        self.scene.active_layer = layer.min(self.scene.layers.len().saturating_sub(1));
    }

    pub fn set_high_contrast(&mut self, on: bool) {
        self.scene.high_contrast = on;
    }

    pub fn set_show_pours(&mut self, on: bool) {
        self.scene.show_pours = on;
    }

    pub fn set_highlight(&mut self, parts: Vec<i32>) {
        self.scene.highlight = parts;
    }

    pub fn bbox(&self) -> Vec<f32> {
        self.scene.bbox.to_vec()
    }

    pub fn part_at(&self, x: f32, y: f32) -> i32 {
        self.scene.part_at(x, y).unwrap_or(0)
    }

    pub fn net_at(&self, x: f32, y: f32, reach: f32) -> i32 {
        self.scene.net_at(x, y, reach).unwrap_or(0)
    }

    pub fn net_name(&self, net: i32) -> String {
        self.scene.nets.get(&net).cloned().unwrap_or_default()
    }

    pub fn set_hot_net(&mut self, net: i32) {
        self.scene.hot_net = (net != 0).then_some(net);
    }

    pub fn set_picked(&mut self, part: i32) {
        self.scene.picked = (part != 0).then_some(part);
    }

    pub fn set_offset(&mut self, part: i32, dx: f32, dy: f32) {
        self.scene.offsets.insert(part, [dx, dy]);
    }

    pub fn clear_offsets(&mut self) {
        self.scene.offsets.clear();
    }

    pub fn set_layer_hidden(&mut self, layer: usize, hidden: bool) {
        if let Some(h) = self.scene.hidden.get_mut(layer) {
            *h = hidden;
        }
    }

    pub fn set_selected(&mut self, part: i32) {
        self.scene.selected = (part != 0).then_some(part);
    }

    pub fn set_congestion(&mut self, shown: bool) {
        self.scene.show_congestion = shown;
    }

    pub fn set_locked(&mut self, part: i32, locked: bool) {
        self.scene.lock_overrides.insert(part, locked);
    }

    pub fn clear_lock_overrides(&mut self) {
        self.scene.lock_overrides.clear();
    }

    /// Every part as JSON: id, name, origin, and whether it is locked.
    pub fn parts(&self) -> String {
        let s = &self.scene;
        let parts: Vec<_> = s
            .parts
            .values()
            .map(|p| json!({"id": p.id, "name": p.name, "at": p.at, "locked": s.is_locked(p.id), "fixed": p.fixed}))
            .collect();
        serde_json::Value::Array(parts).to_string()
    }

    /// The fullest cells and the parts nearest them, as JSON.
    pub fn congestion(&self) -> String {
        let s = &self.scene;
        let c = s.congestion();
        let mut cells: Vec<(usize, f32)> = c.values.iter().copied().enumerate().filter(|(_, v)| *v > 0.0).collect();
        let used = cells.len().max(1);
        let mean = cells.iter().map(|(_, v)| v).sum::<f32>() / used as f32;
        let over = cells.iter().filter(|(_, v)| *v >= 0.8).count();
        cells.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut near: Vec<String> = Vec::new();
        for (i, v) in &cells {
            if near.len() == 5 || *v < 0.8 {
                break;
            }
            let x = c.x0 + ((i % c.cols) as f32 + 0.5) * crate::CELL_MM;
            let y = c.y0 + ((i / c.cols) as f32 + 0.5) * crate::CELL_MM;
            if let Some(p) = s.nearest_part(x, y)
                && !near.contains(&p.name)
            {
                near.push(p.name.clone());
            }
        }
        json!({
            "peak": cells.first().map_or(0.0, |c| c.1),
            "mean": mean,
            "over": over,
            "cells": used,
            "near": near,
        })
        .to_string()
    }

    /// Layers, parts and counters, as JSON.
    pub fn status(&self) -> String {
        let s = &self.scene;
        let (traces, vias) = s.counts();
        let layers: Vec<_> = s
            .layers
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let c = s.layer_color(i);
                json!({"name": name, "hidden": s.hidden.get(i).copied().unwrap_or(false), "color": [c[0], c[1], c[2]], "active": i == s.active_layer})
            })
            .collect();
        json!({
            "seq": s.seq,
            "ms": s.ms,
            "open": s.open,
            "traces": traces,
            "vias": vias,
            "probes": s.probes.len(),
            "layers": layers,
            "done": s.done,
            "focus": s.focus.as_ref().map(|f| f.phase().to_string()),
            "high_contrast": s.high_contrast,
            "pours": s.show_pours,
            "events": s.events.iter().rev().take(12).collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// The part's id, name, origin and whether it is fixed, as JSON, or an empty string.
    pub fn part(&self, part: i32) -> String {
        self.scene
            .parts
            .get(&part)
            .map(|p| json!({"id": p.id, "name": p.name, "at": p.at, "fixed": p.fixed}).to_string())
            .unwrap_or_default()
    }
}

impl Default for Studio {
    fn default() -> Self {
        Self::new()
    }
}
