use std::sync::Arc;

use nomifun_system::{ClientPrefService, LazyLocalModelRuntime, ProviderService};

use crate::shell::ShellService;
use crate::stt::SttService;

#[derive(Clone)]
pub struct ShellRouterState {
    pub shell_service: Arc<ShellService>,
    pub stt_service: Arc<SttService>,
    pub client_pref_service: ClientPrefService,
    pub provider_service: Option<ProviderService>,
    pub lazy_local_model_runtime: Option<Arc<LazyLocalModelRuntime>>,
}
