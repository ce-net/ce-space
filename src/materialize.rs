//! Derivation materialization: project raw cells onto other representation
//! coordinates by calling converter capabilities located on the mesh.
//!
//! Planning is pure (unit-testable); only `convert` touches the mesh. Results
//! are content-addressed memoized: a derived cell records the hash of
//! (input value, converter, params), and planning skips targets whose stored
//! input key is unchanged.

use anyhow::{anyhow, Context, Result};
use ce_rs::CeClient;
use sha2::{Digest, Sha256};

use crate::space::{Coords, SpaceMachine, Value, RAW, REPRESENTATION};

/// One pending conversion: source cell value -> target coordinates.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub rule_id: String,
    pub converter: String,
    pub target_representation: String,
    pub target: Coords,
    pub value: Value,
    pub params: Option<serde_json::Value>,
    pub input_key: String,
}

/// The memoization key: hash of (value, converter, params).
pub fn input_key(value: &Value, converter: &str, params: &Option<serde_json::Value>) -> String {
    let mut h = Sha256::new();
    // serde_json serialization of Value is deterministic (struct field order).
    h.update(serde_json::to_vec(value).unwrap_or_default());
    h.update([0x1f]);
    h.update(converter.as_bytes());
    h.update([0x1f]);
    if let Some(p) = params {
        h.update(p.to_string().as_bytes());
    }
    hex::encode(h.finalize())
}

/// Plan pending work for a space. Returns (items, skipped) where skipped
/// counts up-to-date targets left alone.
pub fn plan(m: &SpaceMachine, space: &str, rule_filter: Option<&str>) -> (Vec<WorkItem>, usize) {
    let mut items = Vec::new();
    let mut skipped = 0;
    let Some(s) = m.spaces.get(space) else { return (items, skipped) };

    for rule in s.rules.values() {
        if let Some(f) = rule_filter {
            if rule.id != f {
                continue;
            }
        }
        // Source slice: the rule's fixed dims, plus representation=raw unless
        // the rule pins representation itself.
        let mut fixed = rule.source.clone();
        fixed
            .entry(REPRESENTATION.to_string())
            .or_insert_with(|| RAW.to_string());

        for cell in m.slice(space, &fixed) {
            let Some(value) = &cell.value else { continue };
            // A cell already on a non-source representation is never a source.
            if let Some(rep) = cell.coords.get(REPRESENTATION) {
                if rep != fixed.get(REPRESENTATION).expect("fixed has representation") {
                    continue;
                }
            }
            let mut target = cell.coords.clone();
            target.insert(REPRESENTATION.to_string(), rule.target_representation.clone());
            let key = input_key(value, &rule.converter, &rule.params);
            let target_key = crate::space::cell_key(&target);
            if s.derived.get(&target_key).map(|d| d.input_key.as_str()) == Some(key.as_str()) {
                skipped += 1;
                continue;
            }
            items.push(WorkItem {
                rule_id: rule.id.clone(),
                converter: rule.converter.clone(),
                target_representation: rule.target_representation.clone(),
                target,
                value: value.clone(),
                params: rule.params.clone(),
                input_key: key,
            });
        }
    }
    (items, skipped)
}

/// Call the converter for one work item over the mesh. Converter contract:
/// request `{"op":"convert","value":...,"target":...,"params":...}` on topic
/// `<converter>/ctl`, service name = converter; reply `{"ok":true,"result":
/// {"value":...}}` or `{"error":"..."}`.
pub async fn convert(ce: &CeClient, item: &WorkItem, timeout_ms: u64) -> Result<Value> {
    let req = serde_json::json!({
        "op": "convert",
        "value": item.value,
        "target": item.target_representation,
        "params": item.params,
    });
    let topic = format!("{}/ctl", item.converter);
    let raw = ce_rs::locate::call(
        ce,
        &item.converter,
        &topic,
        &serde_json::to_vec(&req)?,
        &ce_rs::locate::LocateOpts::default(),
        timeout_ms,
    )
    .await
    .with_context(|| format!("no converter answered for '{}'", item.converter))?;
    parse_convert_reply(&raw)
}

/// Pure reply parsing, unit-tested apart from the mesh.
pub fn parse_convert_reply(raw: &[u8]) -> Result<Value> {
    let reply: serde_json::Value =
        serde_json::from_slice(raw).context("undecodable converter reply")?;
    if let Some(err) = reply.get("error").and_then(|e| e.as_str()) {
        return Err(anyhow!("converter refused: {err}"));
    }
    let value = reply
        .get("result")
        .and_then(|r| r.get("value"))
        .ok_or_else(|| anyhow!("converter reply lacks result.value"))?;
    serde_json::from_value(value.clone()).context("converter returned an invalid value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::{Derivation, Op, Rule};

    fn coords(pairs: &[(&str, &str)]) -> Coords {
        pairs.iter().map(|(d, k)| (d.to_string(), k.to_string())).collect()
    }

    fn machine_with_rule() -> SpaceMachine {
        let mut m = SpaceMachine::default();
        for op in [
            Op::CreateSpace { space: "s".into() },
            Op::SetCell {
                space: "s".into(),
                coords: coords(&[("document", "d1"), (REPRESENTATION, RAW)]),
                value: Value::Text { text: "hello".into() },
            },
            Op::DefineRule {
                space: "s".into(),
                rule: Rule {
                    id: "r1".into(),
                    source: Coords::new(),
                    converter: "space.convert.text".into(),
                    target_representation: "text".into(),
                    params: None,
                },
            },
        ] {
            let st = m.stamp("n", op);
            m.apply(&st);
        }
        m
    }

    #[test]
    fn plan_finds_pending_work() {
        let m = machine_with_rule();
        let (items, skipped) = plan(&m, "s", None);
        assert_eq!(items.len(), 1);
        assert_eq!(skipped, 0);
        let item = &items[0];
        assert_eq!(item.target.get(REPRESENTATION).unwrap(), "text");
        assert_eq!(item.target.get("document").unwrap(), "d1");
        assert_eq!(item.converter, "space.convert.text");
    }

    #[test]
    fn plan_skips_up_to_date_targets() {
        let mut m = machine_with_rule();
        let (items, _) = plan(&m, "s", None);
        let item = items[0].clone();
        let st = m.stamp("n", Op::SetDerived {
            space: "s".into(),
            coords: item.target.clone(),
            value: Value::Text { text: "hello".into() },
            derivation: Derivation {
                rule_id: item.rule_id.clone(),
                converter: item.converter.clone(),
                input_key: item.input_key.clone(),
            },
        });
        m.apply(&st);
        let (items, skipped) = plan(&m, "s", None);
        assert!(items.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn plan_replans_when_input_changes() {
        let mut m = machine_with_rule();
        let (items, _) = plan(&m, "s", None);
        let item = items[0].clone();
        let st = m.stamp("n", Op::SetDerived {
            space: "s".into(),
            coords: item.target.clone(),
            value: Value::Text { text: "hello".into() },
            derivation: Derivation {
                rule_id: item.rule_id.clone(),
                converter: item.converter.clone(),
                input_key: item.input_key.clone(),
            },
        });
        m.apply(&st);
        // Source changes -> input key changes -> work is pending again.
        let st = m.stamp("n", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("document", "d1"), (REPRESENTATION, RAW)]),
            value: Value::Text { text: "changed".into() },
        });
        m.apply(&st);
        let (items, skipped) = plan(&m, "s", None);
        assert_eq!(items.len(), 1);
        assert_eq!(skipped, 0);
    }

    #[test]
    fn plan_never_uses_derived_cells_as_sources() {
        let mut m = machine_with_rule();
        let st = m.stamp("n", Op::SetCell {
            space: "s".into(),
            coords: coords(&[("document", "d2"), (REPRESENTATION, "text")]),
            value: Value::Text { text: "already text".into() },
        });
        m.apply(&st);
        let (items, _) = plan(&m, "s", None);
        // Only d1's raw cell is a source; the representation=text cell is not.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target.get("document").unwrap(), "d1");
    }

    #[test]
    fn rule_filter_limits_planning() {
        let m = machine_with_rule();
        let (items, _) = plan(&m, "s", Some("other-rule"));
        assert!(items.is_empty());
        let (items, _) = plan(&m, "s", Some("r1"));
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn input_key_is_stable_and_sensitive() {
        let v = Value::Text { text: "x".into() };
        let a = input_key(&v, "c", &None);
        let b = input_key(&v, "c", &None);
        assert_eq!(a, b);
        assert_ne!(a, input_key(&Value::Text { text: "y".into() }, "c", &None));
        assert_ne!(a, input_key(&v, "c2", &None));
        assert_ne!(a, input_key(&v, "c", &Some(serde_json::json!({"p": 1}))));
    }

    #[test]
    fn convert_reply_parsing() {
        let ok = br#"{"ok":true,"result":{"value":{"kind":"text","text":"t"}}}"#;
        assert_eq!(parse_convert_reply(ok).unwrap(), Value::Text { text: "t".into() });
        let err = br#"{"error":"scheme not supported"}"#;
        assert!(parse_convert_reply(err).unwrap_err().to_string().contains("scheme"));
        let bad = br#"{"ok":true,"result":{}}"#;
        assert!(parse_convert_reply(bad).is_err());
    }
}
