//! The ce-space instance: a mesh service on `space/ctl` owning this node's
//! spaces. Reads are open to any authenticated mesh caller; mutations are
//! gated to the owning node in v1 (cap-gated remote writes via space:write are
//! the documented next step — the namespace is declared in
//! cecapabilities.toml). Materialize/analyze compose converter and space.ai
//! capabilities located on the mesh.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ce_rs::CeClient;
use tokio::sync::Mutex;

use crate::materialize::{self, WorkItem};
use crate::proto::{self, Request, Response};
use crate::space::{Coords, Derivation, SpaceMachine, Value, REPRESENTATION};
use crate::store::Store;

/// DHT service name instances advertise.
pub const SERVICE: &str = "ce.space";
/// The request/reply control topic.
pub const CTL_TOPIC: &str = "space/ctl";

const ADVERTISE_INTERVAL: Duration = Duration::from_secs(30);
const CONVERT_TIMEOUT_MS: u64 = 30_000;
const AI_TIMEOUT_MS: u64 = 60_000;
/// The analysis service and its topic (a separate ceapp).
const AI_SERVICE: &str = "space.ai";
const AI_TOPIC: &str = "space.ai/ctl";

pub struct Service {
    pub ce: CeClient,
    pub self_node: String,
    state: Mutex<State>,
}

struct State {
    machine: SpaceMachine,
    store: Store,
}

fn mutates(req: &Request) -> bool {
    !matches!(
        req,
        Request::Spaces
            | Request::Describe { .. }
            | Request::Get { .. }
            | Request::Slice { .. }
            | Request::Rules { .. }
    )
}

impl Service {
    pub fn new(ce: CeClient, self_node: String, store: Store) -> Result<Service> {
        let machine = store.load()?;
        Ok(Service { ce, self_node, state: Mutex::new(State { machine, store }) })
    }

    pub async fn handle(&self, from: &str, payload: &[u8]) -> Response {
        let req: Request = match serde_json::from_slice(payload) {
            Ok(r) => r,
            Err(e) => return Response::error(format!("bad request: {e}")),
        };
        if mutates(&req) && from != self.self_node {
            return Response::error(format!(
                "write refused: v1 accepts writes only from the owning node (caller {}, owner {}). \
                 Fix: run the ce-space CLI on the owning node; cap-gated remote writes (space:write) \
                 are the documented next step.",
                short(from),
                short(&self.self_node)
            ));
        }
        match req {
            Request::Materialize { space, rule } => self.materialize(&space, rule.as_deref()).await,
            Request::Analyze { space, op, fixed, params } => {
                self.analyze(&space, &op, fixed, params).await
            }
            other => {
                let mut st = self.state.lock().await;
                let handled = proto::handle(&mut st.machine, &self.self_node, other);
                if handled.op.is_some() {
                    if let Err(e) = st.store.save(&st.machine) {
                        tracing::warn!(error = %e, "persist failed after mutation");
                        return Response::error(format!("persist failed: {e}"));
                    }
                }
                handled.response
            }
        }
    }

    /// Run pending derivations by calling converters over the mesh.
    async fn materialize(&self, space: &str, rule: Option<&str>) -> Response {
        let (items, skipped) = {
            let st = self.state.lock().await;
            if !st.machine.spaces.contains_key(space) {
                return Response::error(format!("no such space: {space}"));
            }
            materialize::plan(&st.machine, space, rule)
        };
        let mut written = 0usize;
        let mut failed = 0usize;
        for item in items {
            match materialize::convert(&self.ce, &item, CONVERT_TIMEOUT_MS).await {
                Ok(value) => {
                    if self.write_derived(space, &item, value).await {
                        written += 1;
                    } else {
                        failed += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!(rule = %item.rule_id, converter = %item.converter,
                        error = %e, "conversion failed");
                    failed += 1;
                }
            }
        }
        Response::Materialized { written, skipped, failed }
    }

    async fn write_derived(&self, space: &str, item: &WorkItem, value: Value) -> bool {
        let req = Request::SetDerived {
            space: space.to_string(),
            coords: item.target.clone(),
            value,
            derivation: Derivation {
                rule_id: item.rule_id.clone(),
                converter: item.converter.clone(),
                input_key: item.input_key.clone(),
            },
        };
        let mut st = self.state.lock().await;
        let handled = proto::handle(&mut st.machine, &self.self_node, req);
        if let Err(e) = st.store.save(&st.machine) {
            tracing::warn!(error = %e, "persist failed after derived write");
            return false;
        }
        !matches!(handled.response, Response::Error { .. })
    }

    /// Run one analysis op over a slice via the space.ai service. The slice
    /// defaults to the text plane unless `fixed` pins representation itself.
    async fn analyze(
        &self,
        space: &str,
        op: &str,
        mut fixed: Coords,
        params: Option<serde_json::Value>,
    ) -> Response {
        fixed
            .entry(REPRESENTATION.to_string())
            .or_insert_with(|| "text".to_string());
        let cells: Vec<(Coords, Value)> = {
            let st = self.state.lock().await;
            if !st.machine.spaces.contains_key(space) {
                return Response::error(format!("no such space: {space}"));
            }
            st.machine
                .slice(space, &fixed)
                .into_iter()
                .filter_map(|c| c.value.clone().map(|v| (c.coords.clone(), v)))
                .collect()
        };
        if cells.is_empty() {
            return Response::error(format!(
                "nothing to analyze: slice {fixed:?} of '{space}' is empty (materialize first?)"
            ));
        }
        let p = params.unwrap_or_else(|| serde_json::json!({}));
        match op {
            "compare" => {
                if cells.len() != 2 {
                    return Response::error(format!(
                        "compare needs exactly 2 cells in the slice, found {}",
                        cells.len()
                    ));
                }
                let req = serde_json::json!({
                    "op": "compare",
                    "a": cells[0].1,
                    "b": cells[1].1,
                    "criteria": p.get("criteria").and_then(|c| c.as_str()).unwrap_or(""),
                });
                match self.ai_call(&req).await {
                    Ok(result) => Response::Analysis {
                        result: serde_json::json!({"op": "compare", "result": result}),
                    },
                    Err(e) => Response::error(e.to_string()),
                }
            }
            "summarize" => {
                let payload: Vec<serde_json::Value> = cells
                    .iter()
                    .map(|(c, v)| serde_json::json!({"coords": c, "value": v}))
                    .collect();
                let req = serde_json::json!({"op": "summarize", "cells": payload});
                match self.ai_call(&req).await {
                    Ok(result) => Response::Analysis {
                        result: serde_json::json!({"op": "summarize", "result": result}),
                    },
                    Err(e) => Response::error(e.to_string()),
                }
            }
            // Per-cell ops that write their verdicts back into the space as
            // label cells: the policy/compliance story.
            "check" | "classify" => {
                let mut results = Vec::new();
                let mut written = 0usize;
                for (coords, value) in &cells {
                    let req = if op == "check" {
                        let Some(policy) = p.get("policy").and_then(|x| x.as_str()) else {
                            return Response::error("check needs params.policy (the policy text)");
                        };
                        serde_json::json!({"op": "check", "value": value, "policy": policy})
                    } else {
                        let Some(labels) = p.get("labels").and_then(|x| x.as_array()) else {
                            return Response::error("classify needs params.labels (an array)");
                        };
                        serde_json::json!({"op": "classify", "value": value, "labels": labels})
                    };
                    match self.ai_call(&req).await {
                        Ok(result) => {
                            if let Some(label) = result.get("label").and_then(|l| l.as_str()) {
                                let mut target = coords.clone();
                                target
                                    .insert(REPRESENTATION.to_string(), "label".to_string());
                                let item = WorkItem {
                                    rule_id: format!("analyze:{op}"),
                                    converter: AI_SERVICE.to_string(),
                                    target_representation: "label".to_string(),
                                    target,
                                    value: value.clone(),
                                    params: Some(p.clone()),
                                    input_key: materialize::input_key(
                                        value,
                                        AI_SERVICE,
                                        &Some(p.clone()),
                                    ),
                                };
                                if self
                                    .write_derived(space, &item, Value::Text { text: label.into() })
                                    .await
                                {
                                    written += 1;
                                }
                            }
                            results.push(serde_json::json!({"coords": coords, "result": result}));
                        }
                        Err(e) => {
                            results.push(serde_json::json!({
                                "coords": coords, "error": e.to_string()
                            }));
                        }
                    }
                }
                Response::Analysis {
                    result: serde_json::json!({"op": op, "results": results, "written": written}),
                }
            }
            other => Response::error(format!(
                "unknown analysis op '{other}' (compare|classify|summarize|check)"
            )),
        }
    }

    async fn ai_call(&self, req: &serde_json::Value) -> Result<serde_json::Value> {
        let raw = ce_rs::locate::call(
            &self.ce,
            AI_SERVICE,
            AI_TOPIC,
            &serde_json::to_vec(req)?,
            &ce_rs::locate::LocateOpts::default(),
            AI_TIMEOUT_MS,
        )
        .await
        .context("no space.ai instance answered (is ce-space-ai installed on the mesh?)")?;
        let reply: serde_json::Value =
            serde_json::from_slice(&raw).context("undecodable space.ai reply")?;
        if let Some(err) = reply.get("error").and_then(|e| e.as_str()) {
            anyhow::bail!("space.ai refused: {err}");
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("space.ai reply lacks result"))
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

pub struct SpaceHandler(pub Arc<Service>);

impl ce_rs::serve::Handler for SpaceHandler {
    async fn handle(&self, req: ce_rs::serve::Request) -> Vec<u8> {
        let resp = self.0.handle(&req.from, &req.payload).await;
        serde_json::to_vec(&resp)
            .unwrap_or_else(|_| br#"{"status":"error","error":"reply encode failed"}"#.to_vec())
    }
}

/// Run the instance: serve space/ctl + advertise ce.space until ctrl-c.
pub async fn run(store_path: &Path) -> Result<()> {
    use futures_util::FutureExt as _;

    let ce = CeClient::local();
    let status = ce
        .status()
        .await
        .context("cannot reach the local ce node — is it running? (ce start)")?;
    let store = Store::open(store_path);
    let svc = Arc::new(Service::new(ce.clone(), status.node_id.clone(), store)?);
    tracing::info!(
        node = %short(&status.node_id),
        state = %store_path.display(),
        "ce-space instance serving on {CTL_TOPIC} (service {SERVICE})"
    );

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    }
    .shared();

    let handler = SpaceHandler(svc.clone());
    let serve_fut = ce_rs::serve::serve(&ce, &[CTL_TOPIC], &handler, shutdown.clone());
    let register_ce = ce.clone();
    let register_fut = async {
        if let Err(e) =
            ce_rs::locate::register(&register_ce, SERVICE, ADVERTISE_INTERVAL, shutdown.clone())
                .await
        {
            tracing::warn!(error = %e, "register loop ended");
        }
    };
    let (serve_res, ()) = tokio::join!(serve_fut, register_fut);
    serve_res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::RAW;

    fn coords(pairs: &[(&str, &str)]) -> Coords {
        pairs.iter().map(|(d, k)| (d.to_string(), k.to_string())).collect()
    }

    fn dead_client() -> CeClient {
        // Any accidental network path fails fast.
        CeClient::new("http://127.0.0.1:9")
    }

    fn svc(dir: &tempfile::TempDir) -> Service {
        let store = Store::open(&dir.path().join("space.json"));
        Service::new(dead_client(), "owner-node".into(), store).unwrap()
    }

    async fn req(s: &Service, from: &str, json: serde_json::Value) -> Response {
        s.handle(from, &serde_json::to_vec(&json).unwrap()).await
    }

    #[tokio::test]
    async fn remote_writes_are_refused_reads_are_open() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(&dir);
        let r = req(&s, "owner-node", serde_json::json!({"cmd":"create_space","space":"s"})).await;
        assert!(matches!(r, Response::Ok));

        let r = req(&s, "stranger", serde_json::json!({"cmd":"create_space","space":"x"})).await;
        match r {
            Response::Error { error } => assert!(error.contains("write refused")),
            other => panic!("expected refusal, got {other:?}"),
        }
        // Reads from strangers work.
        let r = req(&s, "stranger", serde_json::json!({"cmd":"spaces"})).await;
        match r {
            Response::Spaces { spaces } => assert_eq!(spaces, vec!["s".to_string()]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = svc(&dir);
            req(&s, "owner-node", serde_json::json!({"cmd":"create_space","space":"s"})).await;
            req(&s, "owner-node", serde_json::json!({
                "cmd":"set_cell","space":"s",
                "coords":{"k":"x", REPRESENTATION: RAW},
                "value":{"kind":"text","text":"v"}
            }))
            .await;
        }
        let s = svc(&dir);
        let r = req(&s, "anyone", serde_json::json!({"cmd":"slice","space":"s"})).await;
        match r {
            Response::Cells { cells } => assert_eq!(cells.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn analyze_on_empty_slice_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(&dir);
        req(&s, "owner-node", serde_json::json!({"cmd":"create_space","space":"s"})).await;
        let r = req(&s, "owner-node", serde_json::json!({
            "cmd":"analyze","space":"s","op":"summarize"
        }))
        .await;
        match r {
            Response::Error { error } => assert!(error.contains("nothing to analyze")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn materialize_gated_to_owner() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(&dir);
        req(&s, "owner-node", serde_json::json!({"cmd":"create_space","space":"s"})).await;
        let r = req(&s, "stranger", serde_json::json!({"cmd":"materialize","space":"s"})).await;
        match r {
            Response::Error { error } => assert!(error.contains("write refused")),
            other => panic!("unexpected {other:?}"),
        }
        // Owner with no rules: nothing to do, no failure.
        let r = req(&s, "owner-node", serde_json::json!({"cmd":"materialize","space":"s"})).await;
        match r {
            Response::Materialized { written, skipped, failed } => {
                assert_eq!((written, skipped, failed), (0, 0, 0));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn bad_json_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = svc(&dir);
        let r = s.handle("owner-node", b"not json").await;
        assert!(matches!(r, Response::Error { .. }));
    }
}
