//! Contains the [`VimModeSetting`] and [`HelixModeSetting`] used to enable/disable Vim and Helix modes.
//!
//! This is in its own crate as we want other crates to be able to enable or
//! disable Vim/Helix modes without having to depend on the `vim` crate in its
//! entirety.

use gpui::App;
use settings::{RegisterSetting, Settings, SettingsContent};

#[derive(RegisterSetting)]
pub struct VimModeSetting(pub bool);

impl Settings for VimModeSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        Self(content.vim_mode.unwrap())
    }
}

impl VimModeSetting {
    pub fn is_enabled(cx: &App) -> bool {
        Self::try_get(cx)
            .map(|vim_mode| vim_mode.0)
            .unwrap_or(false)
    }
}

#[derive(RegisterSetting)]
pub struct HelixModeSetting(pub bool);

impl HelixModeSetting {
    pub fn is_enabled(cx: &App) -> bool {
        Self::try_get(cx)
            .map(|helix_mode| helix_mode.0)
            .unwrap_or(false)
    }
}

impl Settings for HelixModeSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        Self(content.helix_mode.unwrap())
    }
}

/// Which modal keymaps are loaded on top of the base keymap.
#[derive(RegisterSetting, Clone, Debug, PartialEq)]
pub struct VimKeymapSetting {
    /// Whether the built-in Vim and Helix bindings are loaded.
    pub load_builtin: bool,
    /// Extra keymap files loaded after the built-in modal keymap.
    pub files: Vec<String>,
}

impl Settings for VimKeymapSetting {
    fn from_settings(content: &SettingsContent) -> Self {
        let keymap = content
            .vim
            .as_ref()
            .and_then(|vim| vim.keymap.clone())
            .unwrap_or_default();
        Self {
            load_builtin: keymap.load_builtin.unwrap_or(true),
            files: keymap.files.unwrap_or_default(),
        }
    }
}
