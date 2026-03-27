mod params;
mod server;
mod state;

pub use params::*;
pub use server::PensyveMcpServer;
pub use state::PensyveState;

/// Tenant identifier inserted into HTTP request extensions by the gateway's
/// auth middleware. Tool handlers use this to resolve per-tenant namespaces.
/// In local (stdio) mode, this is absent and the default namespace is used.
#[derive(Clone, Debug)]
pub struct TenantId(pub String);
