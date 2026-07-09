//! Bridges `nomifun-channel`'s [`AssetResolver`] to the workshop asset store,
//! so channel replies can upload AI-generated images. Kept in `nomifun-app`
//! (not `nomifun-channel`) so the channel crate has no workshop dependency —
//! same layering as `CompanionMasterAgentProfile`.

use std::sync::Arc;

use nomifun_channel::message_service::AssetResolver;
use nomifun_channel::types::{MediaKind, OutgoingMedia};
use nomifun_workshop::WorkshopService;

pub struct ChannelAssetResolver {
    workshop: Arc<WorkshopService>,
}

impl ChannelAssetResolver {
    pub fn new(workshop: Arc<WorkshopService>) -> Self {
        Self { workshop }
    }
}

/// Suggested file extension for a mime type (Telegram/most platforms infer the
/// type from the mime part, but a sensible filename improves the UX).
fn ext_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

#[async_trait::async_trait]
impl AssetResolver for ChannelAssetResolver {
    async fn resolve(&self, asset_id: &str) -> Option<OutgoingMedia> {
        match self.workshop.read_asset_bytes(asset_id).await {
            Ok((bytes, mime)) => {
                let kind = if mime.starts_with("image/") {
                    MediaKind::Image
                } else {
                    MediaKind::File
                };
                let filename = format!("{asset_id}.{}", ext_for_mime(&mime));
                Some(OutgoingMedia { bytes, mime, filename, kind })
            }
            Err(e) => {
                tracing::warn!(asset_id = %asset_id, error = %e, "failed to read workshop asset for channel media");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_mapping() {
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("image/jpeg"), "jpg");
        assert_eq!(ext_for_mime("application/pdf"), "bin");
    }
}
