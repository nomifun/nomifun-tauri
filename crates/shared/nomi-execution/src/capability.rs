use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SandboxPolicy {
    UnrestrictedLocalOwner,
    MacSeatbelt { write_roots: Vec<PathBuf> },
    DenyExecution,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityPolicy {
    pub cwd_roots: Vec<PathBuf>,
    pub sandbox: SandboxPolicy,
    pub allow_hand_off: bool,
}

impl CapabilityPolicy {
    pub fn local_owner(root: PathBuf) -> Self {
        Self {
            cwd_roots: vec![root],
            sandbox: SandboxPolicy::UnrestrictedLocalOwner,
            allow_hand_off: false,
        }
    }
}
