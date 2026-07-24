//! The grid wire protocol: one JSON request/response surface shared by every
//! skin (mesh service on `grid/ctl`, loopback socket for local CLIs, future
//! MCP/frontend consumers). Handling a request = routing it onto the
//! `SpaceMachine`; mutations come back as stamped ops for the caller (the
//! daemon) to append to the replicated log.

use serde::{Deserialize, Serialize};

use crate::space::{cell_key, Cell, Coords, Derivation, Op, Rule, SpaceMachine, Stamped, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// List space names.
    Spaces,
    /// Describe one space: dimensions, coordinates, rules, cell count.
    Describe { space: String },
    CreateSpace { space: String },
    AddDimension { space: String, name: String, #[serde(default = "default_kind")] kind: String },
    AddCoordinate { space: String, dimension: String, key: String },
    SetCell { space: String, coords: Coords, value: Value },
    ClearCell { space: String, coords: Coords },
    /// Resolve the most specific cell at a point (subspace spanning applies).
    Get { space: String, coords: Coords },
    /// All live cells compatible with `fixed`.
    Slice { space: String, #[serde(default)] fixed: Coords },
    DefineRule { space: String, rule: Rule },
    /// Written by converters/analyzers: a derived cell with provenance.
    SetDerived { space: String, coords: Coords, value: Value, derivation: Derivation },
    /// Rules whose materialization is pending for a given input key set is the
    /// converter runner's business; the core just lists rules.
    Rules { space: String },
    /// Run pending derivations for a space (optionally one rule) by calling
    /// converter capabilities over the mesh. Daemon-level: the pure layer
    /// rejects it; the service intercepts it before `handle`.
    Materialize {
        space: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    /// Run an analysis op (compare/classify/summarize/check) over a slice via
    /// the grid.ai service. Daemon-level, same interception rule.
    Analyze {
        space: String,
        op: String,
        #[serde(default)]
        fixed: Coords,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<serde_json::Value>,
    },
}

fn default_kind() -> String {
    "string".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionInfo {
    pub name: String,
    pub kind: String,
    pub coords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceInfo {
    pub name: String,
    pub dimensions: Vec<DimensionInfo>,
    pub rules: Vec<Rule>,
    pub cells: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Spaces { spaces: Vec<String> },
    Space { info: SpaceInfo },
    Cell { cell: Option<Cell>, #[serde(skip_serializing_if = "Option::is_none")] key: Option<String> },
    Cells { cells: Vec<Cell> },
    Rules { rules: Vec<Rule> },
    Materialized { written: usize, skipped: usize, failed: usize },
    Analysis { result: serde_json::Value },
    Error { error: String },
}

impl Response {
    pub fn error(msg: impl Into<String>) -> Response {
        Response::Error { error: msg.into() }
    }
}

/// Outcome of handling a request: the response to send back, plus any stamped
/// op the caller must append to the replicated log (already applied locally).
pub struct Handled {
    pub response: Response,
    pub op: Option<Stamped>,
}

impl Handled {
    fn read(response: Response) -> Handled {
        Handled { response, op: None }
    }
}

/// Route one request onto the machine. `writer` identifies this replica in
/// LWW tie-breaks (node id). Mutations are stamped + applied here; the caller
/// owns durability/replication of the returned op.
pub fn handle(m: &mut SpaceMachine, writer: &str, req: Request) -> Handled {
    match req {
        Request::Spaces => {
            Handled::read(Response::Spaces { spaces: m.spaces.keys().cloned().collect() })
        }
        Request::Describe { space } => match m.spaces.get(&space) {
            Some(s) => Handled::read(Response::Space {
                info: SpaceInfo {
                    name: s.name.clone(),
                    dimensions: s
                        .dimensions
                        .values()
                        .map(|d| DimensionInfo {
                            name: d.name.clone(),
                            kind: d.kind.clone(),
                            coords: d.coords.clone(),
                        })
                        .collect(),
                    rules: s.rules.values().cloned().collect(),
                    cells: s.cells.values().filter(|c| c.value.is_some()).count(),
                },
            }),
            None => Handled::read(Response::error(format!("no such space: {space}"))),
        },
        Request::Get { space, coords } => {
            let cell = m.get(&space, &coords).cloned();
            let key = cell.as_ref().map(|c| cell_key(&c.coords));
            Handled::read(Response::Cell { cell, key })
        }
        Request::Slice { space, fixed } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            let cells = m.slice(&space, &fixed).into_iter().cloned().collect();
            Handled::read(Response::Cells { cells })
        }
        Request::Rules { space } => match m.spaces.get(&space) {
            Some(s) => Handled::read(Response::Rules { rules: s.rules.values().cloned().collect() }),
            None => Handled::read(Response::error(format!("no such space: {space}"))),
        },
        // Mutations: validate, stamp, apply, hand the op back.
        Request::CreateSpace { space } => {
            if space.is_empty() {
                return Handled::read(Response::error("space name must not be empty"));
            }
            mutate(m, writer, Op::CreateSpace { space })
        }
        Request::AddDimension { space, name, kind } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            mutate(m, writer, Op::AddDimension { space, name, kind })
        }
        Request::AddCoordinate { space, dimension, key } => {
            let has_dim = m
                .spaces
                .get(&space)
                .map(|s| s.dimensions.contains_key(&dimension))
                .unwrap_or(false);
            if !has_dim {
                return Handled::read(Response::error(format!(
                    "no such dimension: {space}/{dimension}"
                )));
            }
            mutate(m, writer, Op::AddCoordinate { space, dimension, key })
        }
        Request::SetCell { space, coords, value } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            if coords.is_empty() {
                return Handled::read(Response::error("coords must fix at least one dimension"));
            }
            mutate(m, writer, Op::SetCell { space, coords, value })
        }
        Request::ClearCell { space, coords } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            mutate(m, writer, Op::ClearCell { space, coords })
        }
        Request::DefineRule { space, rule } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            if rule.id.is_empty() {
                return Handled::read(Response::error("rule id must not be empty"));
            }
            mutate(m, writer, Op::DefineRule { space, rule })
        }
        Request::SetDerived { space, coords, value, derivation } => {
            if !m.spaces.contains_key(&space) {
                return Handled::read(Response::error(format!("no such space: {space}")));
            }
            if coords.is_empty() {
                return Handled::read(Response::error("coords must fix at least one dimension"));
            }
            mutate(m, writer, Op::SetDerived { space, coords, value, derivation })
        }
        Request::Materialize { .. } | Request::Analyze { .. } => Handled::read(Response::error(
            "materialize/analyze need the mesh and are handled by the daemon",
        )),
    }
}

fn mutate(m: &mut SpaceMachine, writer: &str, op: Op) -> Handled {
    let stamped = m.stamp(writer, op);
    m.apply(&stamped);
    Handled { response: Response::Ok, op: Some(stamped) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::REPRESENTATION;

    fn coords(pairs: &[(&str, &str)]) -> Coords {
        pairs.iter().map(|(d, k)| (d.to_string(), k.to_string())).collect()
    }

    #[test]
    fn full_request_flow() {
        let mut m = SpaceMachine::default();
        let h = handle(&mut m, "n1", Request::CreateSpace { space: "audit".into() });
        assert!(matches!(h.response, Response::Ok));
        assert!(h.op.is_some());

        handle(&mut m, "n1", Request::AddDimension {
            space: "audit".into(),
            name: "document".into(),
            kind: "string".into(),
        });
        handle(&mut m, "n1", Request::SetCell {
            space: "audit".into(),
            coords: coords(&[("document", "d1"), (REPRESENTATION, "raw")]),
            value: Value::Text { text: "hello".into() },
        });

        let h = handle(&mut m, "n1", Request::Slice {
            space: "audit".into(),
            fixed: coords(&[(REPRESENTATION, "raw")]),
        });
        match h.response {
            Response::Cells { cells } => assert_eq!(cells.len(), 1),
            other => panic!("unexpected: {other:?}"),
        }

        let h = handle(&mut m, "n1", Request::Describe { space: "audit".into() });
        match h.response {
            Response::Space { info } => {
                assert_eq!(info.cells, 1);
                assert!(info.dimensions.iter().any(|d| d.name == "document"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn mutations_on_missing_space_error_without_op() {
        let mut m = SpaceMachine::default();
        let h = handle(&mut m, "n1", Request::SetCell {
            space: "nope".into(),
            coords: coords(&[("k", "x")]),
            value: Value::Bool { bool: true },
        });
        assert!(matches!(h.response, Response::Error { .. }));
        assert!(h.op.is_none());
    }

    #[test]
    fn empty_coords_rejected() {
        let mut m = SpaceMachine::default();
        handle(&mut m, "n1", Request::CreateSpace { space: "s".into() });
        let h = handle(&mut m, "n1", Request::SetCell {
            space: "s".into(),
            coords: Coords::new(),
            value: Value::Bool { bool: true },
        });
        assert!(matches!(h.response, Response::Error { .. }));
    }

    #[test]
    fn wire_shapes_are_stable_json() {
        // The JSON envelope is the cross-language contract; pin its shape.
        let req: Request = serde_json::from_value(serde_json::json!({
            "cmd": "set_cell",
            "space": "s",
            "coords": {"document": "d1"},
            "value": {"kind": "text", "text": "hi"}
        }))
        .expect("parse");
        assert!(matches!(req, Request::SetCell { .. }));

        let resp = serde_json::to_value(Response::Spaces { spaces: vec!["s".into()] }).unwrap();
        assert_eq!(resp, serde_json::json!({"status": "spaces", "spaces": ["s"]}));
    }
}
