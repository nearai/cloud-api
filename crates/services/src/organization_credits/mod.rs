//! Organization credentials, administrative policy, and credit distribution.
pub mod models;
pub mod ports;
mod service;
pub use service::{OrganizationCreditService, OrganizationCreditServiceTrait};

#[cfg(test)]
mod tests;
