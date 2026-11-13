//! atelier: gate, records and guards for agents building one project in parallel git worktrees.
//!
//! Module 3.1, the skeleton: exit codes, the dispatcher and pin, settings and the owner's files,
//! the shared state and its locks, runs with process groups and the survivor scan, the tree id,
//! and plain refs. Commands themselves come in later modules.

pub mod agents;
pub mod check;
pub mod cli;
pub mod dispatch;
pub mod exit;
pub mod fsutil;
pub mod git;
pub mod locks;
pub mod machines;
pub mod names;
pub mod nextest;
pub mod platform;
pub mod preflight;
pub mod prereq;
pub mod procs;
pub mod records;
pub mod refs;
pub mod rules;
pub mod runs;
pub mod settings;
pub mod signals;
pub mod state;
pub mod task;
pub mod template;
pub mod tree;
