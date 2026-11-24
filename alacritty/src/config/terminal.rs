use serde::{Deserialize, Deserializer, Serialize, de};
use toml::Value;

use alacritty_config_derive::{ConfigDeserialize, SerdeReplace};
use alacritty_terminal::term::Osc52;

use crate::config::ui_config::{Program, StringVisitor};

#[derive(ConfigDeserialize, Serialize, Clone, Debug, PartialEq)]
pub struct Terminal {
    /// OSC52 support mode.
    pub osc52: SerdeOsc52,
    /// Support the Program Status Protocol (`OSC 7501`), through which agents report their
    /// status.
    pub program_status: bool,
    /// Show what each turn of an agent changed in its git checkout, see
    /// [`crate::turn_changes`].
    pub turn_changes: bool,
    /// Path to a shell program to run on startup.
    pub shell: Option<Program>,
}

impl Default for Terminal {
    fn default() -> Self {
        // Status reports only come from programs that look for support, so this is safe on.
        Self {
            osc52: Default::default(),
            program_status: true,
            turn_changes: true,
            shell: Default::default(),
        }
    }
}

#[derive(SerdeReplace, Serialize, Default, Copy, Clone, Debug, PartialEq)]
pub struct SerdeOsc52(pub Osc52);

impl<'de> Deserialize<'de> for SerdeOsc52 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserializer.deserialize_str(StringVisitor)?;
        Osc52::deserialize(Value::String(value)).map(SerdeOsc52).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_status_is_on_unless_turned_off() {
        assert!(Terminal::default().program_status);

        let empty: Terminal = toml::from_str("").unwrap();
        assert!(empty.program_status);

        let off: Terminal = toml::from_str("program_status = false").unwrap();
        assert!(!off.program_status);
    }

    #[test]
    fn turn_changes_are_on_unless_turned_off() {
        assert!(Terminal::default().turn_changes);
        let off: Terminal = toml::from_str("turn_changes = false").unwrap();
        assert!(!off.turn_changes);
    }
}
