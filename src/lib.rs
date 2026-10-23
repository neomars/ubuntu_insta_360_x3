//! X3 Studio Local — traitement 100 % local des vidéos Insta360 X3 (moteur Rust).
//!
//! Architecture (voir `docs/RUST_PORT.md`) :
//! `geometry`/`calibration` (maths) → `media` (sonde) → `render` (deux fisheye -> vue) →
//! `stabilize` (mouvement de caméra) → `pipeline` (FFmpeg + rendu) → `project`/`jobs`/`app` →
//! `server` (API HTTP + interface web embarquée) / `cli`.

pub mod app;
pub mod calibration;
pub mod cli;
pub mod error;
pub mod geometry;
pub mod grade;
pub mod jobs;
pub mod media;
pub mod multi;
pub mod pipeline;
pub mod project;
pub mod render;
pub mod server;
pub mod spherical;
pub mod stabilize;
pub mod stick;
pub mod synth;
pub mod tracker;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
