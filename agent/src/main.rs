//! Chomp: a repo-aware agentic code-review host built on `agent_core`.
//!
//! This binary is a host for `agent_core`, an owner of repository tools, and
//! nothing else. FRG-15 scaffolds the CLI, configuration loading, and output
//! contract; the review stages land in their own tickets.

mod contract;
mod host;
mod outputs;
mod pr_agent;
mod sessions;
mod snapshot;
mod synthesis;
mod tools;
mod transport;

fn main() {
    // CLI parsing and stage orchestration land in FRG-15 chunk C2.
}
