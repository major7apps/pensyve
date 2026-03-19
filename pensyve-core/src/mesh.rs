//! Memory mesh -- access control for cross-entity memory sharing.

use chrono::{DateTime, Utc};
use uuid::Uuid;

/// Access role for namespace/entity permissions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    Owner,
    Writer,
    Reader,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Writer => "writer",
            Self::Reader => "reader",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "owner" => Self::Owner,
            "writer" => Self::Writer,
            _ => Self::Reader,
        }
    }
}

/// Visibility level for individual memories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Private,
    Shared,
    Public,
}

impl Visibility {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Shared => "shared",
            Self::Public => "public",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "shared" => Self::Shared,
            "public" => Self::Public,
            _ => Self::Private,
        }
    }
}

/// An access control entry.
#[derive(Debug, Clone)]
pub struct AclEntry {
    pub namespace_id: Uuid,
    pub entity_id: Uuid,
    pub role: Role,
    pub granted_by: Uuid,
    pub granted_at: DateTime<Utc>,
}

/// Check if an entity has sufficient access for an operation.
pub fn check_access(role: &Role, required: &Role) -> bool {
    match required {
        Role::Reader => true, // everyone can read
        Role::Writer => matches!(role, Role::Owner | Role::Writer),
        Role::Owner => matches!(role, Role::Owner),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_roundtrip() {
        let variants = [
            (Role::Owner, "owner"),
            (Role::Writer, "writer"),
            (Role::Reader, "reader"),
        ];
        for (role, expected_str) in &variants {
            assert_eq!(role.as_str(), *expected_str);
            assert_eq!(Role::from_str(expected_str), *role);
        }
    }

    #[test]
    fn test_role_unknown_fallback() {
        assert_eq!(Role::from_str("unknown"), Role::Reader);
        assert_eq!(Role::from_str(""), Role::Reader);
    }

    #[test]
    fn test_visibility_roundtrip() {
        let variants = [
            (Visibility::Private, "private"),
            (Visibility::Shared, "shared"),
            (Visibility::Public, "public"),
        ];
        for (vis, expected_str) in &variants {
            assert_eq!(vis.as_str(), *expected_str);
            assert_eq!(Visibility::from_str(expected_str), *vis);
        }
    }

    #[test]
    fn test_visibility_default() {
        let vis = Visibility::default();
        assert_eq!(vis, Visibility::Private);
    }

    #[test]
    fn test_visibility_unknown_fallback() {
        assert_eq!(Visibility::from_str("unknown"), Visibility::Private);
        assert_eq!(Visibility::from_str(""), Visibility::Private);
    }

    #[test]
    fn test_check_access_reader_required() {
        // Everyone can read.
        assert!(check_access(&Role::Owner, &Role::Reader));
        assert!(check_access(&Role::Writer, &Role::Reader));
        assert!(check_access(&Role::Reader, &Role::Reader));
    }

    #[test]
    fn test_check_access_writer_required() {
        assert!(check_access(&Role::Owner, &Role::Writer));
        assert!(check_access(&Role::Writer, &Role::Writer));
        assert!(!check_access(&Role::Reader, &Role::Writer));
    }

    #[test]
    fn test_check_access_owner_required() {
        assert!(check_access(&Role::Owner, &Role::Owner));
        assert!(!check_access(&Role::Writer, &Role::Owner));
        assert!(!check_access(&Role::Reader, &Role::Owner));
    }

    #[test]
    fn test_acl_entry_construction() {
        let entry = AclEntry {
            namespace_id: Uuid::new_v4(),
            entity_id: Uuid::new_v4(),
            role: Role::Writer,
            granted_by: Uuid::new_v4(),
            granted_at: Utc::now(),
        };
        assert_eq!(entry.role, Role::Writer);
        assert_ne!(entry.namespace_id, entry.entity_id);
    }
}
