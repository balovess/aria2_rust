//! RPC adapter for the process-wide browser session context.

use aria2_core::http::{BrowserContextUpdate, global_browser_context};

pub fn update(value: serde_json::Value) -> Result<(), String> {
    let update: BrowserContextUpdate = serde_json::from_value(value)
        .map_err(|error| format!("invalid browser context: {error}"))?;
    global_browser_context().replace(update);
    Ok(())
}

pub fn clear() {
    global_browser_context().clear();
}
