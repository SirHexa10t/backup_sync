//! Process-level runtime concerns — how this *process* behaves, as opposed to what it does to the
//! trees: privilege management ([`elevation`]), graceful-stop signals ([`interrupt`]), and
//! one-sync-per-destination mutual exclusion ([`lock`]).

pub mod elevation;
pub mod interrupt;
pub mod lock;
