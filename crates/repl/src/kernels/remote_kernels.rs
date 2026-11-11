use futures::{SinkExt as _, channel::mpsc};
use gpui::{App, Entity, Task, Window};
use http_client::{AsyncBody, HttpClient, Request};
use jupyter_protocol::{ExecutionState, JupyterKernelspec, JupyterMessage, KernelInfoReply};

use async_tungstenite::tokio::connect_async_with_tls_connector;
use async_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};

use futures::StreamExt;
use smol::io::AsyncReadExt as _;

use super::{KernelSession, RunningKernel};
use anyhow::Result;
use jupyter_websocket_client::{
    JupyterWebSocket, JupyterWebSocketReader, JupyterWebSocketWriter, KernelLaunchRequest,
    KernelSpecsResponse, ProtocolMode, RemoteServer,
};
use std::{fmt::Debug, sync::Arc};

#[derive(Debug, Clone)]
pub struct RemoteKernelSpecification {
    pub name: String,
    /// The name of the server, from the settings.
    pub server_name: String,
    pub url: String,
    pub token: String,
    /// Headers every request to the server carries, such as the proxy token
    /// of a Colab runtime.
    pub headers: Vec<(String, String)>,
    pub kernelspec: JupyterKernelspec,
}

fn with_headers(
    mut request: http_client::Builder,
    headers: &[(String, String)],
) -> http_client::Builder {
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    request
}

pub async fn launch_remote_kernel(
    remote_server: &RemoteServer,
    headers: &[(String, String)],
    http_client: Arc<dyn HttpClient>,
    kernel_name: &str,
    _path: &str,
) -> Result<String> {
    //
    let kernel_launch_request = KernelLaunchRequest {
        name: kernel_name.to_string(),
        // Note: since the path we have locally may not be the same as the one on the remote server,
        // we don't send it. We'll have to evaluate this decision along the way.
        path: None,
    };

    let kernel_launch_request = serde_json::to_string(&kernel_launch_request)?;

    let request = with_headers(Request::builder(), headers)
        .method("POST")
        .uri(&remote_server.api_url("/kernels"))
        .header("Authorization", format!("token {}", remote_server.token))
        .body(AsyncBody::from(kernel_launch_request))?;

    let response = http_client.send(request).await?;

    if !response.status().is_success() {
        let mut body = String::new();
        response.into_body().read_to_string(&mut body).await?;
        anyhow::bail!("Failed to launch kernel: {body}");
    }

    let mut body = String::new();
    response.into_body().read_to_string(&mut body).await?;

    let response: jupyter_websocket_client::Kernel = serde_json::from_str(&body)?;

    Ok(response.id)
}

pub async fn list_remote_kernelspecs(
    server_name: String,
    remote_server: RemoteServer,
    headers: Vec<(String, String)>,
    http_client: Arc<dyn HttpClient>,
) -> Result<Vec<RemoteKernelSpecification>> {
    let url = remote_server.api_url("/kernelspecs");

    let request = with_headers(Request::builder(), &headers)
        .method("GET")
        .uri(&url)
        .header("Authorization", format!("token {}", remote_server.token))
        .body(AsyncBody::default())?;

    let response = http_client.send(request).await?;

    anyhow::ensure!(
        response.status().is_success(),
        "Failed to fetch kernel specs: {}",
        response.status()
    );
    let mut body = response.into_body();

    let mut body_bytes = Vec::new();
    body.read_to_end(&mut body_bytes).await?;

    let kernel_specs: KernelSpecsResponse = serde_json::from_slice(&body_bytes)?;

    let remote_kernelspecs = kernel_specs
        .kernelspecs
        .into_iter()
        .map(|(name, spec)| RemoteKernelSpecification {
            name,
            server_name: server_name.clone(),
            url: remote_server.base_url.clone(),
            token: remote_server.token.clone(),
            headers: headers.clone(),
            kernelspec: spec.spec,
        })
        .collect::<Vec<RemoteKernelSpecification>>();

    anyhow::ensure!(!remote_kernelspecs.is_empty(), "No kernel specs found");
    Ok(remote_kernelspecs)
}

/// Opens the websocket of a kernel that runs on `remote_server`.
pub async fn connect_to_kernel(
    remote_server: &RemoteServer,
    headers: &[(String, String)],
    kernel_id: &str,
    client_session: &str,
) -> Result<JupyterWebSocket> {
    let ws_url = format!(
        "{}&session_id={client_session}",
        kernel_channels_url(remote_server, kernel_id)?
    );
    let mut request: Request<()> = ws_url.into_client_request()?;
    for (name, value) in headers {
        request.headers_mut().insert(
            http_client::http::HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value)?,
        );
    }
    request.headers_mut().insert(
        "User-Agent",
        HeaderValue::from_str(&format!(
            "Zed/{} ({}; {})",
            "repl",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))?,
    );
    // The connector carries the platform's root certificates, which `wss://`
    // servers such as tunnelled Colab runtimes need.
    let (ws_stream, _response) = connect_async_with_tls_connector(
        request,
        Some(Arc::new(http_client_tls::tls_config()).into()),
    )
    .await?;
    // Not the binary v1 protocol: the websocket library rejects servers that
    // don't accept an offered subprotocol, and Colab's proxy doesn't.
    Ok(JupyterWebSocket {
        inner: ws_stream,
        protocol_mode: ProtocolMode::Json,
    })
}

/// The websocket URL of a kernel's channels. Only the scheme changes from the
/// server's URL, so `https` servers are reached over `wss`.
fn kernel_channels_url(remote_server: &RemoteServer, kernel_id: &str) -> Result<String> {
    let base_url = remote_server.base_url.trim_end_matches('/');
    let base_url = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("Jupyter server URL must start with http:// or https://: {base_url}");
    };
    Ok(format!(
        "{base_url}/api/kernels/{kernel_id}/channels?token={}",
        remote_server.token
    ))
}

impl PartialEq for RemoteKernelSpecification {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.url == other.url
    }
}

impl Eq for RemoteKernelSpecification {}

pub struct RemoteRunningKernel {
    remote_server: RemoteServer,
    headers: Vec<(String, String)>,
    _connection_task: Task<Result<()>>,
    http_client: Arc<dyn HttpClient>,
    pub working_directory: std::path::PathBuf,
    pub request_tx: mpsc::Sender<JupyterMessage>,
    pub stdin_tx: mpsc::Sender<JupyterMessage>,
    pub execution_state: ExecutionState,
    pub kernel_info: Option<KernelInfoReply>,
    pub kernel_id: String,
}

impl RemoteRunningKernel {
    pub fn new<S: KernelSession + 'static>(
        kernelspec: RemoteKernelSpecification,
        working_directory: std::path::PathBuf,
        session: Entity<S>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Box<dyn RunningKernel>>> {
        let remote_server = RemoteServer {
            base_url: kernelspec.url,
            token: kernelspec.token,
        };
        let headers = kernelspec.headers;

        let http_client = cx.http_client();

        // Like Jupyter's own clients, every message carries one session id,
        // which is also the websocket's `session_id`: proxies such as Colab's
        // route the kernel's output by it.
        let client_session = uuid::Uuid::new_v4().to_string();

        window.spawn(cx, async move |cx| {
            let kernel_id = launch_remote_kernel(
                &remote_server,
                &headers,
                http_client.clone(),
                &kernelspec.name,
                working_directory.to_str().unwrap_or_default(),
            )
            .await?;
            log::info!(
                "started remote kernel {kernel_id} ({}) on {}",
                kernelspec.name,
                remote_server.base_url
            );

            // Tokio's networking only works inside its runtime, which GPUI's
            // executors aren't.
            let kernel_socket = gpui_tokio::Tokio::spawn_result(cx, {
                let remote_server = RemoteServer {
                    base_url: remote_server.base_url.clone(),
                    token: remote_server.token.clone(),
                };
                let kernel_id = kernel_id.clone();
                let headers = headers.clone();
                let client_session = client_session.clone();
                async move {
                    connect_to_kernel(&remote_server, &headers, &kernel_id, &client_session).await
                }
            })
            .await?;
            log::info!("connected to remote kernel {kernel_id}");
            let (request_tx, request_rx) = futures::channel::mpsc::channel::<JupyterMessage>(100);

            let connection_task = cx.spawn({
                let session = session.clone();
                let remote_server = RemoteServer {
                    base_url: remote_server.base_url.clone(),
                    token: remote_server.token.clone(),
                };
                let headers = headers.clone();
                let kernel_id = kernel_id.clone();
                async move |cx| {
                    keep_connected(
                        kernel_socket,
                        remote_server,
                        headers,
                        kernel_id,
                        client_session,
                        request_rx,
                        session,
                        cx,
                    )
                    .await;
                    Ok(())
                }
            });

            let stdin_tx = request_tx.clone();

            anyhow::Ok(Box::new(Self {
                _connection_task: connection_task,
                remote_server,
                headers,
                working_directory,
                request_tx,
                stdin_tx,
                // todo(kyle): pull this from the kernel API to start with
                execution_state: ExecutionState::Idle,
                kernel_info: None,
                kernel_id,
                http_client: http_client.clone(),
            }) as Box<dyn RunningKernel>)
        })
    }
}

/// How often an idle connection sends a `kernel_info_request`. Proxies in
/// front of remote kernels, like Colab's, close websockets that carry
/// nothing for about half a minute.
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const MAX_RECONNECT_ATTEMPTS: u32 = 5;

/// Carries messages between `session` and a remote kernel's websocket:
/// sends a `kernel_info_request` on connecting and when idle, whose replies
/// it keeps from the session, and reconnects to the same kernel when the
/// connection drops, resending the message that failed.
async fn keep_connected<S: KernelSession + 'static>(
    first_socket: JupyterWebSocket,
    remote_server: RemoteServer,
    headers: Vec<(String, String)>,
    kernel_id: String,
    client_session: String,
    mut requests: mpsc::Receiver<JupyterMessage>,
    session: Entity<S>,
    cx: &mut gpui::AsyncWindowContext,
) {
    use futures::{FutureExt as _, select_biased};

    let mut socket = Some(first_socket);
    let mut unsent: Option<JupyterMessage> = None;
    let mut keepalive_ids = std::collections::VecDeque::<String>::new();
    let mut failed_attempts = 0;
    loop {
        let current = match socket.take() {
            Some(current) => current,
            None => {
                let connected = gpui_tokio::Tokio::spawn_result(cx, {
                    let remote_server = RemoteServer {
                        base_url: remote_server.base_url.clone(),
                        token: remote_server.token.clone(),
                    };
                    let headers = headers.clone();
                    let kernel_id = kernel_id.clone();
                    let client_session = client_session.clone();
                    async move {
                        connect_to_kernel(&remote_server, &headers, &kernel_id, &client_session)
                            .await
                    }
                })
                .await;
                match connected {
                    Ok(connected) => {
                        log::info!("reconnected to remote kernel {kernel_id}");
                        connected
                    }
                    Err(error) => {
                        failed_attempts += 1;
                        log::error!(
                            "remote kernel {kernel_id}: can't reconnect (attempt {failed_attempts}): {error:#}"
                        );
                        if failed_attempts >= MAX_RECONNECT_ATTEMPTS {
                            session.update(cx, |session, cx| {
                                session.kernel_errored(
                                    format!("lost the connection to the kernel: {error:#}"),
                                    cx,
                                )
                            });
                            return;
                        }
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(2u64.pow(failed_attempts)))
                            .await;
                        continue;
                    }
                }
            }
        };
        failed_attempts = 0;
        let (mut writer, mut reader): (JupyterWebSocketWriter, JupyterWebSocketReader) =
            current.split();

        let mut pending = unsent.take().into_iter().collect::<Vec<_>>();
        pending.insert(0, keepalive_request(&mut keepalive_ids));
        let mut connection_lost = false;
        for message in pending {
            let message = outgoing(message, &client_session);
            if let Err(error) = writer.send(message.clone()).await {
                log::error!(
                    "remote kernel {kernel_id}: can't send {}: {error:#}",
                    message.header.msg_type
                );
                unsent = Some(message);
                connection_lost = true;
                break;
            }
        }

        let mut received = 0usize;
        while !connection_lost {
            let mut timer = cx.background_executor().timer(KEEPALIVE_INTERVAL).fuse();
            select_biased! {
                message = reader.next().fuse() => match message {
                    Some(Ok(message)) => {
                        received += 1;
                        if received <= 5 {
                            log::info!(
                                "remote kernel {kernel_id}: received {}",
                                message.header.msg_type
                            );
                        }
                        let is_keepalive = message
                            .parent_header
                            .as_ref()
                            .is_some_and(|parent| keepalive_ids.contains(&parent.msg_id));
                        if !is_keepalive {
                            session
                                .update_in(cx, |session, window, cx| {
                                    session.route(&message, window, cx);
                                })
                                .ok();
                        }
                    }
                    Some(Err(error)) => {
                        log::error!("remote kernel {kernel_id}: error receiving a message: {error:#}");
                    }
                    None => connection_lost = true,
                },
                request = requests.next().fuse() => match request {
                    Some(message) => {
                        let message = outgoing(message, &client_session);
                        log::debug!("remote kernel {kernel_id}: sending {}", message.header.msg_type);
                        if let Err(error) = writer.send(message.clone()).await {
                            log::error!(
                                "remote kernel {kernel_id}: can't send {}: {error:#}",
                                message.header.msg_type
                            );
                            unsent = Some(message);
                            connection_lost = true;
                        }
                    }
                    // The kernel was shut down or dropped.
                    None => return,
                },
                _ = timer => {
                    let message = outgoing(keepalive_request(&mut keepalive_ids), &client_session);
                    if writer.send(message).await.is_err() {
                        connection_lost = true;
                    }
                }
            }
        }
        log::info!(
            "remote kernel {kernel_id}: connection closed after {received} messages; reconnecting"
        );
    }
}

/// Gives a message the connection's session and names its channel, which
/// the server routes it by. Messages are made with a fresh session each and
/// no channel.
fn outgoing(mut message: JupyterMessage, client_session: &str) -> JupyterMessage {
    use jupyter_protocol::{Channel, JupyterMessageContent};
    message.header.session = client_session.to_string();
    if message.channel.is_none() {
        message.channel = Some(match message.content {
            JupyterMessageContent::InputReply(_) => Channel::Stdin,
            JupyterMessageContent::ShutdownRequest(_)
            | JupyterMessageContent::InterruptRequest(_)
            | JupyterMessageContent::DebugRequest(_) => Channel::Control,
            _ => Channel::Shell,
        });
    }
    message
}

/// A `kernel_info_request` whose replies the connection keeps to itself.
fn keepalive_request(keepalive_ids: &mut std::collections::VecDeque<String>) -> JupyterMessage {
    let message: JupyterMessage = jupyter_protocol::KernelInfoRequest {}.into();
    keepalive_ids.push_back(message.header.msg_id.clone());
    if keepalive_ids.len() > 16 {
        keepalive_ids.pop_front();
    }
    message
}

impl Debug for RemoteRunningKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRunningKernel")
            // custom debug that keeps tokens out of logs
            .field("remote_server url", &self.remote_server.base_url)
            .field("working_directory", &self.working_directory)
            .field("request_tx", &self.request_tx)
            .field("execution_state", &self.execution_state)
            .field("kernel_info", &self.kernel_info)
            .finish()
    }
}

impl RunningKernel for RemoteRunningKernel {
    fn request_tx(&self) -> futures::channel::mpsc::Sender<jupyter_protocol::JupyterMessage> {
        self.request_tx.clone()
    }

    fn stdin_tx(&self) -> futures::channel::mpsc::Sender<jupyter_protocol::JupyterMessage> {
        self.stdin_tx.clone()
    }

    fn working_directory(&self) -> &std::path::PathBuf {
        &self.working_directory
    }

    fn execution_state(&self) -> &jupyter_protocol::ExecutionState {
        &self.execution_state
    }

    fn set_execution_state(&mut self, state: jupyter_protocol::ExecutionState) {
        self.execution_state = state;
    }

    fn kernel_info(&self) -> Option<&jupyter_protocol::KernelInfoReply> {
        self.kernel_info.as_ref()
    }

    fn set_kernel_info(&mut self, info: jupyter_protocol::KernelInfoReply) {
        self.kernel_info = Some(info);
    }

    fn force_shutdown(&mut self, window: &mut Window, cx: &mut App) -> Task<anyhow::Result<()>> {
        let url = self
            .remote_server
            .api_url(&format!("/kernels/{}", self.kernel_id));
        let token = self.remote_server.token.clone();
        let headers = self.headers.clone();
        let http_client = self.http_client.clone();

        window.spawn(cx, async move |_| {
            let request = with_headers(Request::builder(), &headers)
                .method("DELETE")
                .uri(&url)
                .header("Authorization", format!("token {}", token))
                .body(AsyncBody::default())?;

            let response = http_client.send(request).await?;

            anyhow::ensure!(
                response.status().is_success(),
                "Failed to shutdown kernel: {}",
                response.status()
            );
            Ok(())
        })
    }

    fn kill(&mut self) {
        self.request_tx.close_channel();
        self.stdin_tx.close_channel();
    }
}

#[cfg(test)]
mod test {
    use std::time::{Duration, Instant};

    use jupyter_protocol::{ExecuteRequest, JupyterMessage, JupyterMessageContent};

    use super::*;

    #[test]
    fn test_outgoing_messages_share_a_session_and_name_their_channel() {
        use jupyter_protocol::{Channel, ExecuteRequest, InterruptRequest};
        let execute = outgoing(ExecuteRequest::new("1".to_string()).into(), "client");
        let interrupt = outgoing(InterruptRequest {}.into(), "client");
        assert_eq!(execute.header.session, "client");
        assert_eq!(interrupt.header.session, "client");
        assert!(matches!(execute.channel, Some(Channel::Shell)));
        assert!(matches!(interrupt.channel, Some(Channel::Control)));
        let json = serde_json::to_value(&execute).map_err(anyhow::Error::from);
        assert_eq!(
            json.ok().and_then(|json| json.get("channel").cloned()),
            Some(serde_json::json!("shell"))
        );
    }

    #[test]
    fn test_kernel_channels_url() {
        let server = |base_url: &str| RemoteServer {
            base_url: base_url.to_string(),
            token: "abc".to_string(),
        };
        assert_eq!(
            kernel_channels_url(&server("https://my-http-tunnel.trycloudflare.com/"), "k1").ok(),
            Some(
                "wss://my-http-tunnel.trycloudflare.com/api/kernels/k1/channels?token=abc"
                    .to_string()
            )
        );
        assert_eq!(
            kernel_channels_url(&server("http://127.0.0.1:8888"), "k1").ok(),
            Some("ws://127.0.0.1:8888/api/kernels/k1/channels?token=abc".to_string())
        );
        assert!(kernel_channels_url(&server("ftp://host"), "k1").is_err());
    }

    use crate::kernels::test_support::{start_jupyter_server, wait_for_server};
    use gpui::AppContext as _;

    struct RecordingSession {
        output: Option<futures::channel::oneshot::Sender<String>>,
    }

    impl KernelSession for RecordingSession {
        fn route(&mut self, message: &JupyterMessage, _: &mut Window, _: &mut gpui::Context<Self>) {
            if let JupyterMessageContent::StreamContent(stream) = &message.content
                && let Some(output) = self.output.take()
            {
                output.send(stream.text.clone()).ok();
            }
        }

        fn kernel_errored(&mut self, error_message: String, _: &mut gpui::Context<Self>) {
            panic!("the kernel errored: {error_message}");
        }
    }

    /// Runs a cell through `RemoteRunningKernel` on GPUI's executors, as the
    /// notebook editor and REPL do, against a real Jupyter server.
    #[gpui::test]
    async fn test_remote_running_kernel(cx: &mut gpui::TestAppContext) {
        run_remote_running_kernel(cx)
            .await
            .expect("running a cell on the remote kernel should work");
    }

    async fn run_remote_running_kernel(cx: &mut gpui::TestAppContext) -> Result<()> {
        let token = "fool-test-token";
        let Some((_server, base_url)) = start_jupyter_server(token) else {
            eprintln!("skipping: jupyter isn't installed (set JUPYTER_TEST_BIN)");
            return Ok(());
        };
        wait_for_server(&base_url)?;
        cx.executor().allow_parking();
        let http_client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        cx.update(|cx| {
            gpui_tokio::init(cx);
            cx.set_http_client(http_client.clone());
        });

        let remote_server = RemoteServer::from_url(&format!("{base_url}/?token={token}"))?;
        let specs =
            list_remote_kernelspecs("local".into(), remote_server, Vec::new(), http_client).await?;
        let spec = specs
            .into_iter()
            .find(|spec| spec.kernelspec.language == "python")
            .ok_or_else(|| anyhow::anyhow!("the server should have a Python kernel"))?;

        let (output_tx, output_rx) = futures::channel::oneshot::channel();
        let cx = cx.add_empty_window();
        let session = cx.update(|_, cx| {
            cx.new(|_| RecordingSession {
                output: Some(output_tx),
            })
        });
        let kernel = cx
            .update(|window, cx| {
                RemoteRunningKernel::new(spec, std::env::temp_dir(), session.clone(), window, cx)
            })
            .await?;
        let mut request_tx = kernel.request_tx();
        let request: JupyterMessage = ExecuteRequest::new("print(6 * 7)".to_string()).into();
        request_tx.send(request).await?;

        let (timeout_tx, timeout_rx) = futures::channel::oneshot::channel::<()>();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            timeout_tx.send(()).ok();
        });
        let output = futures::select! {
            output = futures::FutureExt::fuse(output_rx) => output?,
            _ = futures::FutureExt::fuse(timeout_rx) => anyhow::bail!("the kernel didn't answer"),
        };
        assert_eq!(output.trim(), "42");
        Ok(())
    }

    /// A server added to the `jupyter.servers` setting shows up with its
    /// kernels in the kernel picker's list.
    #[gpui::test]
    async fn test_servers_from_settings_are_listed(cx: &mut gpui::TestAppContext) {
        let token = "fool-test-token";
        let Some((_server, base_url)) = start_jupyter_server(token) else {
            eprintln!("skipping: jupyter isn't installed (set JUPYTER_TEST_BIN)");
            return;
        };
        wait_for_server(&base_url).expect("the Jupyter server should start");
        cx.executor().allow_parking();
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            cx.set_http_client(Arc::new(reqwest_client::ReqwestClient::new()));
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.jupyter.get_or_insert_default().servers =
                        Some(vec![settings::JupyterServerContent {
                            name: Some("local".to_string()),
                            url: Some(format!("{base_url}/?token={token}")),
                        }]);
                });
            });
        });
        let fs = project::FakeFs::new(cx.executor());
        cx.update(|cx| crate::ReplStore::init(fs, cx));
        let store = cx.update(|cx| crate::ReplStore::global(cx));
        store
            .update(cx, |store, cx| store.refresh_kernelspecs(cx))
            .await
            .expect("refreshing kernels should work");
        let server_names = store.read_with(cx, |store, _| {
            store
                .pure_jupyter_kernel_specifications()
                .filter_map(|spec| match spec {
                    crate::KernelSpecification::JupyterServer(spec) => {
                        Some(spec.server_name.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        });
        assert!(
            server_names.iter().any(|name| name == "local"),
            "the server's kernels should be listed, got {server_names:?}"
        );
    }

    /// Runs code on a real Jupyter server the way a notebook connected to a
    /// remote runtime does: list kernels, launch one, connect to its
    /// websocket and execute a request.
    /// Colab's proxy answers the websocket handshake without a subprotocol.
    #[test]
    fn test_connects_to_servers_without_a_subprotocol() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await?;
                let mut socket = async_tungstenite::tokio::accept_async(stream).await?;
                let frame = socket
                    .next()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("the client sent nothing"))??;
                anyhow::Ok(frame.into_text()?.to_string())
            });

            let remote_server = RemoteServer {
                base_url: format!("http://{address}"),
                token: "token".to_string(),
            };
            let socket = connect_to_kernel(&remote_server, &[], "k1", "client").await?;
            let (mut writer, _reader) = socket.split();
            writer
                .send(outgoing(
                    jupyter_protocol::KernelInfoRequest {}.into(),
                    "client",
                ))
                .await?;

            let sent: serde_json::Value = serde_json::from_str(&server.await??)?;
            assert_eq!(sent["channel"], "shell");
            assert_eq!(sent["header"]["session"], "client");
            anyhow::Ok(())
        })
    }

    #[test]
    fn test_remote_kernel_end_to_end() -> Result<()> {
        let token = "fool-test-token";
        let Some((_server, base_url)) = start_jupyter_server(token) else {
            eprintln!("skipping: jupyter isn't installed (set JUPYTER_TEST_BIN)");
            return Ok(());
        };
        let remote_server = RemoteServer::from_url(&format!("{base_url}/?token={token}"))?;
        let http_client: Arc<dyn HttpClient> = Arc::new(reqwest_client::ReqwestClient::new());
        let runtime = tokio::runtime::Runtime::new()?;

        runtime.block_on(async {
            let started = Instant::now();
            let specs = loop {
                let remote_server = RemoteServer {
                    base_url: remote_server.base_url.clone(),
                    token: remote_server.token.clone(),
                };
                match list_remote_kernelspecs(
                    "test".into(),
                    remote_server,
                    Vec::new(),
                    http_client.clone(),
                )
                .await
                {
                    Ok(specs) => break specs,
                    Err(error) if started.elapsed() > Duration::from_secs(60) => {
                        return Err(error.context("the Jupyter server didn't start"));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
                }
            };
            let spec = specs
                .iter()
                .find(|spec| spec.kernelspec.language == "python")
                .ok_or_else(|| anyhow::anyhow!("the server should have a Python kernel"))?;
            assert_eq!(spec.server_name, "test");

            let kernel_id =
                launch_remote_kernel(&remote_server, &[], http_client.clone(), &spec.name, "")
                    .await?;
            let socket = connect_to_kernel(&remote_server, &[], &kernel_id, "test-session").await?;
            let (mut writer, mut reader) = socket.split();

            let request = outgoing(
                ExecuteRequest::new("print(6 * 7)".to_string()).into(),
                "test-session",
            );
            writer.send(request).await?;

            let output = tokio::time::timeout(Duration::from_secs(60), async {
                while let Some(message) = reader.next().await {
                    if let JupyterMessageContent::StreamContent(stream) = message?.content
                        && matches!(stream.name, jupyter_protocol::Stdio::Stdout)
                    {
                        return anyhow::Ok(stream.text);
                    }
                }
                anyhow::bail!("the kernel's websocket closed before any output")
            })
            .await
            .map_err(|_| anyhow::anyhow!("the kernel didn't answer"))??;
            assert_eq!(output.trim(), "42");

            let shutdown = Request::builder()
                .method("DELETE")
                .uri(remote_server.api_url(&format!("/kernels/{kernel_id}")))
                .header("Authorization", format!("token {}", remote_server.token))
                .body(AsyncBody::default())?;
            http_client.send(shutdown).await?;
            anyhow::Ok(())
        })
    }
}
