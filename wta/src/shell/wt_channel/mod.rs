mod cli_channel;
mod pipe_channel;
mod routed_channel;

pub use cli_channel::CliChannel;
pub use cli_channel::spawn_wtcli_focus_pane;
pub use cli_channel::spawn_wtcli_split_then_focus_with_callback;
pub use pipe_channel::PipeChannel;
pub use routed_channel::RoutedChannel;
pub(crate) use cli_channel::resolve_wtcli_path;

/// Connection info discovered from environment variables.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub connection_id: String,
    pub source: DiscoverySource,
}

#[derive(Debug, Clone)]
pub enum DiscoverySource {
    ComClsid,
}

/// Discover WT protocol connection info from the WT_COM_CLSID env var.
pub fn discover_connection_info() -> Option<ConnectionInfo> {
    if let Ok(clsid) = std::env::var("WT_COM_CLSID") {
        return Some(ConnectionInfo {
            connection_id: clsid,
            source: DiscoverySource::ComClsid,
        });
    }
    None
}

/// Channel for communicating with the Windows Terminal protocol server.
#[async_trait::async_trait]
pub trait WtChannel: Send + Sync {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value>;

    fn is_available(&self) -> bool;
}
