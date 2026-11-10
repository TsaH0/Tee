use collections::HashMap;

use editor::EditorSettings;
use gpui::App;
use settings::{RegisterSetting, Settings};

#[derive(Debug, Default, RegisterSetting)]
pub struct JupyterSettings {
    pub kernel_selections: HashMap<String, String>,
    pub servers: Vec<JupyterServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JupyterServer {
    pub name: String,
    pub url: String,
}

impl JupyterSettings {
    pub fn enabled(cx: &App) -> bool {
        // In order to avoid a circular dependency between `editor` and `repl` crates,
        // we put the `enable` flag on its settings.
        // This allows the editor to set up context for key bindings/actions.
        EditorSettings::jupyter_enabled(cx)
    }
}

impl Settings for JupyterSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let jupyter = content.editor.jupyter.clone().unwrap();
        let servers = jupyter
            .servers
            .unwrap_or_default()
            .into_iter()
            .filter_map(|server| {
                let url = server.url?;
                Some(JupyterServer {
                    name: server.name.unwrap_or_else(|| url_host(&url)),
                    url,
                })
            })
            .collect();
        Self {
            kernel_selections: jupyter.kernel_selections.unwrap_or_default(),
            servers,
        }
    }
}

/// The host of `url`, to name servers that weren't given a name.
pub fn url_host(url: &str) -> String {
    let without_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    without_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(without_scheme)
        .to_string()
}
