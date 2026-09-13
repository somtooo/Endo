use anyhow::Result;
use serde_json::Value;

use crate::migrations::migrate_settings;

pub fn make_expand_terminal_card_an_enum(value: &mut Value) -> Result<()> {
    migrate_settings(value, &mut migrate_one)
}

fn migrate_one(obj: &mut serde_json::Map<String, Value>) -> Result<()> {
    let Some(display) = obj
        .get_mut("agent")
        .and_then(|agent| agent.as_object_mut())
        .and_then(|agent| agent.get_mut("expand_terminal_card"))
    else {
        return Ok(());
    };

    *display = match display {
        Value::Bool(true) => Value::String("always_expanded".to_string()),
        Value::Bool(false) => Value::String("always_collapsed".to_string()),
        Value::String(display)
            if matches!(
                display.as_str(),
                "auto" | "always_expanded" | "always_collapsed"
            ) =>
        {
            return Ok(());
        }
        _ => {
            anyhow::bail!("Expected expand_terminal_card to be a boolean or valid enum value")
        }
    };

    Ok(())
}
