use std::sync::Arc;

use nomifun_system::{ClientPrefService, ProviderService};

use crate::shell::ShellService;
use crate::stt::SttService;

#[derive(Clone)]
pub struct ShellRouterState {
    pub shell_service: Arc<ShellService>,
    pub stt_service: Arc<SttService>,
    pub client_pref_service: ClientPrefService,
    pub provider_service: Option<ProviderService>,
}
