use tokio::sync::Mutex;

use pensyve_core::config::RetrievalConfig;
use pensyve_core::embedding::OnnxEmbedder;
use pensyve_core::storage::StorageTrait;
use pensyve_core::types::Namespace;
use pensyve_core::vector::VectorIndex;

/// Shared state for the Pensyve MCP server.
///
/// Uses `Box<dyn StorageTrait>` to support both `SQLite` (local) and `PostgreSQL`
/// (cloud gateway) backends without generics.
pub struct PensyveState {
    pub storage: Box<dyn StorageTrait>,
    pub embedder: OnnxEmbedder,
    pub vector_index: Mutex<VectorIndex>,
    pub namespace: Namespace,
    pub retrieval_config: RetrievalConfig,
}
