---
name: ce-grid
description: How to operate and extend ce-grid — N-dimensional data comparison spaces over the CE mesh (the core of the grid app group). Read before touching this repo or building against the grid/ctl wire contract.
---

# ce-grid

The core backend of the grid app group: sparse N-dimensional spaces whose cells hold inline
values or typed refs to data anywhere on the mesh, a built-in `representation` dimension
carrying the shared comparison plane, derivation rules materialized by mesh-located
converter capabilities, and analysis ops composed from the `grid.ai` capability.

## Operate

- `ce-grid serve [--state <path>]` — run this node's instance (needs a running ce node).
  State defaults to `ce-grid.json` in cwd, or `$CE_GRID_STATE`.
- Every other verb is one mesh round-trip on topic `grid/ctl` (service `ce.grid`), local
  node by default, `--node <id>` for remote instances (reads only in v1).
- Coordinate tuples on the CLI are `dim=key,dim2=key2` (`--at`, `--fix`, `--source`).
- The demo loop: `create` -> `set` raw cells -> `rule` (converter + target representation)
  -> `materialize` -> `analyze --op check` -> `slice --fix representation=label`.
- Materialize/analyze need converter apps on the mesh: ce-grid-convert-text,
  ce-grid-convert-embed, ce-grid-ai (each its own repo under github.com/ce-net).

## Code map

- `src/space.rs` — the pure model: SpaceMachine, Op/Stamped (true Lamport + writer
  tie-break, LWW per cell, order-convergent — property-tested), subspace-spanning partial
  tuples, slice/get. NO mesh, NO IO.
- `src/proto.rs` — the ONE JSON wire surface (`cmd`-tagged requests, `status`-tagged
  responses) + pure routing onto the machine. Mutations come back as stamped ops.
- `src/store.rs` — atomic JSON snapshot persistence (temp + rename).
- `src/materialize.rs` — pure derivation planning (content-addressed memo keys) + the
  converter mesh call (`<converter>/ctl`, `{"op":"convert",...}`).
- `src/service.rs` — the instance: write gate (v1: mutations only from the owning node),
  materialize/analyze orchestration, serve + advertise loops.
- `src/main.rs` — CLI (hand-rolled args; one round-trip per verb).

## Rules for extending

- The `representation` dimension is built-in; never special-case other dimensions in the
  core. New planes are just new representation coordinates.
- The core NEVER interprets ref payloads and NEVER ships a converter. New data source =
  new ref scheme string + a converter app on the mesh; zero core change.
- Keep `space.rs` pure and order-convergent: every mutation is a stamped op through
  `apply`; never mutate state outside it. If you add an op, add the convergence test
  (apply in both orders, assert equal states).
- Wire shapes are a cross-language contract (Python converters, JS cells parse them).
  Never rename JSON tags; add optional fields only.
- Cap-gating goes in `service.rs::handle` per op class (`grid:read`/`grid:write`/
  `grid:define`/`grid:analyze` — see cecapabilities.toml); never in the pure layers.
- Tests are colocated and node-free (dead client `http://127.0.0.1:9` in service tests).
