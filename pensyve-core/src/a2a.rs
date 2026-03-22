//! Agent-to-Agent (A2A) protocol integration.
//!
//! Implements Pensyve as a memory service within the A2A ecosystem.
//! Agents can discover, query, and share memories across A2A connections.
//!
//! A2A Agent Card format for Pensyve:
//! - name: "pensyve-memory"
//! - capabilities: `["memory.recall", "memory.remember", "memory.forget"]`
//! - protocol: "a2a/v1"

use serde::{Deserialize, Serialize};

/// A2A Agent Card describing Pensyve's capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    /// Agent name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Protocol version.
    pub protocol: String,
    /// Supported capabilities.
    pub capabilities: Vec<AgentCapability>,
    /// Endpoint URL.
    pub endpoint: String,
    /// Authentication requirements.
    pub auth: AgentAuth,
}

/// A capability offered by the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapability {
    /// Capability identifier (e.g., "memory.recall").
    pub id: String,
    /// Human-readable description.
    pub description: String,
    /// Input schema (JSON Schema).
    pub input_schema: serde_json::Value,
    /// Output schema (JSON Schema).
    pub output_schema: serde_json::Value,
}

/// Authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentAuth {
    /// Auth type: `api_key`, `oauth2`, or `none`.
    pub auth_type: String,
    /// Header name for API key auth.
    pub header: Option<String>,
}

/// A2A task request from another agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATaskRequest {
    /// Unique task ID.
    pub task_id: String,
    /// Capability being invoked.
    pub capability: String,
    /// Input parameters.
    pub input: serde_json::Value,
    /// Requesting agent's identity.
    pub from_agent: String,
}

/// A2A task response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2ATaskResponse {
    /// Task ID (echo back).
    pub task_id: String,
    /// Status: "completed", "failed", "pending".
    pub status: String,
    /// Output data.
    pub output: serde_json::Value,
    /// Error message (if failed).
    pub error: Option<String>,
}

impl AgentCard {
    /// Build the default Pensyve agent card.
    pub fn pensyve_default(endpoint: &str) -> Self {
        Self {
            name: "pensyve-memory".to_string(),
            description: "Universal memory runtime for AI agents — recall, remember, and forget across sessions".to_string(),
            protocol: "a2a/v1".to_string(),
            capabilities: vec![
                AgentCapability {
                    id: "memory.recall".to_string(),
                    description: "Query memories by semantic similarity".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "limit": {"type": "integer", "default": 5},
                            "entity": {"type": "string"}
                        },
                        "required": ["query"]
                    }),
                    output_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "memories": {"type": "array"}
                        }
                    }),
                },
                AgentCapability {
                    id: "memory.remember".to_string(),
                    description: "Store a new memory".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "entity": {"type": "string"},
                            "fact": {"type": "string"},
                            "confidence": {"type": "number", "default": 0.8}
                        },
                        "required": ["entity", "fact"]
                    }),
                    output_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "memory_id": {"type": "string"}
                        }
                    }),
                },
                AgentCapability {
                    id: "memory.forget".to_string(),
                    description: "Delete memories for an entity".to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "entity": {"type": "string"}
                        },
                        "required": ["entity"]
                    }),
                    output_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "forgotten_count": {"type": "integer"}
                        }
                    }),
                },
            ],
            endpoint: endpoint.to_string(),
            auth: AgentAuth {
                auth_type: "api_key".to_string(),
                header: Some("X-Pensyve-Key".to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_card_default() {
        let card = AgentCard::pensyve_default("http://localhost:8000");
        assert_eq!(card.name, "pensyve-memory");
        assert_eq!(card.protocol, "a2a/v1");
        assert_eq!(card.capabilities.len(), 3);
        assert_eq!(card.endpoint, "http://localhost:8000");
    }

    #[test]
    fn test_agent_card_serialization() {
        let card = AgentCard::pensyve_default("http://localhost:8000");
        let json = serde_json::to_string(&card).unwrap();
        assert!(json.contains("pensyve-memory"));
        assert!(json.contains("memory.recall"));
        assert!(json.contains("memory.remember"));
        assert!(json.contains("memory.forget"));
    }

    #[test]
    fn test_task_request_deserialization() {
        let json = r#"{
            "task_id": "task-123",
            "capability": "memory.recall",
            "input": {"query": "user preferences", "limit": 5},
            "from_agent": "coding-assistant"
        }"#;
        let req: A2ATaskRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.task_id, "task-123");
        assert_eq!(req.capability, "memory.recall");
        assert_eq!(req.from_agent, "coding-assistant");
    }

    #[test]
    fn test_task_response_serialization() {
        let resp = A2ATaskResponse {
            task_id: "task-123".to_string(),
            status: "completed".to_string(),
            output: serde_json::json!({"memories": []}),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("completed"));
    }
}
