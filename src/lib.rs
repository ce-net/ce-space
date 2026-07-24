//! ce-grid — N-dimensional data comparison spaces over the CE mesh.
//!
//! The core of the grid app group: sparse coordinate spaces (any number of
//! named dimensions), cells that hold inline values or typed refs to data
//! anywhere on the mesh, a built-in `representation` dimension carrying the
//! shared comparison plane, derivation rules materialized by converter
//! capabilities located on the mesh, and analysis ops composed from the
//! grid.ai capability.
//!
//! Layering: `space` (pure model, LWW-convergent ops) -> `proto` (the one
//! JSON wire surface) -> `store` (atomic persistence) -> `materialize`
//! (derivation planning + converter calls) -> `service` (the mesh instance).

pub mod materialize;
pub mod proto;
pub mod service;
pub mod space;
pub mod store;
