//! Experimental authenticated server for a single operator-owned workspace.
pub(crate) mod artifacts;
pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod contract;
pub(crate) mod controller;
pub(crate) mod http;
pub(crate) mod sink;
mod startup;
pub(crate) use startup::run;
#[cfg(test)]
mod tests;
