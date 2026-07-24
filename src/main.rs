//! ce-grid CLI: `serve` runs this node's instance; every other verb is one
//! JSON round-trip to a grid instance over the mesh (the local one by
//! default, any node with `--node <id>`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use ce_rs::CeClient;

use ce_grid::proto::{Request, Response};
use ce_grid::service::CTL_TOPIC;
use ce_grid::space::{Coords, Ref, Rule, Value};

const USAGE: &str = "ce-grid — N-dimensional data comparison spaces over the CE mesh

USAGE:
  ce-grid serve [--state <path>]                 run this node's instance
  ce-grid spaces [--node <id>]                   list spaces
  ce-grid create <space>                         create a space
  ce-grid describe <space>                       dimensions, rules, cell count
  ce-grid dim <space> <name> [--kind <k>]        add a dimension
  ce-grid coord <space> <dim> <key>              declare a coordinate
  ce-grid set <space> --at k=v[,k=v...] (--text s | --number f | --bool b | --ts ms | --json j | --ref scheme:addr)
  ce-grid clear <space> --at k=v[,...]           tombstone a cell
  ce-grid get <space> --at k=v[,...]             most specific cell at a point
  ce-grid slice <space> [--fix k=v[,...]]        all cells compatible with the fix
  ce-grid rule <space> --id r --converter c --target rep [--source k=v[,...]] [--params <json>]
  ce-grid rules <space>                          list derivation rules
  ce-grid materialize <space> [--rule r]         run pending derivations (via mesh converters)
  ce-grid analyze <space> --op <compare|classify|summarize|check> [--fix k=v[,...]] [--params <json>]

Every verb accepts --node <node-id> to target a remote instance (reads only in v1).";

fn parse_args(args: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut flags = BTreeMap::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            if i + 1 < args.len() {
                flags.insert(name.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name.to_string(), String::new());
                i += 1;
            }
        } else {
            pos.push(a.clone());
            i += 1;
        }
    }
    (flags, pos)
}

/// Parse "dim=key,dim2=key2" into a coordinate tuple.
fn parse_coords(s: &str) -> Result<Coords> {
    let mut coords = Coords::new();
    for part in s.split(',').filter(|p| !p.is_empty()) {
        let (d, k) = part
            .split_once('=')
            .ok_or_else(|| anyhow!("bad coordinate '{part}' (want dim=key)"))?;
        if d.is_empty() || k.is_empty() {
            bail!("bad coordinate '{part}' (empty side)");
        }
        coords.insert(d.to_string(), k.to_string());
    }
    Ok(coords)
}

fn parse_value(flags: &BTreeMap<String, String>) -> Result<Value> {
    if let Some(t) = flags.get("text") {
        return Ok(Value::Text { text: t.clone() });
    }
    if let Some(n) = flags.get("number") {
        return Ok(Value::Number { number: n.parse().context("--number wants a float")? });
    }
    if let Some(b) = flags.get("bool") {
        return Ok(Value::Bool { bool: b.parse().context("--bool wants true|false")? });
    }
    if let Some(ts) = flags.get("ts") {
        return Ok(Value::Timestamp { ms: ts.parse().context("--ts wants epoch millis")? });
    }
    if let Some(j) = flags.get("json") {
        return Ok(Value::Json { json: serde_json::from_str(j).context("--json wants valid JSON")? });
    }
    if let Some(r) = flags.get("ref") {
        let r = Ref::parse(r).ok_or_else(|| anyhow!("--ref wants scheme:address"))?;
        return Ok(Value::Ref { r });
    }
    bail!("give the value: --text | --number | --bool | --ts | --json | --ref")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        println!("{USAGE}");
        return Ok(());
    };
    let (flags, pos) = parse_args(&args[1..]);

    if cmd == "serve" {
        // Default state lives in the HOME dir, not cwd: the app supervisor runs
        // daemons from the install dir, and per-cwd state would silently fork.
        let state: PathBuf = flags
            .get("state")
            .cloned()
            .or_else(|| std::env::var("CE_GRID_STATE").ok())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".ce-grid/ce-grid.json"))
            })
            .unwrap_or_else(|| "ce-grid.json".into());
        return ce_grid::service::run(&state).await;
    }

    let ce = CeClient::local();
    let self_node = ce
        .status()
        .await
        .context("cannot reach the local ce node — is it running? (ce start)")?
        .node_id;
    let target = flags.get("node").cloned().unwrap_or_else(|| self_node.clone());

    let space = || -> Result<String> {
        pos.first().cloned().ok_or_else(|| anyhow!("missing <space>\n\n{USAGE}"))
    };

    let req = match cmd.as_str() {
        "spaces" => Request::Spaces,
        "create" => Request::CreateSpace { space: space()? },
        "describe" => Request::Describe { space: space()? },
        "dim" => Request::AddDimension {
            space: space()?,
            name: pos.get(1).cloned().ok_or_else(|| anyhow!("missing <name>"))?,
            kind: flags.get("kind").cloned().unwrap_or_else(|| "string".into()),
        },
        "coord" => Request::AddCoordinate {
            space: space()?,
            dimension: pos.get(1).cloned().ok_or_else(|| anyhow!("missing <dim>"))?,
            key: pos.get(2).cloned().ok_or_else(|| anyhow!("missing <key>"))?,
        },
        "set" => Request::SetCell {
            space: space()?,
            coords: parse_coords(flags.get("at").ok_or_else(|| anyhow!("missing --at"))?)?,
            value: parse_value(&flags)?,
        },
        "clear" => Request::ClearCell {
            space: space()?,
            coords: parse_coords(flags.get("at").ok_or_else(|| anyhow!("missing --at"))?)?,
        },
        "get" => Request::Get {
            space: space()?,
            coords: parse_coords(flags.get("at").ok_or_else(|| anyhow!("missing --at"))?)?,
        },
        "slice" => Request::Slice {
            space: space()?,
            fixed: match flags.get("fix") {
                Some(f) => parse_coords(f)?,
                None => Coords::new(),
            },
        },
        "rule" => Request::DefineRule {
            space: space()?,
            rule: Rule {
                id: flags.get("id").cloned().ok_or_else(|| anyhow!("missing --id"))?,
                source: match flags.get("source") {
                    Some(s) => parse_coords(s)?,
                    None => Coords::new(),
                },
                converter: flags
                    .get("converter")
                    .cloned()
                    .ok_or_else(|| anyhow!("missing --converter"))?,
                target_representation: flags
                    .get("target")
                    .cloned()
                    .ok_or_else(|| anyhow!("missing --target"))?,
                params: match flags.get("params") {
                    Some(p) => Some(serde_json::from_str(p).context("--params wants JSON")?),
                    None => None,
                },
            },
        },
        "rules" => Request::Rules { space: space()? },
        "materialize" => Request::Materialize { space: space()?, rule: flags.get("rule").cloned() },
        "analyze" => Request::Analyze {
            space: space()?,
            op: flags.get("op").cloned().ok_or_else(|| anyhow!("missing --op"))?,
            fixed: match flags.get("fix") {
                Some(f) => parse_coords(f)?,
                None => Coords::new(),
            },
            params: match flags.get("params") {
                Some(p) => Some(serde_json::from_str(p).context("--params wants JSON")?),
                None => None,
            },
        },
        other => {
            bail!("unknown command '{other}'\n\n{USAGE}");
        }
    };

    let raw = ce
        .request(&target, CTL_TOPIC, &serde_json::to_vec(&req)?, 20_000)
        .await
        .with_context(|| {
            format!(
                "no reply from grid instance on {} — is `ce-grid serve` running there?",
                &target[..target.len().min(8)]
            )
        })?;
    let resp: Response = serde_json::from_slice(&raw).context("undecodable reply")?;
    match &resp {
        Response::Error { error } => bail!("{error}"),
        other => println!("{}", serde_json::to_string_pretty(other)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coords_parsing() {
        let c = parse_coords("document=d1,representation=raw").unwrap();
        assert_eq!(c.get("document").unwrap(), "d1");
        assert_eq!(c.get("representation").unwrap(), "raw");
        assert!(parse_coords("noequals").is_err());
        assert!(parse_coords("=x").is_err());
        assert!(parse_coords("x=").is_err());
        assert!(parse_coords("").unwrap().is_empty());
    }

    #[test]
    fn value_parsing() {
        let mut f = BTreeMap::new();
        f.insert("text".to_string(), "hi".to_string());
        assert_eq!(parse_value(&f).unwrap(), Value::Text { text: "hi".into() });

        let mut f = BTreeMap::new();
        f.insert("ref".to_string(), "blob:abc123".to_string());
        match parse_value(&f).unwrap() {
            Value::Ref { r } => assert_eq!(r.scheme, "blob"),
            other => panic!("unexpected {other:?}"),
        }

        let f = BTreeMap::new();
        assert!(parse_value(&f).is_err());
    }

    #[test]
    fn flag_parsing() {
        let args: Vec<String> =
            ["s1", "--at", "k=v", "--text", "hello"].iter().map(|s| s.to_string()).collect();
        let (flags, pos) = parse_args(&args);
        assert_eq!(pos, vec!["s1".to_string()]);
        assert_eq!(flags.get("at").unwrap(), "k=v");
        assert_eq!(flags.get("text").unwrap(), "hello");
    }
}
