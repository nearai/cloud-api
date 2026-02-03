pub mod client;
pub mod ports;

pub use client::RagServiceClient;
pub use ports::{RagError, RagFile, RagServiceTrait, SearchResult, VectorStore};
