//! Connecting to remote Jupyter servers, such as a Google Colab runtime,
//! whose kernels then run notebooks and REPL code.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, ClipboardItem, Context, DismissEvent, Task, WeakEntity, Window, actions,
};
use jupyter_websocket_client::RemoteServer;
use picker::{Picker, PickerDelegate};
use settings::update_settings_file;
use ui::{ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::{ReplStore, jupyter_settings::url_host, kernels::list_remote_kernelspecs};

actions!(
    repl,
    [
        /// Connects to a remote Jupyter server by its URL, so that its kernels
        /// can run notebooks and REPL code.
        ConnectToJupyterServer,
        /// Copies the code that starts a Jupyter server in a Google Colab
        /// notebook, and connects to the URL it prints, to run code on Colab's
        /// GPUs and CPUs.
        ConnectToColab,
    ]
);

/// Run in a Google Colab notebook, this starts a Jupyter server with a fresh
/// token, exposes it through a Cloudflare quick tunnel and prints the URL to
/// connect to.
pub const COLAB_RUNTIME_CELL: &str = r#"# Run this cell in Google Colab, then paste the printed URL into Fool's
# `repl: connect to colab`. Keep this tab open while you use the runtime.
import re, secrets, subprocess, sys, threading

token = secrets.token_hex(24)
# Colab ships a Jupyter server; upgrading it could break the Colab tab itself.
try:
    import jupyter_server
except ImportError:
    subprocess.run([sys.executable, "-m", "pip", "install", "-q", "jupyter_server"], check=True)
subprocess.run(["wget", "-q", "-O", "/usr/local/bin/cloudflared",
    "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64"], check=True)
subprocess.run(["chmod", "+x", "/usr/local/bin/cloudflared"], check=True)
subprocess.Popen([sys.executable, "-m", "jupyter", "server", "--no-browser", "--allow-root",
    "--ip=127.0.0.1", "--port=8890", "--ServerApp.port_retries=0",
    f"--ServerApp.token={token}", "--ServerApp.root_dir=/content"],
    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
tunnel = subprocess.Popen(["cloudflared", "tunnel", "--no-autoupdate", "--url", "http://127.0.0.1:8890"],
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
for line in tunnel.stdout:
    found = re.search(r"https://[a-z0-9-]+\.trycloudflare\.com", line)
    if found:
        print("Paste this into Fool's `repl: connect to colab`:")
        print(f"{found.group(0)}/?token={token}")
        break
# Keep reading the tunnel's log so that it never blocks on a full pipe.
threading.Thread(target=lambda: [None for _ in tunnel.stdout], daemon=True).start()
"#;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &ConnectToJupyterServer, window, cx| {
                open_prompt(workspace, ServerKind::Jupyter, window, cx);
            })
            .register_action(|workspace, _: &ConnectToColab, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(COLAB_RUNTIME_CELL.to_string()));
                open_prompt(workspace, ServerKind::Colab, window, cx);
            });
    })
    .detach();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerKind {
    Jupyter,
    Colab,
}

fn open_prompt(
    workspace: &mut Workspace,
    kind: ServerKind,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        Picker::uniform_list(
            ServerPromptDelegate {
                workspace: handle,
                kind,
                query: String::new(),
            },
            window,
            cx,
        )
        .initial_width(rems(48.))
    });
}

/// Parses what the user pasted into the server's name and connection.
fn parse_server_url(url: &str, kind: ServerKind) -> anyhow::Result<(String, RemoteServer)> {
    let url = url.trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("the URL must start with http:// or https://");
    }
    let server = RemoteServer::from_url(url)?;
    let name = match kind {
        ServerKind::Colab => "Colab".to_string(),
        ServerKind::Jupyter => url_host(url),
    };
    Ok((name, server))
}

pub(crate) fn notify(message: String, cx: &mut App) {
    struct JupyterServerNotification;
    workspace::notifications::show_app_notification(
        NotificationId::unique::<JupyterServerNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(|cx| MessageNotification::new(message, cx))
        },
    );
}

/// Checks that the server answers with its kernels, then saves it to the
/// settings, replacing any server with the same name.
fn connect(url: String, kind: ServerKind, cx: &mut App) {
    let (name, server) = match parse_server_url(&url, kind) {
        Ok(parsed) => parsed,
        Err(error) => {
            notify(format!("Can't connect to {url}: {error:#}"), cx);
            return;
        }
    };
    let http_client = cx.http_client();
    let fs = ReplStore::global(cx).read(cx).fs().clone();
    let url = url.trim().to_string();
    cx.spawn(async move |cx| {
        let result = list_remote_kernelspecs(name.clone(), server, Vec::new(), http_client).await;
        cx.update(|cx| match result {
            Ok(kernels) => {
                let kernel_names = kernels
                    .iter()
                    .map(|kernel| kernel.kernelspec.display_name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                let saved_name = name.clone();
                update_settings_file(fs, cx, move |settings, _| {
                    let servers = settings
                        .editor
                        .jupyter
                        .get_or_insert_default()
                        .servers
                        .get_or_insert_default();
                    servers.retain(|server| server.name.as_deref() != Some(saved_name.as_str()));
                    servers.push(settings::JupyterServerContent {
                        name: Some(saved_name),
                        url: Some(url),
                    });
                });
                notify(
                    format!(
                        "Connected to {name} ({kernel_names}). Pick its kernel in a notebook's \
                         kernel picker."
                    ),
                    cx,
                );
            }
            Err(error) => notify(format!("Can't connect to {name}: {error:#}"), cx),
        });
    })
    .detach();
}

struct ServerPromptDelegate {
    workspace: WeakEntity<Workspace>,
    kind: ServerKind,
    query: String,
}

impl PickerDelegate for ServerPromptDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "jupyter server prompt"
    }

    fn match_count(&self) -> usize {
        usize::from(!self.query.trim().is_empty())
    }

    fn selected_index(&self) -> usize {
        0
    }

    fn set_selected_index(&mut self, _: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        match self.kind {
            ServerKind::Colab => {
                "Run the copied cell in Colab, then paste the URL it prints…".into()
            }
            ServerKind::Jupyter => {
                "Paste a Jupyter server URL, like http://host:8888/?token=…".into()
            }
        }
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        let url = self.query.trim().to_string();
        if url.is_empty() {
            return;
        }
        if self.workspace.upgrade().is_some() {
            connect(url, self.kind, cx);
        }
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let label = match parse_server_url(&self.query, self.kind) {
            Ok((name, _)) => format!("Connect to {name}"),
            Err(error) => format!("{error:#}"),
        };
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(Label::new(label)),
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_parse_server_url() {
        let (name, server) = parse_server_url(
            " https://abc-def.trycloudflare.com/?token=xyz ",
            ServerKind::Colab,
        )
        .expect("a Colab URL should parse");
        assert_eq!(name, "Colab");
        assert_eq!(server.base_url, "https://abc-def.trycloudflare.com/");
        assert_eq!(server.token, "xyz");

        let (name, _) = parse_server_url("http://127.0.0.1:8888/tree?token=a", ServerKind::Jupyter)
            .expect("a Jupyter URL should parse");
        assert_eq!(name, "127.0.0.1:8888");

        assert!(parse_server_url("http://127.0.0.1:8888/", ServerKind::Jupyter).is_err());
        assert!(parse_server_url("127.0.0.1:8888/?token=a", ServerKind::Jupyter).is_err());
    }

    /// The cell runs in Colab's Python; at least check that it's valid
    /// Python and prints a URL in the format `parse_server_url` accepts.
    #[test]
    fn test_colab_cell_is_valid_python() {
        let Ok(output) = std::process::Command::new("python3")
            .args(["-c", "import ast, sys; ast.parse(sys.stdin.read())"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                if let Some(stdin) = child.stdin.as_mut() {
                    stdin.write_all(COLAB_RUNTIME_CELL.as_bytes())?;
                }
                child.wait_with_output()
            })
        else {
            eprintln!("skipping: python3 isn't installed");
            return;
        };
        assert!(
            output.status.success(),
            "the Colab cell should parse as Python"
        );
        assert!(COLAB_RUNTIME_CELL.contains("/?token={token}"));
    }
}
