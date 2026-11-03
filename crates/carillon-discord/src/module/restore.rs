use serde_json::Value;
use tracing::warn;

use super::{DiscordApp, Module};

impl Module {
    /// Gives Discord `definitions` again after a change registered another
    /// module's commands and then failed. A failure here leaves the record
    /// not showing `definitions`, so the provider's next start registers
    /// them; the change's own failure is what the caller reports.
    pub(super) async fn restore(&self, app: Option<&DiscordApp>, definitions: &[Value]) {
        if let Err(error) = self.reconcile_definitions(app, definitions).await {
            warn!(%error, "Discord keeps the refused module's commands until the provider starts again");
        }
    }
}
