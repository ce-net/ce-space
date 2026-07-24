# ce-grid

N-dimensional data comparison spaces over the CE mesh — Google Sheets for all kinds of data.
Structure anything, link anything by ref, project it onto a shared representation plane, and
compare it with AI — without the data ever needing to live inside the grid.

ce-grid is the core backend of the **grid app group**. The other members are separate apps
that compose with it over the mesh:

| App | Role |
|---|---|
| ce-grid (this repo) | The spaces: dimensions, cells, refs, derivation rules, materialization, analysis orchestration |
| github.com/ce-net/ce-grid-convert-text | `grid.convert.text` — anything -> the text plane |
| github.com/ce-net/ce-grid-convert-embed | `grid.convert.embed` — anything -> the embedding plane |
| github.com/ce-net/ce-grid-ai | `grid.ai` — compare / classify / summarize / policy-check |
| github.com/ce-net/ce-grid-cell-table | a mesh-transmitted table view of any 2D slice |
| github.com/ce-net/ce-grid-cell-chart | a mesh-transmitted chart of number cells along a dimension |
| github.com/ce-net/ce-grid-web | the emergent dashboard host (hardcodes nothing) |

The core never ships converters and never enumerates them: it locates whatever the mesh
currently offers (DHT discovery). Install a new converter anywhere on the mesh and every
space can use it.

## The model

A **space** is a sparse coordinate system with ANY number of named dimensions — not sheets
of columns and rows. A **cell** is a value addressed by a coordinate tuple:

```
{document: "contract-14", policy: "gdpr-retention", time: "2026-07"} -> cell
```

- Partial tuples span subspaces: a cell addressed only by `{policy: "gdpr-retention"}`
  applies to every document — the policy text exists once, not copied down a column.
- A **slice** fixes some dimensions and varies the rest. A 2D slice renders as a familiar
  sheet, but that exists only in views, never in the model.
- Every space carries a built-in **representation** dimension. Source data lives at
  `representation=raw`; derived planes (`text`, `embedding`, `label`, `number`) sit at the
  SAME coordinates with a different representation coordinate. Nothing is overwritten and
  provenance is positional.

A cell's value is an inline scalar (text, number, bool, timestamp, json, vector) or a
**ref** — a typed link to data anywhere on the mesh: `blob:<cid>`, `drive:<node>/<path>`,
`db:<collection>/<doc>`, `twin:<node>/<id>/<reading>`, `cap:<capability>/<args>`,
`topic:<name>` (reserved for streaming), `url:<...>`. The core never interprets ref
payloads; converters do. Adding a data source = a new ref scheme + a converter app, zero
core change.

**Derivation rules** project a slice onto another representation via a converter located on
the mesh: `{source, converter, target_representation, params}`. Materialization is
content-addressed memoized — a derived cell records the hash of (input, converter, params)
and is only recomputed when that changes.

**Analysis ops** run over representation slices via the `grid.ai` capability: `compare`,
`classify`, `summarize`, and `check` (the policy op — verdicts are written back into the
space as `label` cells at the same coordinates).

## Quick start

```bash
# On a machine with a running ce node:
cargo build --release
./target/release/ce-grid serve &          # or: ce app install ./ce-grid

ce-grid create audit
ce-grid set audit --at document=d1,representation=raw --text "We retain user data for 30 days."
ce-grid set audit --at policy=gdpr --text "must not retain user data longer than 90 days"

# Project the raw plane onto text (needs ce-grid-convert-text running somewhere on the mesh):
ce-grid rule audit --id to-text --converter grid.convert.text --target text
ce-grid materialize audit

# Check every document against a policy (needs ce-grid-ai on the mesh):
ce-grid analyze audit --op check --fix representation=text \
  --params '{"policy":"must not retain user data longer than 90 days"}'

ce-grid slice audit --fix representation=label     # the verdicts, as cells
ce-grid slice audit --fix document=d1              # everything known about d1, all planes
```

## Wire contract (topic `grid/ctl`, service `ce.grid`)

Requests are JSON tagged on `cmd`; responses tagged on `status`. Examples:

```jsonc
{"cmd":"slice","space":"audit","fixed":{"representation":"text"}}
// -> {"status":"cells","cells":[{"coords":{...},"value":{"kind":"text","text":"..."},"lamport":3,"writer":"<node>"}]}

{"cmd":"set_cell","space":"audit","coords":{"document":"d1","representation":"raw"},
 "value":{"kind":"ref","r":{"scheme":"blob","address":"<cid>"}}}
// -> {"status":"ok"}

{"cmd":"materialize","space":"audit"}
// -> {"status":"materialized","written":2,"skipped":5,"failed":0}
```

Full verb list: `spaces`, `describe`, `create_space`, `add_dimension`, `add_coordinate`,
`set_cell`, `clear_cell`, `get`, `slice`, `define_rule`, `rules`, `materialize`, `analyze`.
Values are tagged on `kind`: `text` | `number` | `bool` | `timestamp` | `json` | `vector` |
`ref`.

Converter contract (what `materialize` calls, topic `<converter>/ctl`):
`{"op":"convert","value":<Value>,"target":"text","params":{}}` ->
`{"ok":true,"result":{"value":<Value>}}` or `{"error":"..."}`.

## Trust

The `grid:*` capability namespace is declared in `cecapabilities.toml` (this app owns it).
v1 enforcement, honestly: reads are open to any authenticated mesh caller; every mutation
(including materialize/analyze) is gated to the owning node. Cap-gated remote writes verify
a ce-cap chain per op class and layer on without a wire change.

## State and convergence

Every mutation is a stamped op (true Lamport, writer id as tie-break); the state machine is
LWW-convergent per cell and order-independent — replicas that see the same op set reach the
same state (property-tested). v1 persists a local atomic snapshot on the owning node;
replicated multi-writer spaces ride the same op model over a replicated log as the
documented next step. Remote spaces are already reachable: every read verb takes
`--node <id>`.

## Known constraint

Cross-node `blob:` ref resolution rides the mesh's blob fetch-by-CID path; where DHT
provider discovery is weak, fetch from a known holder (`GET /blobs/<hash>?from=<node>`) —
converters that dereference blob refs should prefer the holder-directed path when the ref
names one.

## Tests

```bash
cargo test          # pure model, protocol, planning, service gates — no node needed
```
