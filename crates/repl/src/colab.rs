//! Google Colab runtimes through the Colab API, as Google's Colab extension
//! for VS Code uses it: sign in with Google, create a runtime on a GPU, TPU
//! or CPU, and run notebooks on its kernels, with no browser tab open.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use gpui::{App, AsyncApp, Context, DismissEvent, Global, Task, Window, actions};
use http_client::{AsyncBody, HttpClient, Method, Request};
use jupyter_websocket_client::RemoteServer;
use picker::{Picker, PickerDelegate};
use serde::Deserialize;
use serde_json::{Value, json};
use settings::Settings as _;
use sha2::Digest as _;
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use ui::{ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use workspace::Workspace;

use crate::{
    jupyter_servers::notify,
    jupyter_settings::{ColabClient, JupyterSettings},
};

actions!(
    repl,
    [
        /// Creates a Google Colab runtime on a GPU, TPU or CPU, signing in to
        /// Google first if needed, so that notebooks can run on its kernels.
        NewColabRuntime,
        /// Deletes one of your Colab runtimes, freeing what it uses of your
        /// quota.
        DeleteColabRuntime,
        /// Signs in to Google Colab.
        SignInToColab,
        /// Signs out of Google Colab.
        SignOutOfColab,
    ]
);

const API_URL: &str = "https://colaboratory.googleapis.com";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/colaboratory";
/// Where the refresh token is kept in the system keychain.
const CREDENTIALS_URL: &str = "https://colaboratory.googleapis.com/fool";
const PROXY_TOKEN_HEADER: &str = "X-Colab-Runtime-Proxy-Token";
/// Colab's runtime proxy only passes kernel traffic for the client agents it
/// knows; this is the one Colab's VS Code extension sends, whose protocol
/// Fool follows.
const CLIENT_AGENT_HEADER: &str = "X-Colab-Client-Agent";
const CLIENT_AGENT: &str = "vscode";
/// The Colab API waits up to 180s to create a runtime.
const WAIT_TIMEOUT: &str = "200s";
/// How long before a runtime's proxy token expires to refresh it.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
const SETUP_HINT: &str = "Add a Google OAuth client to your settings as \
    \"jupyter\": { \"colab\": { \"client_id\": \"…\", \"client_secret\": \"…\" } }. \
    See \"Running on Colab's GPUs and CPUs\" in Fool's docs for how to make one.";

/// The Colab runtimes of the signed-in user, whose kernels the kernel picker
/// lists.
#[derive(Default)]
pub struct ColabRuntimes {
    pub runtimes: Vec<ColabRuntime>,
    access_token: Option<(String, std::time::Instant)>,
    _refresh_task: Option<Task<()>>,
}

impl Global for ColabRuntimes {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColabRuntime {
    /// `runtimes/<id>`.
    pub name: String,
    pub label: String,
    pub url: String,
    pub token: String,
    pub expire_time: Option<DateTime<Utc>>,
}

/// The runtimes as Jupyter servers, with the header their proxy needs.
pub(crate) fn jupyter_servers(cx: &App) -> Vec<(String, RemoteServer, Vec<(String, String)>)> {
    let Some(runtimes) = cx.try_global::<ColabRuntimes>() else {
        return Vec::new();
    };
    runtimes
        .runtimes
        .iter()
        .map(|runtime| {
            (
                format!("Colab {}", runtime.label),
                RemoteServer {
                    base_url: runtime.url.clone(),
                    token: runtime.token.clone(),
                },
                vec![
                    (PROXY_TOKEN_HEADER.to_string(), runtime.token.clone()),
                    (CLIENT_AGENT_HEADER.to_string(), CLIENT_AGENT.to_string()),
                ],
            )
        })
        .collect()
}

pub(crate) fn init(cx: &mut App) {
    cx.set_global(ColabRuntimes::default());
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|workspace, _: &NewColabRuntime, window, cx| {
                new_runtime(workspace, window, cx);
            })
            .register_action(|workspace, _: &DeleteColabRuntime, window, cx| {
                delete_runtime(workspace, window, cx);
            })
            .register_action(|_, _: &SignInToColab, _, cx| {
                cx.spawn(async move |_, cx| {
                    let result = sign_in(cx).await;
                    cx.update(|cx| match result {
                        Ok(()) => {
                            notify("Signed in to Google Colab.".into(), cx);
                            reconnect(cx);
                        }
                        Err(error) => notify(format!("Can't sign in to Colab: {error:#}"), cx),
                    });
                })
                .detach();
            })
            .register_action(|_, _: &SignOutOfColab, _, cx| sign_out(cx));
    })
    .detach();

    if client(cx).is_ok() {
        reconnect(cx);
    }
}

fn client(cx: &App) -> Result<ColabClient> {
    JupyterSettings::get_global(cx)
        .colab_client
        .clone()
        .context(SETUP_HINT)
}

/// Lists the runtimes the user already has, if they're signed in.
fn reconnect(cx: &mut App) {
    cx.spawn(async move |cx| {
        let signed_in = match cx.update(|cx| cx.read_credentials(CREDENTIALS_URL)).await {
            Ok(credentials) => credentials.is_some(),
            Err(error) => {
                log::error!("can't read the Colab credentials: {error:#}");
                false
            }
        };
        if !signed_in {
            return;
        }
        if let Err(error) = refresh_runtimes(cx).await {
            log::error!("can't list Colab runtimes: {error:#}");
        }
    })
    .detach();
}

fn sign_out(cx: &mut App) {
    cx.update_global::<ColabRuntimes, _>(|runtimes, _| {
        runtimes.runtimes.clear();
        runtimes.access_token = None;
        runtimes._refresh_task = None;
    });
    let delete = cx.delete_credentials(CREDENTIALS_URL);
    cx.spawn(async move |cx| {
        let result = delete.await;
        cx.update(|cx| match result {
            Ok(()) => notify("Signed out of Google Colab.".into(), cx),
            Err(error) => notify(format!("Can't sign out of Colab: {error:#}"), cx),
        });
    })
    .detach();
}

/// The code verifier and challenge of PKCE, which keeps the authorization
/// code useless to anyone who intercepts it.
fn pkce_pair() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn authorization_url(client_id: &str, redirect_uri: &str, challenge: &str, state: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPE)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .finish();
    format!("{AUTH_URL}?{query}")
}

/// What the browser's redirect to the loopback server carries: the
/// authorization code, or `None` for unrelated requests such as the favicon.
fn parse_redirect(request_line: &str, state: &str) -> Option<Result<String>> {
    let target = request_line.split_whitespace().nth(1)?;
    let query = target.split_once('?')?.1;
    let mut code = None;
    let mut returned_state = None;
    let mut error = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => returned_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Some(Err(anyhow::anyhow!("Google refused the sign-in: {error}")));
    }
    let code = code?;
    if returned_state.as_deref() != Some(state) {
        return Some(Err(anyhow::anyhow!("the sign-in's state didn't match")));
    }
    Some(Ok(code))
}

/// Signs in with Google in the browser, which redirects back to a server on
/// localhost, and keeps the refresh token in the system keychain.
async fn sign_in(cx: &mut AsyncApp) -> Result<()> {
    let client = cx.update(|cx| client(cx))?;
    let http_client = cx.update(|cx| cx.http_client());
    let listener = smol::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let (verifier, challenge) = pkce_pair();
    let state = uuid::Uuid::new_v4().simple().to_string();
    let url = authorization_url(&client.id, &redirect_uri, &challenge, &state);
    cx.update(|cx| {
        cx.open_url(&url);
        notify("Sign in to Google in your browser to use Colab.".into(), cx);
    });

    let receive_code = async {
        loop {
            let (mut stream, _) = listener.accept().await?;
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            let request_line = request.lines().next().unwrap_or_default();
            let Some(result) = parse_redirect(request_line, &state) else {
                stream
                    .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                    .await?;
                continue;
            };
            let page = if result.is_ok() {
                "Signed in to Google Colab. You can close this tab and return to Fool."
            } else {
                "Signing in to Google Colab failed. Return to Fool for details."
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            );
            stream.write_all(response.as_bytes()).await?;
            stream.flush().await?;
            return result;
        }
    };
    let timeout = cx.background_executor().timer(Duration::from_secs(10 * 60));
    let code = futures::select_biased! {
        code = futures::FutureExt::fuse(receive_code) => code?,
        _ = futures::FutureExt::fuse(timeout) => anyhow::bail!("the sign-in timed out"),
    };

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client.id)
        .append_pair("client_secret", &client.secret)
        .append_pair("code", &code)
        .append_pair("code_verifier", &verifier)
        .append_pair("grant_type", "authorization_code")
        .append_pair("redirect_uri", &redirect_uri)
        .finish();
    let tokens: TokenResponse = serde_json::from_value(post_form(&http_client, body).await?)?;
    let refresh_token = tokens
        .refresh_token
        .context("Google didn't return a refresh token")?;
    cx.update(|cx| cx.write_credentials(CREDENTIALS_URL, "colab", refresh_token.as_bytes()))
        .await?;
    cx.update(|cx| {
        cx.update_global::<ColabRuntimes, _>(|runtimes, _| {
            runtimes.access_token = Some((
                tokens.access_token,
                std::time::Instant::now() + Duration::from_secs(tokens.expires_in),
            ));
        })
    });
    Ok(())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default = "default_expiry")]
    expires_in: u64,
    refresh_token: Option<String>,
}

fn default_expiry() -> u64 {
    3600
}

async fn post_form(http_client: &Arc<dyn HttpClient>, body: String) -> Result<Value> {
    let request = Request::builder()
        .method(Method::POST)
        .uri(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(body))?;
    read_json(http_client.send(request).await?).await
}

async fn read_json(response: http_client::Response<AsyncBody>) -> Result<Value> {
    let status = response.status();
    let mut body = String::new();
    response.into_body().read_to_string(&mut body).await?;
    if !status.is_success() {
        anyhow::bail!("{}", error_message(status.as_u16(), &body));
    }
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&body)?)
}

/// The message of a Google API error, which is either `{"error": {"message"}}`
/// or, from the OAuth endpoints, `{"error": "...", "error_description"}`.
fn error_message(status: u16, body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|value| {
        value["error"]["message"]
            .as_str()
            .or_else(|| value["error_description"].as_str())
            .or_else(|| value["error"].as_str())
            .map(str::to_string)
    });
    let reason = parsed.as_ref().and_then(|value| {
        value["error"]["details"]
            .as_array()?
            .iter()
            .find_map(|detail| detail["reason"].as_str())
            .map(str::to_string)
    });
    let message = message.unwrap_or_else(|| body.trim().to_string());
    match reason {
        Some(reason) => format!("{message} ({reason}, HTTP {status})"),
        None => format!("{message} (HTTP {status})"),
    }
}

/// A fresh access token, from the refresh token in the keychain. Signs in
/// first when there's none.
async fn access_token(cx: &mut AsyncApp) -> Result<String> {
    let cached = cx.update(|cx| {
        cx.global::<ColabRuntimes>()
            .access_token
            .clone()
            .filter(|(_, expiry)| *expiry > std::time::Instant::now() + Duration::from_secs(60))
            .map(|(token, _)| token)
    });
    if let Some(token) = cached {
        return Ok(token);
    }
    let client = cx.update(|cx| client(cx))?;
    let http_client = cx.update(|cx| cx.http_client());
    let credentials = cx.update(|cx| cx.read_credentials(CREDENTIALS_URL)).await?;
    let Some((_, refresh_token)) = credentials else {
        sign_in(cx).await?;
        return cx.update(|cx| {
            cx.global::<ColabRuntimes>()
                .access_token
                .clone()
                .map(|(token, _)| token)
                .context("the sign-in didn't give an access token")
        });
    };
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client.id)
        .append_pair("client_secret", &client.secret)
        .append_pair("refresh_token", &String::from_utf8(refresh_token)?)
        .append_pair("grant_type", "refresh_token")
        .finish();
    let tokens: TokenResponse = serde_json::from_value(
        post_form(&http_client, body)
            .await
            .context("signing in to Colab again failed; run `repl: sign in to colab`")?,
    )?;
    cx.update(|cx| {
        cx.update_global::<ColabRuntimes, _>(|runtimes, _| {
            runtimes.access_token = Some((
                tokens.access_token.clone(),
                std::time::Instant::now() + Duration::from_secs(tokens.expires_in),
            ));
        })
    });
    Ok(tokens.access_token)
}

/// Calls the Colab API.
async fn call(
    cx: &mut AsyncApp,
    method: Method,
    path_and_query: &str,
    body: Option<Value>,
) -> Result<Value> {
    let token = access_token(cx).await?;
    let http_client = cx.update(|cx| cx.http_client());
    api_request(&http_client, &token, method, path_and_query, body).await
}

async fn api_request(
    http_client: &Arc<dyn HttpClient>,
    token: &str,
    method: Method,
    path_and_query: &str,
    body: Option<Value>,
) -> Result<Value> {
    let request = Request::builder()
        .method(method)
        .uri(format!("{API_URL}/{path_and_query}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/json");
    let request = match body {
        Some(body) => request
            .header("Content-Type", "application/json")
            .body(AsyncBody::from(body.to_string()))?,
        None => request.body(AsyncBody::default())?,
    };
    read_json(http_client.send(request).await?).await
}

/// A kind of runtime, like a T4 GPU with high RAM.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeKey {
    pub accelerator: String,
    pub shape: String,
    pub variant: String,
}

impl RuntimeKey {
    fn label(&self) -> String {
        let accelerator = match self.variant.as_str() {
            "VARIANT_GPU" => format!("{} GPU", self.accelerator),
            "VARIANT_TPU" => format!("{} TPU", self.accelerator),
            _ => "CPU".to_string(),
        };
        if self.shape == "SHAPE_HIGHMEM" {
            format!("{accelerator} (High-RAM)")
        } else {
            accelerator
        }
    }
}

#[derive(Debug, Deserialize)]
struct RuntimeSpecJson {
    key: RuntimeKey,
    #[serde(default)]
    eligible: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeJson {
    name: String,
    runtime_spec: RuntimeKey,
    connection_info: Option<ConnectionInfoJson>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionInfoJson {
    url: String,
    token: String,
    expire_time: Option<DateTime<Utc>>,
}

fn runtime_from_json(value: Value) -> Result<ColabRuntime> {
    let runtime: RuntimeJson = serde_json::from_value(value)?;
    let connection = runtime
        .connection_info
        .with_context(|| format!("Colab didn't say how to connect to {}", runtime.name))?;
    Ok(ColabRuntime {
        label: runtime.runtime_spec.label(),
        name: runtime.name,
        url: connection.url,
        token: connection.token,
        expire_time: connection.expire_time,
    })
}

/// The runtime an operation created, or why it failed.
fn operation_result(operation: &Value) -> Option<Result<Value>> {
    if !operation["done"].as_bool().unwrap_or(false) {
        return None;
    }
    if !operation["error"].is_null() {
        let error = &operation["error"];
        let reason = error["details"]
            .as_array()
            .and_then(|details| details.iter().find_map(|detail| detail["reason"].as_str()));
        let message = match reason {
            Some("TOO_MANY_ACTIVE_RUNTIMES") => {
                "you have too many Colab runtimes; delete one with `repl: delete colab runtime`"
                    .to_string()
            }
            Some("QUOTA_EXCEEDED_USAGE_TIME") => {
                "you don't have enough Colab quota for this runtime".to_string()
            }
            Some("DENYLISTED") => "Colab blocked this account from runtimes".to_string(),
            _ => error["message"]
                .as_str()
                .unwrap_or("Colab couldn't create the runtime")
                .to_string(),
        };
        return Some(Err(anyhow::anyhow!("{message}")));
    }
    Some(Ok(operation["response"].clone()))
}

async fn create_runtime(key: &RuntimeKey, cx: &mut AsyncApp) -> Result<ColabRuntime> {
    let request_id = uuid::Uuid::new_v4();
    let mut operation = call(
        cx,
        Method::POST,
        &format!("v1beta/runtimes?requestId={request_id}"),
        Some(json!({
            "runtimeSpec": {
                "accelerator": key.accelerator,
                "shape": key.shape,
                "variant": key.variant,
            }
        })),
    )
    .await?;
    for _ in 0..4 {
        if let Some(result) = operation_result(&operation) {
            return runtime_from_json(result?);
        }
        let name = operation["name"]
            .as_str()
            .context("Colab didn't name the operation creating the runtime")?
            .to_string();
        operation = call(
            cx,
            Method::GET,
            &format!("v1/{name}:wait?timeout={WAIT_TIMEOUT}"),
            None,
        )
        .await?;
    }
    anyhow::bail!("Colab took too long to create the runtime")
}

/// Reads the user's runtimes from Colab and connects to them.
async fn refresh_runtimes(cx: &mut AsyncApp) -> Result<()> {
    let listed = call(cx, Method::GET, "v1beta/runtimes", None).await?;
    let runtimes = listed["runtimes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|runtime| match runtime_from_json(runtime) {
            Ok(runtime) => Some(runtime),
            Err(error) => {
                log::error!("skipping a Colab runtime: {error:#}");
                None
            }
        })
        .collect::<Vec<ColabRuntime>>();
    log::info!(
        "Colab runtimes: {}",
        runtimes
            .iter()
            .map(|runtime: &ColabRuntime| format!("{} at {}", runtime.label, runtime.url))
            .collect::<Vec<_>>()
            .join(", ")
    );
    cx.update(|cx| set_runtimes(runtimes, cx));
    Ok(())
}

fn set_runtimes(runtimes: Vec<ColabRuntime>, cx: &mut App) {
    let refresh_task = schedule_token_refresh(&runtimes, cx);
    cx.update_global::<ColabRuntimes, _>(|state, _| {
        state.runtimes = runtimes;
        state._refresh_task = refresh_task;
    });
}

/// Refreshes the runtimes' proxy tokens shortly before the first expires.
fn schedule_token_refresh(runtimes: &[ColabRuntime], cx: &mut App) -> Option<Task<()>> {
    let first_expiry = runtimes
        .iter()
        .filter_map(|runtime| runtime.expire_time)
        .min()?;
    let wait = (first_expiry - Utc::now())
        .to_std()
        .unwrap_or_default()
        .saturating_sub(REFRESH_MARGIN)
        .max(Duration::from_secs(30));
    Some(cx.spawn(async move |cx| {
        cx.background_executor().timer(wait).await;
        if let Err(error) = refresh_runtimes(cx).await {
            log::error!("can't refresh the Colab runtimes: {error:#}");
            // Try again later rather than never.
            cx.background_executor()
                .timer(Duration::from_secs(60))
                .await;
            if let Err(error) = refresh_runtimes(cx).await {
                log::error!("can't refresh the Colab runtimes: {error:#}");
            }
        }
    }))
}

fn new_runtime(_: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    if let Err(error) = client(cx) {
        notify(format!("{error:#}"), cx);
        return;
    }
    let workspace_handle = cx.weak_entity();
    cx.spawn_in(window, async move |_, cx| {
        let specs = match call(cx, Method::GET, "v1beta/runtimespecs", None).await {
            Ok(specs) => specs,
            Err(error) => {
                cx.update(|_, cx| notify(format!("Can't list Colab runtimes: {error:#}"), cx))
                    .ok();
                return;
            }
        };
        let specs: Vec<RuntimeSpecJson> = specs["runtimeSpecs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|spec| serde_json::from_value(spec).ok())
            .collect();
        let eligible: Vec<RuntimeKey> = specs
            .into_iter()
            .filter(|spec| spec.eligible)
            .map(|spec| spec.key)
            .collect();
        if eligible.is_empty() {
            cx.update(|_, cx| notify("Colab offers no runtimes to this account.".into(), cx))
                .ok();
            return;
        }
        workspace_handle
            .update_in(cx, |workspace, window, cx| {
                let labels = eligible.iter().map(RuntimeKey::label).collect();
                open_picker(
                    workspace,
                    "Pick a Colab runtime…",
                    labels,
                    move |index, cx| {
                        if let Some(key) = eligible.get(index).cloned() {
                            start_runtime(key, cx);
                        }
                    },
                    window,
                    cx,
                );
            })
            .ok();
    })
    .detach();
}

fn start_runtime(key: RuntimeKey, cx: &mut App) {
    let label = key.label();
    notify(format!("Creating a Colab {label} runtime…"), cx);
    cx.spawn(async move |cx| {
        let result = async {
            let runtime = create_runtime(&key, cx).await?;
            refresh_runtimes(cx).await?;
            anyhow::Ok(runtime)
        }
        .await;
        cx.update(|cx| match result {
            Ok(runtime) => notify(
                format!(
                    "Colab {} is ready. Pick its kernel in a notebook's kernel picker.",
                    runtime.label
                ),
                cx,
            ),
            Err(error) => notify(
                format!("Can't create a Colab {label} runtime: {error:#}"),
                cx,
            ),
        });
    })
    .detach();
}

fn delete_runtime(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let runtimes = cx.global::<ColabRuntimes>().runtimes.clone();
    if runtimes.is_empty() {
        notify("You have no Colab runtimes.".into(), cx);
        return;
    }
    let labels = runtimes
        .iter()
        .map(|runtime| format!("Colab {}", runtime.label))
        .collect();
    open_picker(
        workspace,
        "Delete which Colab runtime?",
        labels,
        move |index, cx| {
            let Some(runtime) = runtimes.get(index).cloned() else {
                return;
            };
            cx.spawn(async move |cx| {
                let result = async {
                    call(
                        cx,
                        Method::DELETE,
                        &format!("v1beta/{}?allowMissing=true", runtime.name),
                        None,
                    )
                    .await?;
                    refresh_runtimes(cx).await
                }
                .await;
                cx.update(|cx| match result {
                    Ok(()) => notify(format!("Deleted Colab {}.", runtime.label), cx),
                    Err(error) => notify(format!("Can't delete the runtime: {error:#}"), cx),
                });
            })
            .detach();
        },
        window,
        cx,
    );
}

fn open_picker(
    workspace: &mut Workspace,
    placeholder: &'static str,
    items: Vec<String>,
    on_confirm: impl FnOnce(usize, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_modal(window, cx, move |window, cx| {
        let matches = (0..items.len()).collect();
        Picker::uniform_list(
            ChoiceDelegate {
                placeholder,
                items,
                matches,
                selected: 0,
                on_confirm: Some(Box::new(on_confirm)),
            },
            window,
            cx,
        )
        .initial_width(rems(34.))
    });
}

struct ChoiceDelegate {
    placeholder: &'static str,
    items: Vec<String>,
    matches: Vec<usize>,
    selected: usize,
    on_confirm: Option<Box<dyn FnOnce(usize, &mut App)>>,
}

impl PickerDelegate for ChoiceDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "colab picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected = index;
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let (Some(index), Some(on_confirm)) = (
            self.matches.get(self.selected).copied(),
            self.on_confirm.take(),
        ) {
            on_confirm(index, cx);
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
        let item = self.items.get(*self.matches.get(index)?)?;
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(Label::new(item.clone())),
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use http_client::FakeHttpClient;
    use std::sync::Mutex;

    #[test]
    fn test_labels() {
        let key = |accelerator: &str, shape: &str, variant: &str| RuntimeKey {
            accelerator: accelerator.into(),
            shape: shape.into(),
            variant: variant.into(),
        };
        assert_eq!(key("NONE", "SHAPE_STANDARD", "VARIANT_CPU").label(), "CPU");
        assert_eq!(key("T4", "SHAPE_STANDARD", "VARIANT_GPU").label(), "T4 GPU");
        assert_eq!(
            key("A100", "SHAPE_HIGHMEM", "VARIANT_GPU").label(),
            "A100 GPU (High-RAM)"
        );
        assert_eq!(
            key("V6E1", "SHAPE_STANDARD", "VARIANT_TPU").label(),
            "V6E1 TPU"
        );
    }

    #[test]
    fn test_sign_in_urls() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(verifier.len(), 64);
        assert_eq!(
            challenge,
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(verifier.as_bytes()))
        );
        let url = authorization_url("id.apps", "http://127.0.0.1:4000", &challenge, "s1");
        assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?client_id=id.apps"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A4000"));
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcolaboratory"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("access_type=offline"));

        assert_eq!(
            parse_redirect("GET /?state=s1&code=4%2Fabc HTTP/1.1", "s1")
                .expect("a redirect")
                .expect("a code"),
            "4/abc"
        );
        assert!(parse_redirect("GET /favicon.ico HTTP/1.1", "s1").is_none());
        assert!(
            parse_redirect("GET /?state=other&code=x HTTP/1.1", "s1")
                .expect("a redirect")
                .is_err()
        );
        assert!(
            parse_redirect("GET /?error=access_denied&state=s1 HTTP/1.1", "s1")
                .expect("a redirect")
                .is_err()
        );
    }

    #[test]
    fn test_errors() {
        let body = r#"{"error": {"code": 9, "message": "Precondition failed",
            "details": [{"@type": "type.googleapis.com/google.rpc.ErrorInfo",
                         "reason": "SERVICE_DISABLED"}]}}"#;
        assert_eq!(
            error_message(403, body),
            "Precondition failed (SERVICE_DISABLED, HTTP 403)"
        );
        assert_eq!(
            error_message(
                400,
                r#"{"error": "invalid_grant", "error_description": "Bad Request"}"#
            ),
            "Bad Request (HTTP 400)"
        );
        let failed = json!({"done": true, "error": {"code": 9, "message": "x",
            "details": [{"reason": "TOO_MANY_ACTIVE_RUNTIMES"}]}});
        let error = operation_result(&failed)
            .expect("done")
            .expect_err("an error");
        assert!(error.to_string().contains("too many Colab runtimes"));
        assert!(operation_result(&json!({"done": false})).is_none());
    }

    /// Creating a runtime waits on its operation, and the runtime's
    /// connection carries the proxy token.
    #[gpui::test]
    async fn test_create_runtime(cx: &mut gpui::TestAppContext) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let http_client = FakeHttpClient::create({
            let requests = requests.clone();
            move |request| {
                let requests = requests.clone();
                async move {
                    let uri = request.uri().to_string();
                    let auth = request
                        .headers()
                        .get("Authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let mut body = String::new();
                    request.into_body().read_to_string(&mut body).await?;
                    requests
                        .lock()
                        .map_err(|_| anyhow::anyhow!("poisoned"))?
                        .push(format!("{uri} {auth} {body}"));
                    let runtime = json!({
                        "name": "runtimes/r1",
                        "runtimeSpec": {"accelerator": "T4", "shape": "SHAPE_STANDARD",
                                        "variant": "VARIANT_GPU"},
                        "connectionInfo": {"url": "https://r1.colab.example/",
                                           "token": "proxy", "expireTime": "2030-01-01T00:00:00Z"}
                    });
                    let response = if uri.contains(":wait") {
                        json!({"name": "operations/o1", "done": true, "response": runtime})
                    } else if uri.contains("v1beta/runtimes?requestId=") {
                        json!({"name": "operations/o1", "done": false})
                    } else if uri.ends_with("v1beta/runtimes") {
                        json!({"runtimes": [runtime]})
                    } else {
                        anyhow::bail!("unexpected request {uri}")
                    };
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(response.to_string()))?)
                }
            }
        });
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            JupyterSettings::register(cx);
            cx.set_http_client(http_client);
            cx.set_global(ColabRuntimes {
                access_token: Some((
                    "access".into(),
                    std::time::Instant::now() + Duration::from_secs(3600),
                )),
                ..Default::default()
            });
        });
        let key = RuntimeKey {
            accelerator: "T4".into(),
            shape: "SHAPE_STANDARD".into(),
            variant: "VARIANT_GPU".into(),
        };
        let mut async_cx = cx.to_async();
        let runtime = create_runtime(&key, &mut async_cx)
            .await
            .expect("the runtime");
        assert_eq!(runtime.label, "T4 GPU");
        assert_eq!(runtime.url, "https://r1.colab.example/");
        refresh_runtimes(&mut async_cx).await.expect("the runtimes");

        let logged = requests.lock().expect("the requests").clone();
        assert_eq!(logged.len(), 3);
        assert!(
            logged[0].starts_with("https://colaboratory.googleapis.com/v1beta/runtimes?requestId=")
        );
        assert!(logged[0].contains("Bearer access"));
        assert!(logged[0].contains(r#""variant":"VARIANT_GPU""#));
        assert!(
            logged[1].starts_with(
                "https://colaboratory.googleapis.com/v1/operations/o1:wait?timeout=200s"
            )
        );

        cx.update(|cx| {
            let servers = jupyter_servers(cx);
            assert_eq!(servers.len(), 1);
            let (name, server, headers) = &servers[0];
            assert_eq!(name, "Colab T4 GPU");
            assert_eq!(server.base_url, "https://r1.colab.example/");
            assert_eq!(server.token, "proxy");
            assert_eq!(
                headers,
                &vec![
                    (PROXY_TOKEN_HEADER.to_string(), "proxy".to_string()),
                    (CLIENT_AGENT_HEADER.to_string(), CLIENT_AGENT.to_string()),
                ]
            );
        });
    }

    /// Signing in opens Google's page in the browser, takes the code from
    /// the redirect to localhost and trades it for tokens.
    #[gpui::test]
    async fn test_sign_in(cx: &mut gpui::TestAppContext) {
        cx.executor().allow_parking();
        let token_requests = Arc::new(Mutex::new(Vec::new()));
        let http_client = FakeHttpClient::create({
            let token_requests = token_requests.clone();
            move |request| {
                let token_requests = token_requests.clone();
                async move {
                    anyhow::ensure!(request.uri().to_string() == TOKEN_URL);
                    let mut body = String::new();
                    request.into_body().read_to_string(&mut body).await?;
                    token_requests
                        .lock()
                        .map_err(|_| anyhow::anyhow!("poisoned"))?
                        .push(body);
                    let tokens = json!({"access_token": "fresh", "expires_in": 3599,
                                        "refresh_token": "refresh"});
                    Ok(http_client::Response::builder()
                        .status(200)
                        .body(AsyncBody::from(tokens.to_string()))?)
                }
            }
        });
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            JupyterSettings::register(cx);
            cx.set_http_client(http_client);
            cx.set_global(ColabRuntimes::default());
            gpui::UpdateGlobal::update_global(cx, |store: &mut settings::SettingsStore, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.jupyter.get_or_insert_default().colab =
                        Some(settings::ColabContent {
                            client_id: Some("me.apps.googleusercontent.com".into()),
                            client_secret: Some("not-so-secret".into()),
                        });
                });
            });
        });
        let signed_in = Arc::new(Mutex::new(None));
        cx.spawn({
            let signed_in = signed_in.clone();
            async move |mut cx| {
                let result = sign_in(&mut cx).await;
                if let Ok(mut signed_in) = signed_in.lock() {
                    *signed_in = Some(result);
                }
            }
        })
        .detach();

        let started = std::time::Instant::now();
        let opened = loop {
            if let Some(url) = cx.opened_url() {
                break url;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "no browser opened"
            );
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        };
        let query = opened.split_once('?').expect("a query").1;
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();
        assert_eq!(params["client_id"], "me.apps.googleusercontent.com");
        let redirect = params["redirect_uri"].clone();
        let state = params["state"].clone();

        // The browser follows Google's redirect.
        let browser = std::thread::spawn(move || -> std::io::Result<String> {
            use std::io::{Read as _, Write as _};
            let address = redirect.trim_start_matches("http://").to_string();
            let mut favicon = std::net::TcpStream::connect(&address)?;
            favicon.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")?;
            let mut ignored = String::new();
            favicon.read_to_string(&mut ignored)?;
            let mut stream = std::net::TcpStream::connect(&address)?;
            write!(
                stream,
                "GET /?state={state}&code=4%2Fthe-code&scope=x HTTP/1.1\r\nHost: x\r\n\r\n"
            )?;
            let mut page = String::new();
            stream.read_to_string(&mut page)?;
            Ok(page)
        });
        while !browser.is_finished() {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }
        let page = browser
            .join()
            .expect("the browser thread")
            .expect("the redirect");
        assert!(page.contains("Signed in to Google Colab"), "{page}");

        let started = std::time::Instant::now();
        loop {
            if let Some(result) = signed_in.lock().expect("the result").take() {
                result.expect("signing in");
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(30), "sign-in hung");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(20));
        }

        let body = token_requests.lock().expect("the requests")[0].clone();
        assert!(body.contains("code=4%2Fthe-code"), "{body}");
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code_verifier="));
        assert!(body.contains("client_secret=not-so-secret"));
        cx.update(|cx| {
            assert_eq!(
                cx.global::<ColabRuntimes>()
                    .access_token
                    .as_ref()
                    .map(|(token, _)| token.as_str()),
                Some("fresh")
            );
        });
    }

    /// Requests to a runtime's Jupyter server carry its proxy token.
    #[gpui::test]
    async fn test_runtime_requests_carry_the_proxy_token() {
        let http_client = FakeHttpClient::create(|request| async move {
            let header = request
                .headers()
                .get(PROXY_TOKEN_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            anyhow::ensure!(header.as_deref() == Some("proxy"), "no proxy token");
            let specs = json!({"default": "python3", "kernelspecs": {"python3": {
                "name": "python3", "resources": {},
                "spec": {"argv": [], "display_name": "Python 3", "language": "python"}}}});
            Ok(http_client::Response::builder()
                .status(200)
                .body(AsyncBody::from(specs.to_string()))?)
        });
        let specs = crate::kernels::list_remote_kernelspecs(
            "Colab T4 GPU".into(),
            RemoteServer {
                base_url: "https://r1.colab.example/".into(),
                token: "proxy".into(),
            },
            vec![(PROXY_TOKEN_HEADER.to_string(), "proxy".to_string())],
            http_client,
        )
        .await
        .expect("the kernels");
        assert_eq!(specs[0].kernelspec.display_name, "Python 3");
        assert_eq!(specs[0].headers[0].1, "proxy");
    }
}
