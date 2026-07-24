//! The pure N-dimensional space model.
//!
//! A space is a sparse coordinate system: named dimensions, cells addressed by
//! coordinate tuples. A cell addressed by a subset of the dimensions applies to
//! the whole subspace it spans. Every mutation is a stamped `Op`; `apply` is
//! idempotent and order-convergent (LWW per cell point by Lamport time, writer
//! id as tie-break), so replicas that see the same op set reach the same state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The built-in representation dimension: the shared plane lives here.
pub const REPRESENTATION: &str = "representation";
/// The coordinate on the representation axis where source data lives.
pub const RAW: &str = "raw";

/// A coordinate tuple: dimension name -> coordinate key. Partial tuples are
/// legal and span the subspace of every unmentioned dimension.
pub type Coords = BTreeMap<String, String>;

/// A typed link to data living anywhere on ce-net. The core never interprets
/// the payload; converters do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ref {
    /// blob | drive | db | twin | cap | topic | url
    pub scheme: String,
    /// Scheme-specific address, e.g. the CID for `blob`.
    pub address: String,
    /// Optional media/content hint, e.g. "video/mp4".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hint: Option<String>,
    /// Optional pinned content-addressed snapshot for reproducible comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_cid: Option<String>,
}

impl Ref {
    /// Parse `scheme:address` (e.g. `blob:ab12..`, `twin:node/kitchen/temp`).
    pub fn parse(s: &str) -> Option<Ref> {
        let (scheme, address) = s.split_once(':')?;
        if scheme.is_empty() || address.is_empty() {
            return None;
        }
        Some(Ref {
            scheme: scheme.to_string(),
            address: address.to_string(),
            content_hint: None,
            snapshot_cid: None,
        })
    }

    pub fn uri(&self) -> String {
        format!("{}:{}", self.scheme, self.address)
    }
}

/// What a cell holds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Value {
    Text { text: String },
    Number { number: f64 },
    Bool { bool: bool },
    /// Milliseconds since the Unix epoch.
    Timestamp { ms: i64 },
    Json { json: serde_json::Value },
    /// A vector on the embedding plane.
    Vector { vector: Vec<f32> },
    Ref { r: Ref },
}

/// A named axis of a space.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dimension {
    pub name: String,
    /// Free-form kind hint: "string" | "number" | "time" | "node" | ...
    pub kind: String,
    /// Explicitly declared coordinates, in declaration order. Coordinates
    /// also exist implicitly by appearing in a cell address.
    pub coords: Vec<String>,
}

/// A derivation rule: project a slice at one representation onto another via
/// a converter capability located on the mesh.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// Dimensions fixed on the source slice (representation is implied `raw`
    /// unless present here).
    pub source: Coords,
    /// Converter capability name, e.g. "grid.convert.text".
    pub converter: String,
    /// Target coordinate on the representation axis, e.g. "text".
    pub target_representation: String,
    /// Free-form converter parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// One stored cell with its LWW stamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub coords: Coords,
    /// None = tombstone (cleared cell; kept for convergence).
    pub value: Option<Value>,
    pub lamport: u64,
    pub writer: String,
}

/// Provenance for a derived cell: which inputs produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Derivation {
    pub rule_id: String,
    pub converter: String,
    /// Content hash of (input, converter, params) — the memoization key.
    pub input_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Space {
    pub name: String,
    pub dimensions: BTreeMap<String, Dimension>,
    /// Canonical cell key -> cell.
    pub cells: BTreeMap<String, Cell>,
    pub rules: BTreeMap<String, Rule>,
    /// Cell key -> provenance, for cells written by derivation.
    pub derived: BTreeMap<String, Derivation>,
}

/// A mutation. Every op is stamped (lamport, writer) by the daemon before it
/// enters the replicated log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    CreateSpace { space: String },
    AddDimension { space: String, name: String, kind: String },
    AddCoordinate { space: String, dimension: String, key: String },
    SetCell { space: String, coords: Coords, value: Value },
    ClearCell { space: String, coords: Coords },
    DefineRule { space: String, rule: Rule },
    SetDerived { space: String, coords: Coords, value: Value, derivation: Derivation },
}

/// A stamped op as it appears in the replicated log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stamped {
    pub lamport: u64,
    pub writer: String,
    pub inner: Op,
}

/// The whole replicated state: all spaces on this log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SpaceMachine {
    pub spaces: BTreeMap<String, Space>,
    /// Highest lamport observed (for stamping the next local op).
    pub clock: u64,
}

/// Canonical key for a coordinate tuple: `dim=key` pairs joined by `\x1f`,
/// sorted by dimension name (BTreeMap iteration order).
pub fn cell_key(coords: &Coords) -> String {
    let mut parts = Vec::with_capacity(coords.len());
    for (d, k) in coords {
        parts.push(format!("{d}={k}"));
    }
    parts.join("\u{1f}")
}

impl SpaceMachine {
    /// Stamp a local op: true Lamport (max(local, observed) + 1).
    pub fn stamp(&mut self, writer: &str, inner: Op) -> Stamped {
        self.clock += 1;
        Stamped { lamport: self.clock, writer: writer.to_string(), inner }
    }

    /// Apply one stamped op. Idempotent; order-convergent per cell.
    pub fn apply(&mut self, op: &Stamped) {
        if op.lamport > self.clock {
            self.clock = op.lamport;
        }
        match &op.inner {
            Op::CreateSpace { space } => {
                self.spaces.entry(space.clone()).or_insert_with(|| {
                    let mut s = Space { name: space.clone(), ..Space::default() };
                    // Every space carries the representation axis from birth.
                    s.dimensions.insert(
                        REPRESENTATION.to_string(),
                        Dimension {
                            name: REPRESENTATION.to_string(),
                            kind: "string".to_string(),
                            coords: vec![RAW.to_string()],
                        },
                    );
                    s
                });
            }
            Op::AddDimension { space, name, kind } => {
                if let Some(s) = self.spaces.get_mut(space) {
                    s.dimensions.entry(name.clone()).or_insert_with(|| Dimension {
                        name: name.clone(),
                        kind: kind.clone(),
                        coords: Vec::new(),
                    });
                }
            }
            Op::AddCoordinate { space, dimension, key } => {
                if let Some(d) =
                    self.spaces.get_mut(space).and_then(|s| s.dimensions.get_mut(dimension))
                {
                    if !d.coords.contains(key) {
                        d.coords.push(key.clone());
                    }
                }
            }
            Op::SetCell { space, coords, value } => {
                self.write_cell(space, coords, Some(value.clone()), op, None);
            }
            Op::ClearCell { space, coords } => {
                self.write_cell(space, coords, None, op, None);
            }
            Op::DefineRule { space, rule } => {
                if let Some(s) = self.spaces.get_mut(space) {
                    // Rules are LWW by id via op order on the same log; last
                    // definition observed with the highest stamp wins.
                    s.rules.insert(rule.id.clone(), rule.clone());
                }
            }
            Op::SetDerived { space, coords, value, derivation } => {
                self.write_cell(space, coords, Some(value.clone()), op, Some(derivation.clone()));
            }
        }
    }

    fn write_cell(
        &mut self,
        space: &str,
        coords: &Coords,
        value: Option<Value>,
        op: &Stamped,
        derivation: Option<Derivation>,
    ) {
        let Some(s) = self.spaces.get_mut(space) else { return };
        // Auto-register coordinates observed in addresses.
        for (dim, key) in coords {
            let d = s.dimensions.entry(dim.clone()).or_insert_with(|| Dimension {
                name: dim.clone(),
                kind: "string".to_string(),
                coords: Vec::new(),
            });
            if !d.coords.contains(key) {
                d.coords.push(key.clone());
            }
        }
        let k = cell_key(coords);
        let newer = match s.cells.get(&k) {
            Some(existing) => {
                (op.lamport, op.writer.as_str()) > (existing.lamport, existing.writer.as_str())
            }
            None => true,
        };
        if newer {
            s.cells.insert(
                k.clone(),
                Cell {
                    coords: coords.clone(),
                    value,
                    lamport: op.lamport,
                    writer: op.writer.clone(),
                },
            );
            match derivation {
                Some(d) => {
                    s.derived.insert(k, d);
                }
                None => {
                    s.derived.remove(&k);
                }
            }
        }
    }

    /// Read a slice: all live cells compatible with `fixed`. A cell is
    /// compatible when, for every dimension in `fixed`, the cell either pins
    /// the same coordinate or does not mention the dimension at all (subspace
    /// spanning).
    pub fn slice<'a>(&'a self, space: &str, fixed: &Coords) -> Vec<&'a Cell> {
        let Some(s) = self.spaces.get(space) else { return Vec::new() };
        s.cells
            .values()
            .filter(|c| c.value.is_some())
            .filter(|c| {
                fixed.iter().all(|(dim, key)| match c.coords.get(dim) {
                    Some(k) => k == key,
                    None => true,
                })
            })
            .collect()
    }

    /// Resolve the value at a full point: the most specific live cell whose
    /// coordinates are a subset of `at` and agree with it.
    pub fn get<'a>(&'a self, space: &str, at: &Coords) -> Option<&'a Cell> {
        let Some(s) = self.spaces.get(space) else { return None };
        s.cells
            .values()
            .filter(|c| c.value.is_some())
            .filter(|c| c.coords.iter().all(|(dim, key)| at.get(dim) == Some(key)))
            .max_by_key(|c| (c.coords.len(), c.lamport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coords(pairs: &[(&str, &str)]) -> Coords {
        pairs.iter().map(|(d, k)| (d.to_string(), k.to_string())).collect()
    }

    fn text(t: &str) -> Value {
        Value::Text { text: t.to_string() }
    }

    fn machine_with_space(name: &str) -> SpaceMachine {
        let mut m = SpaceMachine::default();
        let op = m.stamp("a", Op::CreateSpace { space: name.into() });
        m.apply(&op);
        m
    }

    #[test]
    fn create_space_has_representation_axis() {
        let m = machine_with_space("s");
        let s = &m.spaces["s"];
        assert_eq!(s.dimensions[REPRESENTATION].coords, vec![RAW.to_string()]);
    }

    #[test]
    fn set_and_get_full_point() {
        let mut m = machine_with_space("s");
        let at = coords(&[("document", "d1"), ("representation", "raw")]);
        let op = m.stamp("a", Op::SetCell { space: "s".into(), coords: at.clone(), value: text("hello") });
        m.apply(&op);
        let c = m.get("s", &at).expect("cell");
        assert_eq!(c.value, Some(text("hello")));
    }

    #[test]
    fn partial_tuple_spans_subspace() {
        let mut m = machine_with_space("s");
        // Policy text exists once, addressed only by policy.
        let policy_cell = coords(&[("policy", "gdpr")]);
        let op = m.stamp("a", Op::SetCell { space: "s".into(), coords: policy_cell, value: text("retain nothing") });
        m.apply(&op);
        // It resolves at any point that pins policy=gdpr plus more dims.
        let at = coords(&[("policy", "gdpr"), ("document", "d42"), ("representation", "raw")]);
        let c = m.get("s", &at).expect("spanning cell");
        assert_eq!(c.value, Some(text("retain nothing")));
        // And it is NOT visible where policy differs.
        let other = coords(&[("policy", "ccpa"), ("document", "d42")]);
        assert!(m.get("s", &other).is_none());
    }

    #[test]
    fn more_specific_cell_wins_at_a_point() {
        let mut m = machine_with_space("s");
        let broad = m.stamp("a", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("policy", "gdpr")]),
            value: text("default"),
        });
        m.apply(&broad);
        let narrow = m.stamp("a", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("policy", "gdpr"), ("document", "d1")]),
            value: text("override"),
        });
        m.apply(&narrow);
        let at = coords(&[("policy", "gdpr"), ("document", "d1")]);
        assert_eq!(m.get("s", &at).unwrap().value, Some(text("override")));
        // The broad cell still answers for other documents.
        let elsewhere = coords(&[("policy", "gdpr"), ("document", "d2")]);
        assert_eq!(m.get("s", &elsewhere).unwrap().value, Some(text("default")));
    }

    #[test]
    fn slice_filters_by_fixed_dims() {
        let mut m = machine_with_space("s");
        for (doc, rep, v) in [("d1", "raw", "a"), ("d1", "text", "b"), ("d2", "raw", "c")] {
            let op = m.stamp("a", Op::SetCell {
                space: "s".into(),
                coords: coords(&[("document", doc), ("representation", rep)]),
                value: text(v),
            });
            m.apply(&op);
        }
        let raws = m.slice("s", &coords(&[("representation", "raw")]));
        assert_eq!(raws.len(), 2);
        let d1 = m.slice("s", &coords(&[("document", "d1")]));
        assert_eq!(d1.len(), 2);
    }

    #[test]
    fn lww_converges_regardless_of_order() {
        let mut m1 = machine_with_space("s");
        let mut m2 = machine_with_space("s");
        let at = coords(&[("k", "x")]);
        let op_a = Stamped {
            lamport: 5,
            writer: "a".into(),
            inner: Op::SetCell { space: "s".into(), coords: at.clone(), value: text("from-a") },
        };
        let op_b = Stamped {
            lamport: 7,
            writer: "b".into(),
            inner: Op::SetCell { space: "s".into(), coords: at.clone(), value: text("from-b") },
        };
        m1.apply(&op_a);
        m1.apply(&op_b);
        m2.apply(&op_b);
        m2.apply(&op_a);
        assert_eq!(m1.spaces, m2.spaces);
        assert_eq!(m1.get("s", &at).unwrap().value, Some(text("from-b")));
    }

    #[test]
    fn lww_tiebreak_by_writer_is_deterministic() {
        let mut m1 = machine_with_space("s");
        let mut m2 = machine_with_space("s");
        let at = coords(&[("k", "x")]);
        let op_a = Stamped {
            lamport: 5,
            writer: "a".into(),
            inner: Op::SetCell { space: "s".into(), coords: at.clone(), value: text("from-a") },
        };
        let op_b = Stamped {
            lamport: 5,
            writer: "b".into(),
            inner: Op::SetCell { space: "s".into(), coords: at.clone(), value: text("from-b") },
        };
        m1.apply(&op_a);
        m1.apply(&op_b);
        m2.apply(&op_b);
        m2.apply(&op_a);
        assert_eq!(m1.spaces, m2.spaces);
        // Higher writer id wins the tie.
        assert_eq!(m1.get("s", &at).unwrap().value, Some(text("from-b")));
    }

    #[test]
    fn apply_is_idempotent() {
        let mut m = machine_with_space("s");
        let op = m.stamp("a", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("k", "x")]),
            value: text("v"),
        });
        m.apply(&op);
        let snapshot = m.clone();
        m.apply(&op);
        assert_eq!(m, snapshot);
    }

    #[test]
    fn clear_cell_tombstones() {
        let mut m = machine_with_space("s");
        let at = coords(&[("k", "x")]);
        let set = m.stamp("a", Op::SetCell { space: "s".into(), coords: at.clone(), value: text("v") });
        m.apply(&set);
        let clear = m.stamp("a", Op::ClearCell { space: "s".into(), coords: at.clone() });
        m.apply(&clear);
        assert!(m.get("s", &at).is_none());
        assert!(m.slice("s", &Coords::new()).is_empty());
        // Late-arriving older write does not resurrect it.
        let old = Stamped {
            lamport: 1,
            writer: "z".into(),
            inner: Op::SetCell { space: "s".into(), coords: at.clone(), value: text("stale") },
        };
        m.apply(&old);
        assert!(m.get("s", &at).is_none());
    }

    #[test]
    fn coordinates_register_implicitly_and_explicitly() {
        let mut m = machine_with_space("s");
        let add_dim = m.stamp("a", Op::AddDimension { space: "s".into(), name: "doc".into(), kind: "string".into() });
        m.apply(&add_dim);
        let add_coord = m.stamp("a", Op::AddCoordinate { space: "s".into(), dimension: "doc".into(), key: "d1".into() });
        m.apply(&add_coord);
        let set = m.stamp("a", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("doc", "d2")]),
            value: text("v"),
        });
        m.apply(&set);
        assert_eq!(m.spaces["s"].dimensions["doc"].coords, vec!["d1".to_string(), "d2".to_string()]);
    }

    #[test]
    fn derived_cell_carries_provenance() {
        let mut m = machine_with_space("s");
        let at = coords(&[("document", "d1"), ("representation", "text")]);
        let op = m.stamp("conv", Op::SetDerived {
            space: "s".into(),
            coords: at.clone(),
            value: text("plain rendering"),
            derivation: Derivation {
                rule_id: "r1".into(),
                converter: "grid.convert.text".into(),
                input_key: "abc123".into(),
            },
        });
        m.apply(&op);
        let k = cell_key(&at);
        assert_eq!(m.spaces["s"].derived[&k].converter, "grid.convert.text");
    }

    #[test]
    fn ref_parse_roundtrip() {
        let r = Ref::parse("blob:ab12cd").unwrap();
        assert_eq!(r.scheme, "blob");
        assert_eq!(r.address, "ab12cd");
        assert_eq!(r.uri(), "blob:ab12cd");
        assert!(Ref::parse("noscheme").is_none());
        // twin refs keep their full path address
        let t = Ref::parse("twin:node1/kitchen/temp").unwrap();
        assert_eq!(t.address, "node1/kitchen/temp");
    }

    #[test]
    fn stamp_advances_past_observed_remote_clock() {
        let mut m = machine_with_space("s");
        let remote = Stamped {
            lamport: 99,
            writer: "remote".into(),
            inner: Op::CreateSpace { space: "other".into() },
        };
        m.apply(&remote);
        let local = m.stamp("me", Op::CreateSpace { space: "third".into() });
        assert_eq!(local.lamport, 100);
    }
}
