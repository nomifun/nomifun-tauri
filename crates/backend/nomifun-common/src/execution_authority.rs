//! Server-derived authority for code that can execute work on the host.
//!
//! This is deliberately an internal runtime policy, not a persisted product
//! concept and not a client-selectable DTO.  A principal either owns the local
//! installation or is confined to model-only execution.  Every host-capability
//! boundary derives the value from the authenticated/persisted user id and the
//! immutable installation owner; open-ended Conversation JSON can never grant
//! or widen it.

/// Maximum execution authority derived by the backend for one principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionAuthority {
    /// The canonical owner of this installation may control the host process,
    /// filesystem, configured tools and installation-wide domains.
    InstanceOwner,
    /// An authenticated non-owner may use model-only conversation features but
    /// cannot spawn host processes, mount installation data or receive tools.
    ModelOnly,
}

impl ExecutionAuthority {
    /// Resolve authority from two canonical identities.  Empty or non-canonical
    /// values are rejected by callers before resolution; equality is exact and
    /// intentionally has no alias/admin fallback.
    pub fn resolve(principal_user_id: &str, authoritative_user_id: &str) -> Self {
        if principal_user_id == authoritative_user_id {
            Self::InstanceOwner
        } else {
            Self::ModelOnly
        }
    }

    pub const fn controls_host(self) -> bool {
        matches!(self, Self::InstanceOwner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_OWNER_ID: &str = "0190f5fe-7c00-7a00-8000-000000000001";

    #[test]
    fn resolution_is_exact_and_never_treats_admin_as_owner() {
        assert_eq!(
            ExecutionAuthority::resolve(TEST_OWNER_ID, TEST_OWNER_ID),
            ExecutionAuthority::InstanceOwner
        );
        assert_eq!(
            ExecutionAuthority::resolve("admin", TEST_OWNER_ID),
            ExecutionAuthority::ModelOnly
        );
        assert_eq!(
            ExecutionAuthority::resolve(" 0190f5fe-7c00-7a00-8000-000000000001", TEST_OWNER_ID),
            ExecutionAuthority::ModelOnly
        );
    }
}
