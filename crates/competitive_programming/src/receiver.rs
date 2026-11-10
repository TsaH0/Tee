//! A localhost HTTP receiver for Competitive Companion (`POST /`) and the
//! cptui browser extension (`POST /solutions` and `/solutions/progress`). It
//! speaks the same protocol, with the same CORS policy, as cptui, so both
//! extensions work unchanged.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::channel::mpsc::UnboundedSender;
use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::storage::{self, Task};

/// What the receiver tells the rest of the plugin about.
pub enum ReceiverEvent {
    ProblemReceived {
        dir: PathBuf,
        source: PathBuf,
        name: String,
        index: u64,
        size: u64,
    },
    SolutionsSaved {
        dir: PathBuf,
        written: usize,
        existing: usize,
    },
    Status {
        message: String,
        is_error: bool,
    },
}

#[derive(Clone)]
pub struct ReceiverConfig {
    pub workspace: PathBuf,
    pub source: String,
    pub template: String,
}

pub struct Receiver {
    server: Arc<Server>,
    pub address: String,
}

impl Receiver {
    pub fn start(
        host: &str,
        port: u16,
        config: ReceiverConfig,
        events: UnboundedSender<ReceiverEvent>,
    ) -> Result<Self> {
        let address = format!("{host}:{port}");
        let server = Server::http(&address).map_err(|error| {
            anyhow::anyhow!("can't listen on {address}: {error} (is cptui already running?)")
        })?;
        let server = Arc::new(server);
        thread::Builder::new()
            .name("competitive-companion".into())
            .spawn({
                let server = server.clone();
                move || {
                    let mut handler = Handler {
                        config,
                        events,
                        batches: HashMap::new(),
                    };
                    for request in server.incoming_requests() {
                        handler.handle(request);
                    }
                }
            })
            .context("failed to start the receiver thread")?;
        Ok(Self { server, address })
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

struct Handler {
    config: ReceiverConfig,
    events: UnboundedSender<ReceiverEvent>,
    /// How many problems of each contest batch have arrived.
    batches: HashMap<String, u64>,
}

/// Only the installed extension or a supported judge page may post solutions.
fn allowed_origin(origin: &str) -> bool {
    if origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://") {
        return true;
    }
    let Some(host) = origin.strip_prefix("https://") else {
        return false;
    };
    if host.contains(['/', ':']) {
        return false;
    }
    ["codeforces.com", "atcoder.jp", "vjudge.net"]
        .iter()
        .any(|base| host == *base || host.ends_with(&format!(".{base}")))
}

fn header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

fn cors_headers(origin: &str) -> Vec<Header> {
    [
        ("Access-Control-Allow-Origin", origin),
        ("Access-Control-Allow-Headers", "content-type"),
        ("Access-Control-Allow-Methods", "POST, OPTIONS"),
        ("Access-Control-Allow-Private-Network", "true"),
    ]
    .into_iter()
    .filter_map(|(name, value)| header(name, value))
    .collect()
}

fn respond(request: Request, status: u16, body: &str, headers: Vec<Header>) {
    let mut response = Response::from_string(body).with_status_code(status);
    for header in headers {
        response.add_header(header);
    }
    if let Err(error) = request.respond(response) {
        log::warn!("competitive programming receiver: failed to respond: {error}");
    }
}

#[derive(Deserialize)]
struct SolutionBatch {
    problem: SolutionProblem,
    #[serde(default)]
    files: Vec<SolutionFile>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SolutionProblem {
    platform: Option<String>,
    contest_id: Option<serde_json::Value>,
    index: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SolutionFile {
    handle: Option<String>,
    submission_id: Option<serde_json::Value>,
    ext: Option<String>,
    source: Option<String>,
    image_base64: Option<String>,
    image_mime: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SolutionProgress {
    stage: Option<String>,
    completed: Option<u64>,
    total: Option<u64>,
    message: Option<String>,
}

fn json_text(value: &Option<serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// The key a judge's problem URL identifies a problem by, such as
/// "1999/A" for Codeforces.
fn url_key(platform: &str, url: &str) -> Option<String> {
    let after = |marker: &str| url.find(marker).map(|index| &url[index + marker.len()..]);
    let segments = |rest: &str| {
        rest.split(['/', '?', '#'])
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    match platform {
        "Codeforces" => {
            if let Some(rest) = after("codeforces.com/problemset/problem/") {
                let parts = segments(rest);
                return Some(format!("{}/{}", parts.first()?, parts.get(1)?.to_uppercase()));
            }
            let parts = segments(after("codeforces.com/")?);
            if parts.get(2).map(String::as_str) == Some("problem") {
                return Some(format!("{}/{}", parts.get(1)?, parts.get(3)?.to_uppercase()));
            }
            None
        }
        "AtCoder" => {
            let parts = segments(after("atcoder.jp/contests/")?);
            (parts.get(1).map(String::as_str) == Some("tasks"))
                .then(|| Some(format!("{}/{}", parts.first()?, parts.get(2)?)))
                .flatten()
        }
        "VJudge" => {
            let parts = segments(after("vjudge.net/problem/")?);
            Some(format!("vjudge/{}", parts.first()?))
        }
        _ => None,
    }
}

impl Handler {
    fn handle(&mut self, mut request: Request) {
        let path = request.url().split('?').next().unwrap_or_default().to_string();
        let method = request.method().clone();
        let mut body = String::new();
        if let Err(error) = request.as_reader().read_to_string(&mut body) {
            respond(request, 400, &format!("unreadable body: {error}"), Vec::new());
            return;
        }

        if path == "/" && method == Method::Post {
            let (status, message) = match self.receive_task(&body) {
                Ok(()) => (200, "OK".to_string()),
                Err(error) => {
                    self.status(format!("Failed to import a problem: {error:#}"), true);
                    (400, format!("{error:#}"))
                }
            };
            respond(request, status, &message, Vec::new());
            return;
        }

        if path != "/solutions" && path != "/solutions/progress" {
            respond(request, 404, "", Vec::new());
            return;
        }
        let origin = request
            .headers()
            .iter()
            .find(|header| header.field.equiv("Origin"))
            .map(|header| header.value.to_string())
            .unwrap_or_default();
        if !allowed_origin(&origin) {
            respond(request, 403, "", Vec::new());
            return;
        }
        let headers = cors_headers(&origin);
        if method == Method::Options {
            respond(request, 204, "", headers);
            return;
        }
        if method != Method::Post {
            respond(request, 404, "", headers);
            return;
        }
        let result = if path == "/solutions" {
            self.receive_solutions(&body).map(|accepted| {
                (202, serde_json::json!({ "accepted": accepted }).to_string())
            })
        } else {
            self.receive_progress(&body).map(|()| (204, String::new()))
        };
        match result {
            Ok((status, message)) => respond(request, status, &message, headers),
            Err(error) => respond(request, 400, &format!("{error:#}"), headers),
        }
    }

    fn status(&self, message: String, is_error: bool) {
        self.events
            .unbounded_send(ReceiverEvent::Status { message, is_error })
            .ok();
    }

    fn receive_task(&mut self, body: &str) -> Result<()> {
        let task: Task = serde_json::from_str(body).context("invalid problem JSON")?;
        if task.name.is_empty() {
            anyhow::bail!("the problem has no name");
        }
        let created = storage::create_problem(
            &self.config.workspace,
            &self.config.source,
            &self.config.template,
            &task,
        )?;
        let size = task.batch.size.max(1);
        let key = if task.batch.id.is_empty() {
            created.dir.to_string_lossy().into_owned()
        } else {
            task.batch.id.clone()
        };
        let index = self.batches.entry(key).or_default();
        *index += 1;
        self.events
            .unbounded_send(ReceiverEvent::ProblemReceived {
                source: created.dir.join(&created.meta.source),
                dir: created.dir,
                name: created.meta.name,
                index: *index,
                size,
            })
            .ok();
        Ok(())
    }

    /// The directory that holds `solutions/` for a problem from the
    /// extension: the matching imported problem, else `<workspace>/<title>`.
    fn solution_dir(&self, problem: &SolutionProblem) -> PathBuf {
        let platform = problem.platform.as_deref().unwrap_or("Codeforces");
        let contest = json_text(&problem.contest_id);
        let index = problem.index.clone().unwrap_or_default();
        let wanted = match platform {
            "Codeforces" => Some(format!("{contest}/{}", index.to_uppercase())),
            "AtCoder" => Some(format!("{contest}/{index}")),
            "VJudge" => Some(format!("vjudge/{index}")),
            _ => None,
        };
        if let Some(wanted) = wanted {
            for candidate in storage::list_problems(&self.config.workspace) {
                if url_key(platform, &candidate.meta.url).as_deref() == Some(wanted.as_str()) {
                    return candidate.dir.join("solutions");
                }
            }
        }
        let mut title = problem.title.clone().unwrap_or_default().trim().to_string();
        if title.is_empty() || title == "problem" {
            title = format!("{platform}-{contest}-{index}");
        }
        self.config
            .workspace
            .join(storage::sanitize(&title))
            .join("solutions")
    }

    fn receive_solutions(&mut self, body: &str) -> Result<usize> {
        let batch: SolutionBatch = serde_json::from_str(body).context("invalid solutions JSON")?;
        if batch.files.is_empty() {
            anyhow::bail!("no source files");
        }
        let dir = self.solution_dir(&batch.problem);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        let mut written = 0;
        let mut existing = 0;
        for file in &batch.files {
            let stem = format!(
                "{}_{}",
                storage::sanitize(file.handle.as_deref().unwrap_or("user")),
                json_text(&file.submission_id)
            );
            let has_source = file
                .source
                .as_deref()
                .is_some_and(|source| !source.trim().is_empty());
            if let Some(image) = &file.image_base64 {
                save_image(&dir, &stem, image, file.image_mime.as_deref())?;
            }
            let (extension, contents) = if has_source {
                (
                    storage::sanitize(file.ext.as_deref().unwrap_or("txt")),
                    file.source.clone().unwrap_or_default(),
                )
            } else {
                (
                    "ocr-error.txt".to_string(),
                    "# This submission has no text source. OCR of image-only sources \
                     is only supported by the cptui TUI and Neovim plugin.\n"
                        .to_string(),
                )
            };
            let path = dir.join(format!("{stem}.{extension}"));
            if path.exists() {
                existing += 1;
            } else {
                fs::write(&path, contents)
                    .with_context(|| format!("failed to write {}", path.display()))?;
                written += 1;
            }
        }
        self.events
            .unbounded_send(ReceiverEvent::SolutionsSaved {
                dir,
                written,
                existing,
            })
            .ok();
        Ok(batch.files.len())
    }

    fn receive_progress(&mut self, body: &str) -> Result<()> {
        let progress: SolutionProgress =
            serde_json::from_str(body).context("invalid progress JSON")?;
        let stage = progress.stage.unwrap_or_default();
        let count = match progress.total {
            Some(total) if total > 0 => format!(" {}/{total}", progress.completed.unwrap_or(0)),
            _ => String::new(),
        };
        let detail = progress
            .message
            .filter(|message| !message.is_empty())
            .map(|message| format!(": {message}"))
            .unwrap_or_default();
        self.status(
            format!("Solutions {stage}{count}{detail}"),
            stage == "Failed",
        );
        Ok(())
    }
}

fn save_image(dir: &Path, stem: &str, image: &str, mime: Option<&str>) -> Result<()> {
    let extension = match mime {
        Some("image/jpeg") => "jpg",
        Some("image/webp") => "webp",
        _ => "png",
    };
    let path = dir.join(format!("{stem}.{extension}"));
    if path.exists() {
        return Ok(());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image)
        .context("invalid image data")?;
    fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_allowed_origin() {
        assert!(allowed_origin("chrome-extension://abc"));
        assert!(allowed_origin("https://codeforces.com"));
        assert!(allowed_origin("https://m1.codeforces.com"));
        assert!(!allowed_origin("https://evilcodeforces.com"));
        assert!(!allowed_origin("http://codeforces.com"));
        assert!(!allowed_origin("https://codeforces.com:8080"));
    }

    #[test]
    fn test_url_key() {
        assert_eq!(
            url_key("Codeforces", "https://codeforces.com/contest/1999/problem/a"),
            Some("1999/A".to_string())
        );
        assert_eq!(
            url_key("Codeforces", "https://codeforces.com/problemset/problem/4/A"),
            Some("4/A".to_string())
        );
        assert_eq!(
            url_key("AtCoder", "https://atcoder.jp/contests/abc300/tasks/abc300_a"),
            Some("abc300/abc300_a".to_string())
        );
    }
}
