//! Compiles a problem's source and runs its tests, judging the output the
//! same way cptui does.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _};
use gpui::BackgroundExecutor;
use smol::process::Command;

use crate::storage::{Problem, Testcase};

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Language {
    pub compile: Option<Vec<String>>,
    pub run: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Accepted,
    WrongAnswer,
    TimeLimitExceeded,
    RuntimeError,
    /// The test has no expected output to judge against.
    Done,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Accepted => "AC",
            Verdict::WrongAnswer => "WA",
            Verdict::TimeLimitExceeded => "TLE",
            Verdict::RuntimeError => "RE",
            Verdict::Done => "DONE",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TestResult {
    pub verdict: Verdict,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub exit_status: Option<i32>,
}

/// CRLF and CR become LF, trailing whitespace is stripped from every line and
/// trailing blank lines are dropped.
pub fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = text.split('\n').map(str::trim_end).collect::<Vec<_>>();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// Accepts output equal to the expected output after normalizing, or with the
/// same whitespace-separated tokens.
pub fn judge(expected: &str, actual: &str) -> Verdict {
    let expected = normalize(expected);
    let actual = normalize(actual);
    if expected == actual || expected.split_whitespace().eq(actual.split_whitespace()) {
        Verdict::Accepted
    } else {
        Verdict::WrongAnswer
    }
}

pub struct Variables {
    pub source: PathBuf,
    pub binary: PathBuf,
    pub dir: PathBuf,
}

fn expand(command: &[String], variables: &Variables) -> Vec<String> {
    command
        .iter()
        .map(|argument| {
            argument
                .replace("$(FNAME)", &variables.source.to_string_lossy())
                .replace("$(BIN)", &variables.binary.to_string_lossy())
                .replace("$(DIR)", &variables.dir.to_string_lossy())
        })
        .collect()
}

fn command(arguments: &[String], dir: &Path) -> Result<Command> {
    let (program, arguments) = arguments
        .split_first()
        .ok_or_else(|| anyhow!("empty command"))?;
    let mut command = Command::new(program);
    command.args(arguments).current_dir(dir).kill_on_drop(true);
    Ok(command)
}

/// Binaries are cached outside the problem directory, where cptui keeps them.
fn binary_path(problem: &Problem) -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".cache"));
    let mut hasher = DefaultHasher::new();
    problem.dir.hash(&mut hasher);
    cache
        .join("cptui")
        .join("bin")
        .join(format!("{:016x}", hasher.finish()))
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n… (truncated)");
    }
    text
}

/// Compiles the problem's source, returning the variables to run it with,
/// or the compiler's output on failure.
pub async fn compile(
    problem: &Problem,
    language: &Language,
) -> Result<std::result::Result<(Variables, String), String>> {
    let binary = binary_path(problem);
    if let Some(parent) = binary.parent() {
        smol::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let variables = Variables {
        source: problem.source_path(),
        binary,
        dir: problem.dir.clone(),
    };
    let Some(compile) = &language.compile else {
        return Ok(Ok((variables, String::new())));
    };
    let output = command(&expand(compile, &variables), &problem.dir)?
        .output()
        .await
        .with_context(|| format!("failed to run {}", compile.join(" ")))?;
    let mut messages = String::from_utf8_lossy(&output.stderr).into_owned();
    messages.push_str(&String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        Ok(Ok((variables, messages)))
    } else {
        Ok(Err(messages))
    }
}

pub async fn run_test(
    test: &Testcase,
    language: &Language,
    variables: &Variables,
    timeout: Duration,
    executor: BackgroundExecutor,
) -> TestResult {
    let started = Instant::now();
    let spawned = command(&expand(&language.run, variables), &variables.dir).and_then(|mut command| {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start the program")
    });
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return TestResult {
                verdict: Verdict::RuntimeError,
                stdout: String::new(),
                stderr: format!("{error:#}"),
                elapsed: Duration::ZERO,
                exit_status: None,
            };
        }
    };

    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let input = test.input.clone();
    let run = async {
        let write_input = async {
            if let Some(mut stdin) = stdin.take() {
                // A program may exit without reading its input; that's not an error.
                stdin.write_all(input.as_bytes()).await.ok();
            }
        };
        let read_stdout = async {
            let mut buffer = Vec::new();
            if let Some(stdout) = stdout.as_mut() {
                stdout.read_to_end(&mut buffer).await.ok();
            }
            buffer
        };
        let read_stderr = async {
            let mut buffer = Vec::new();
            if let Some(stderr) = stderr.as_mut() {
                stderr.read_to_end(&mut buffer).await.ok();
            }
            buffer
        };
        let ((), stdout, stderr) = futures::join!(write_input, read_stdout, read_stderr);
        let status = child.status().await;
        (status, stdout, stderr)
    };

    let mut timer = executor.timer(timeout).fuse();
    let mut run = Box::pin(run.fuse());
    let finished = futures::select_biased! {
        finished = run => Some(finished),
        _ = timer => None,
    };
    let elapsed = started.elapsed();

    let Some((status, stdout, stderr)) = finished else {
        // Dropping the run drops the child, which kills it.
        return TestResult {
            verdict: Verdict::TimeLimitExceeded,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: elapsed.max(timeout),
            exit_status: None,
        };
    };
    let stdout = truncate(String::from_utf8_lossy(&stdout).into_owned());
    let mut stderr = truncate(String::from_utf8_lossy(&stderr).into_owned());
    let (succeeded, exit_status) = match status {
        Ok(status) => (status.success(), status.code()),
        Err(error) => {
            stderr.push_str(&format!("\nfailed to wait for the program: {error}"));
            (false, None)
        }
    };
    let verdict = if !succeeded {
        Verdict::RuntimeError
    } else if test.expected.trim().is_empty() {
        Verdict::Done
    } else {
        judge(&test.expected, &stdout)
    };
    TestResult {
        verdict,
        stdout,
        stderr,
        elapsed,
        exit_status,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_judge() {
        assert_eq!(judge("1 2\r\n3  \n\n", "1 2\n3"), Verdict::Accepted);
        assert_eq!(judge("1 2 3", "1\n2\n3\n"), Verdict::Accepted);
        assert_eq!(judge("1 2 3", "1 2 4"), Verdict::WrongAnswer);
        assert_eq!(normalize("a \r\nb\t\n\n"), "a\nb");
    }
}
