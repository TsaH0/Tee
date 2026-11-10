//! Competitive programming in Zed: receives problems from Competitive
//! Companion, keeps their testcases on disk in cptui's layout, compiles and
//! judges solutions, and moves between the problems of a contest.

mod receiver;
mod runner;
mod storage;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::{StreamExt as _, channel::mpsc, stream::FuturesUnordered};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, Global, Task, WeakEntity, Window,
};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use picker::{Picker, PickerDelegate};
use settings::{RegisterSetting, Settings};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, Toggleable, prelude::*, rems};
use util::ResultExt as _;
use workspace::{
    MultiWorkspace, OpenOptions, SaveIntent, Workspace,
    notifications::{DetachAndPromptErr as _, NotificationId, show_app_notification},
};
use zed_actions::competitive_programming::{
    AcceptOutput, AddTestcase, DeleteTestcase, EditTestcase, NextProblem, OpenProblem,
    OpenProblemUrl, OpenSolutions, PreviousProblem, ReceiverStatus, RunTests, ShowResults,
    StartReceiver, StopReceiver,
};

use receiver::{Receiver, ReceiverConfig, ReceiverEvent};
use runner::{Language, TestResult, Verdict};
use storage::{Problem, TestKind, Testcase};

#[derive(RegisterSetting, Clone, Debug)]
struct CompetitiveProgrammingSettings {
    workspace: PathBuf,
    host: String,
    port: u16,
    autostart: bool,
    open_on_receive: bool,
    source: String,
    template_file: Option<PathBuf>,
    default_timeout: Duration,
    timeout_multiplier: f32,
    languages: HashMap<String, Language>,
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => util::paths::home_dir().join(rest),
        None if path == "~" => util::paths::home_dir().clone(),
        None => PathBuf::from(path),
    }
}

impl Settings for CompetitiveProgrammingSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let content = content.competitive_programming.clone().unwrap_or_default();
        let languages = content
            .languages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(extension, language)| {
                let run = language.run.filter(|run| !run.is_empty())?;
                Some((
                    extension,
                    Language {
                        compile: language.compile.filter(|compile| !compile.is_empty()),
                        run,
                    },
                ))
            })
            .collect();
        Self {
            workspace: expand_home(content.workspace.as_deref().unwrap_or("~/cp")),
            host: content.host.unwrap_or_else(|| "127.0.0.1".to_string()),
            port: content.port.unwrap_or(27121),
            autostart: content.autostart.unwrap_or(false),
            open_on_receive: content.open_on_receive.unwrap_or(true),
            source: content.source.unwrap_or_else(|| "main.cpp".to_string()),
            template_file: content.template_file.as_deref().map(expand_home),
            default_timeout: Duration::from_millis(content.default_timeout_ms.unwrap_or(2000)),
            timeout_multiplier: content.timeout_multiplier.unwrap_or(1.5),
            languages,
        }
    }
}

impl CompetitiveProgrammingSettings {
    fn language_for(&self, source: &Path) -> Result<Language> {
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        self.languages.get(extension).cloned().ok_or_else(|| {
            anyhow!(
                "no language is configured for .{extension} files in \
                 competitive_programming.languages"
            )
        })
    }

    fn timeout_for(&self, problem: &Problem) -> Duration {
        let base = if problem.meta.time_limit_ms > 0 {
            Duration::from_millis(problem.meta.time_limit_ms)
        } else {
            self.default_timeout
        };
        base.mul_f32(self.timeout_multiplier.max(0.1))
            .max(Duration::from_millis(100))
    }

    fn template(&self) -> String {
        self.template_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).log_err())
            .unwrap_or_else(|| storage::DEFAULT_TEMPLATE.to_string())
    }
}

#[derive(Clone)]
enum TestState {
    Pending,
    Finished(TestResult),
}

struct RunReport {
    problem: Problem,
    /// Testcase numbers (starting at 1) in this run.
    tests: Vec<usize>,
    compiling: bool,
    compile_output: String,
    compile_error: Option<String>,
    results: HashMap<usize, TestState>,
}

impl RunReport {
    fn render(&self) -> String {
        let mut text = format!("# {}\n\n", self.problem.meta.name);
        if self.compiling {
            text.push_str("Compiling…\n");
            return text;
        }
        if let Some(error) = &self.compile_error {
            text.push_str("## CE: compilation failed\n\n```\n");
            text.push_str(error.trim_end());
            text.push_str("\n```\n");
            return text;
        }
        let passed = self
            .results
            .values()
            .filter(|state| {
                matches!(state, TestState::Finished(result) if result.verdict == Verdict::Accepted)
            })
            .count();
        text.push_str(&format!("Passed {passed}/{}\n\n", self.tests.len()));
        for number in &self.tests {
            let Some(test) = self.problem.tests.get(number - 1) else {
                continue;
            };
            let kind = match test.kind {
                TestKind::Sample => "sample",
                TestKind::Custom => "custom",
            };
            match self.results.get(number) {
                Some(TestState::Finished(result)) => {
                    text.push_str(&format!(
                        "## Test {number} ({kind}): {} in {} ms{}\n\n",
                        result.verdict.label(),
                        result.elapsed.as_millis(),
                        result
                            .exit_status
                            .filter(|status| *status != 0)
                            .map(|status| format!(", exit code {status}"))
                            .unwrap_or_default()
                    ));
                    push_block(&mut text, "Input", &test.input);
                    push_block(&mut text, "Expected", &test.expected);
                    push_block(&mut text, "Output", &result.stdout);
                    if !result.stderr.trim().is_empty() {
                        push_block(&mut text, "Stderr", &result.stderr);
                    }
                }
                _ => text.push_str(&format!("## Test {number} ({kind}): running…\n\n")),
            }
        }
        if !self.compile_output.trim().is_empty() {
            text.push_str("## Compiler output\n\n```\n");
            text.push_str(self.compile_output.trim_end());
            text.push_str("\n```\n");
        }
        text
    }
}

fn push_block(text: &mut String, title: &str, contents: &str) {
    text.push_str(&format!("### {title}\n\n```\n{}\n```\n\n", contents.trim_end()));
}

#[derive(Default)]
struct GlobalState {
    receiver: Option<Receiver>,
    receiver_events: Option<Task<()>>,
    status: String,
    last_problem: Option<PathBuf>,
    reports: HashMap<PathBuf, RunReport>,
    results_buffers: HashMap<PathBuf, WeakEntity<Buffer>>,
}

impl Global for GlobalState {}

pub fn init(cx: &mut App) {
    cx.set_global(GlobalState::default());
    cx.observe_new(|workspace: &mut Workspace, _, _| register(workspace))
        .detach();
    if CompetitiveProgrammingSettings::get_global(cx).autostart {
        start_receiver(cx).log_err();
    }
}

fn register(workspace: &mut Workspace) {
    workspace
        .register_action(|_, _: &StartReceiver, _, cx| match start_receiver(cx) {
            Ok(()) => notify(receiver_status(cx), false, cx),
            Err(error) => notify(format!("{error:#}"), true, cx),
        })
        .register_action(|_, _: &StopReceiver, _, cx| {
            let state = cx.global_mut::<GlobalState>();
            state.receiver = None;
            state.receiver_events = None;
            notify("Competitive Companion receiver stopped".to_string(), false, cx);
        })
        .register_action(|_, _: &ReceiverStatus, _, cx| {
            notify(receiver_status(cx), false, cx);
        })
        .register_action(|workspace, action: &RunTests, window, cx| {
            run_tests(workspace, action.test, window, cx);
        })
        .register_action(|workspace, _: &ShowResults, window, cx| {
            match current_problem_dir(workspace, cx) {
                Some(dir) => show_results(workspace, &dir, window, cx),
                None => not_in_problem(cx),
            }
        })
        .register_action(|workspace, _: &AddTestcase, window, cx| {
            with_problem(workspace, window, cx, |workspace, problem, window, cx| {
                let mut tests = problem.tests.clone();
                tests.push(Testcase {
                    input: String::new(),
                    expected: String::new(),
                    kind: TestKind::Custom,
                });
                storage::save_tests(&problem.dir, &tests)?;
                open_testcase(workspace, &problem, tests.len(), window, cx);
                Ok(())
            });
        })
        .register_action(|workspace, action: &EditTestcase, window, cx| {
            let test = action.test;
            with_problem(workspace, window, cx, move |workspace, problem, window, cx| {
                check_test(&problem, test)?;
                open_testcase(workspace, &problem, test, window, cx);
                Ok(())
            });
        })
        .register_action(|workspace, action: &DeleteTestcase, window, cx| {
            let test = action.test;
            with_problem(workspace, window, cx, move |_, problem, _, cx| {
                check_test(&problem, test)?;
                let mut tests = problem.tests.clone();
                tests.remove(test - 1);
                storage::save_tests(&problem.dir, &tests)?;
                notify(format!("Deleted testcase {test}"), false, cx);
                Ok(())
            });
        })
        .register_action(|workspace, action: &AcceptOutput, window, cx| {
            let test = action.test;
            with_problem(workspace, window, cx, move |_, problem, _, cx| {
                accept_output(&problem, test, cx)
            });
        })
        .register_action(|workspace, _: &NextProblem, window, cx| {
            jump_problem(workspace, 1, window, cx);
        })
        .register_action(|workspace, _: &PreviousProblem, window, cx| {
            jump_problem(workspace, -1, window, cx);
        })
        .register_action(|workspace, _: &OpenProblem, window, cx| {
            open_problem_picker(workspace, window, cx);
        })
        .register_action(|workspace, _: &OpenSolutions, window, cx| {
            open_solutions_picker(workspace, window, cx);
        })
        .register_action(|workspace, _: &OpenProblemUrl, window, cx| {
            with_problem(workspace, window, cx, |_, problem, _, cx| {
                if problem.meta.url.is_empty() {
                    anyhow::bail!("this problem has no URL");
                }
                cx.open_url(&problem.meta.url);
                Ok(())
            });
        });
}

fn notify(message: String, is_error: bool, cx: &mut App) {
    struct CompetitiveProgrammingNotification;
    if is_error {
        log::error!("competitive programming: {message}");
    }
    cx.global_mut::<GlobalState>().status = message.clone();
    show_app_notification(
        NotificationId::unique::<CompetitiveProgrammingNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(|cx| workspace::notifications::simple_message_notification::MessageNotification::new(message, cx))
        },
    );
}

fn not_in_problem(cx: &mut App) {
    notify(
        "Not inside a problem (no .cptui/problem.toml). Receive one with `cp: start receiver`."
            .to_string(),
        true,
        cx,
    );
}

fn receiver_status(cx: &App) -> String {
    match &cx.global::<GlobalState>().receiver {
        Some(receiver) => format!("Receiving problems on http://{}", receiver.address),
        None => "The Competitive Companion receiver isn't running".to_string(),
    }
}

fn start_receiver(cx: &mut App) -> Result<()> {
    if cx.global::<GlobalState>().receiver.is_some() {
        return Ok(());
    }
    let settings = CompetitiveProgrammingSettings::get_global(cx).clone();
    let config = ReceiverConfig {
        workspace: settings.workspace.clone(),
        source: settings.source.clone(),
        template: settings.template(),
    };
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let receiver = Receiver::start(&settings.host, settings.port, config, events_tx)?;
    let events = cx.spawn(async move |cx| {
        while let Some(event) = events_rx.next().await {
            cx.update(|cx| handle_receiver_event(event, cx));
        }
    });
    let state = cx.global_mut::<GlobalState>();
    state.receiver = Some(receiver);
    state.receiver_events = Some(events);
    Ok(())
}

fn handle_receiver_event(event: ReceiverEvent, cx: &mut App) {
    match event {
        ReceiverEvent::ProblemReceived {
            dir,
            source,
            name,
            index,
            size,
        } => {
            let message = if size > 1 {
                if index >= size {
                    format!("Imported contest ({size} problems)")
                } else {
                    format!("Importing contest {index}/{size}")
                }
            } else {
                format!("Imported {name}")
            };
            cx.global_mut::<GlobalState>().last_problem = Some(dir);
            notify(message, false, cx);
            if index == 1 && CompetitiveProgrammingSettings::get_global(cx).open_on_receive {
                with_any_workspace(cx, move |workspace, window, cx| {
                    open_path(workspace, source, window, cx);
                });
            }
        }
        ReceiverEvent::SolutionsSaved {
            dir,
            written,
            existing,
        } => notify(
            format!(
                "Solutions saved: {written} new, {existing} existing → {}",
                dir.display()
            ),
            false,
            cx,
        ),
        ReceiverEvent::Status { message, is_error } => notify(message, is_error, cx),
    }
}

/// Runs `f` in the active workspace, or in any open workspace when Zed isn't
/// focused, as it usually isn't while problems arrive from the browser.
fn with_any_workspace(
    cx: &mut App,
    f: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + Send + 'static,
) {
    let window = cx
        .active_window()
        .and_then(|window| window.downcast::<MultiWorkspace>())
        .or_else(|| {
            cx.windows()
                .into_iter()
                .find_map(|window| window.downcast::<MultiWorkspace>())
        });
    match window {
        Some(window) => {
            cx.defer(move |cx| {
                window
                    .update(cx, |multi_workspace, window, cx| {
                        let workspace = multi_workspace.workspace().clone();
                        workspace.update(cx, |workspace, cx| f(workspace, window, cx));
                    })
                    .log_err();
            });
        }
        None => workspace::with_active_or_new_workspace(cx, f),
    }
}

fn open_path(
    workspace: &mut Workspace,
    path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace
        .open_abs_path(path, OpenOptions::default(), window, cx)
        .detach_and_log_err(cx);
}

fn active_file_path(workspace: &Workspace, cx: &App) -> Option<PathBuf> {
    let editor = workspace.active_item_as::<Editor>(cx)?;
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let file = buffer.read(cx).file()?.as_local()?;
    Some(file.abs_path(cx))
}

/// The problem of the active file, else the last problem used.
fn current_problem_dir(workspace: &Workspace, cx: &mut App) -> Option<PathBuf> {
    let dir = active_file_path(workspace, cx)
        .and_then(|path| storage::find_problem_dir(&path))
        .or_else(|| cx.global::<GlobalState>().last_problem.clone())?;
    cx.global_mut::<GlobalState>().last_problem = Some(dir.clone());
    Some(dir)
}

fn with_problem(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    f: impl FnOnce(&mut Workspace, Problem, &mut Window, &mut Context<Workspace>) -> Result<()>,
) {
    let Some(dir) = current_problem_dir(workspace, cx) else {
        not_in_problem(cx);
        return;
    };
    let result = storage::load_problem(&dir).and_then(|problem| f(workspace, problem, window, cx));
    if let Err(error) = result {
        notify(format!("{error:#}"), true, cx);
    }
}

fn check_test(problem: &Problem, test: usize) -> Result<()> {
    if test == 0 || test > problem.tests.len() {
        anyhow::bail!(
            "there is no testcase {test} (the problem has {})",
            problem.tests.len()
        );
    }
    Ok(())
}

fn open_testcase(
    workspace: &mut Workspace,
    problem: &Problem,
    test: usize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    open_path(workspace, problem.input_path(test), window, cx);
    open_path(workspace, problem.expected_path(test), window, cx);
}

fn accept_output(problem: &Problem, test: Option<usize>, cx: &mut App) -> Result<()> {
    let state = cx.global::<GlobalState>();
    let report = state
        .reports
        .get(&problem.dir)
        .context("run the tests before accepting their output")?;
    let numbers = match test {
        Some(test) => vec![test],
        None => report.tests.clone(),
    };
    let mut tests = problem.tests.clone();
    let mut accepted = 0;
    for number in numbers {
        let Some(TestState::Finished(result)) = report.results.get(&number) else {
            continue;
        };
        if let Some(test) = tests.get_mut(number - 1) {
            test.expected = result.stdout.clone();
            accepted += 1;
        }
    }
    storage::save_tests(&problem.dir, &tests)?;
    notify(format!("Saved the output of {accepted} test(s) as expected"), false, cx);
    Ok(())
}

fn jump_problem(
    workspace: &mut Workspace,
    step: isize,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(dir) = current_problem_dir(workspace, cx) else {
        not_in_problem(cx);
        return;
    };
    let problems = storage::contest_problems(&dir);
    let Some(position) = problems.iter().position(|problem| problem.dir == dir) else {
        return;
    };
    let target = position.checked_add_signed(step).and_then(|index| problems.get(index));
    match target {
        Some(problem) => {
            cx.global_mut::<GlobalState>().last_problem = Some(problem.dir.clone());
            open_path(workspace, problem.source_path(), window, cx);
        }
        None if step > 0 => notify("Already at the last problem".to_string(), false, cx),
        None => notify("Already at the first problem".to_string(), false, cx),
    }
}

fn run_tests(
    workspace: &mut Workspace,
    test: Option<usize>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(dir) = current_problem_dir(workspace, cx) else {
        not_in_problem(cx);
        return;
    };
    let settings = CompetitiveProgrammingSettings::get_global(cx).clone();
    let save = if active_file_path(workspace, cx).is_some() {
        workspace.save_active_item(SaveIntent::Save, window, cx)
    } else {
        Task::ready(Ok(()))
    };
    let executor = cx.background_executor().clone();
    cx.spawn_in(window, async move |workspace, cx| {
        save.await.log_err();
        let problem = storage::load_problem(&dir)?;
        let numbers = match test {
            Some(test) => {
                check_test(&problem, test)?;
                vec![test]
            }
            None => (1..=problem.tests.len()).collect(),
        };
        let language = settings.language_for(&problem.source_path())?;
        let timeout = settings.timeout_for(&problem);
        let report = RunReport {
            problem: problem.clone(),
            tests: numbers.clone(),
            compiling: true,
            compile_output: String::new(),
            compile_error: None,
            results: numbers
                .iter()
                .map(|number| (*number, TestState::Pending))
                .collect(),
        };
        workspace.update_in(cx, |workspace, window, cx| {
            cx.global_mut::<GlobalState>()
                .reports
                .insert(dir.clone(), report);
            show_results(workspace, &dir, window, cx);
        })?;

        let compiled = cx
            .background_spawn({
                let problem = problem.clone();
                let language = language.clone();
                async move { runner::compile(&problem, &language).await }
            })
            .await?;
        let variables = match compiled {
            Ok((variables, output)) => {
                update_report(&dir, cx, |report| {
                    report.compiling = false;
                    report.compile_output = output;
                });
                variables
            }
            Err(error) => {
                update_report(&dir, cx, |report| {
                    report.compiling = false;
                    report.compile_error = Some(error);
                });
                return anyhow::Ok(());
            }
        };

        let variables = Arc::new(variables);
        let mut runs = numbers
            .iter()
            .filter_map(|number| {
                let test = problem.tests.get(number - 1)?.clone();
                let number = *number;
                let language = language.clone();
                let variables = variables.clone();
                let executor = executor.clone();
                Some(cx.background_spawn(async move {
                    let result =
                        runner::run_test(&test, &language, &variables, timeout, executor).await;
                    (number, result)
                }))
            })
            .collect::<FuturesUnordered<_>>();
        while let Some((number, result)) = runs.next().await {
            update_report(&dir, cx, |report| {
                report.results.insert(number, TestState::Finished(result));
            });
        }
        anyhow::Ok(())
    })
    .detach_and_prompt_err("Failed to run tests", window, cx, |_, _, _| None);
}

fn update_report(dir: &Path, cx: &mut gpui::AsyncWindowContext, f: impl FnOnce(&mut RunReport)) {
    cx.update(|_, cx| {
        let state = cx.global_mut::<GlobalState>();
        let Some(report) = state.reports.get_mut(dir) else {
            return;
        };
        f(report);
        let text = report.render();
        if let Some(buffer) = state
            .results_buffers
            .get(dir)
            .and_then(|buffer| buffer.upgrade())
        {
            set_read_only_text(&buffer, text, cx);
        }
    })
    .log_err();
}

fn set_read_only_text(buffer: &Entity<Buffer>, text: String, cx: &mut App) {
    buffer.update(cx, |buffer, cx| {
        buffer.set_capability(Capability::ReadWrite, cx);
        buffer.set_text(text, cx);
        buffer.set_capability(Capability::ReadOnly, cx);
    });
}

/// Opens the results of the last run of the problem in `dir`, reusing its
/// tab when there is one.
fn show_results(
    workspace: &mut Workspace,
    dir: &Path,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let text = cx
        .global::<GlobalState>()
        .reports
        .get(dir)
        .map(RunReport::render)
        .unwrap_or_else(|| "No tests have run yet. Use `cp: run tests`.\n".to_string());
    let existing_buffer = cx
        .global::<GlobalState>()
        .results_buffers
        .get(dir)
        .and_then(|buffer| buffer.upgrade());
    if let Some(buffer) = existing_buffer {
        set_read_only_text(&buffer, text.clone(), cx);
        let existing_editor = workspace.items_of_type::<Editor>(cx).find(|editor| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .is_some_and(|singleton| singleton == buffer)
        });
        if let Some(editor) = existing_editor {
            // Keep focus on the source so it can be edited while tests run.
            workspace.activate_item(&editor, true, false, window, cx);
            return;
        }
    }

    let title: SharedString = format!(
        "Results: {}",
        dir.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
    .into();
    let dir = dir.to_path_buf();
    let language = workspace
        .app_state()
        .languages
        .language_for_name("Markdown");
    cx.spawn_in(window, async move |workspace, cx| {
        let language = language.await.log_err();
        let buffer = workspace
            .update(cx, |workspace, cx| {
                workspace
                    .project()
                    .update(cx, |project, cx| project.create_buffer(language, false, cx))
            })?
            .await?;
        workspace.update_in(cx, |workspace, window, cx| {
            set_read_only_text(&buffer, text, cx);
            cx.global_mut::<GlobalState>()
                .results_buffers
                .insert(dir, buffer.downgrade());
            let multi_buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx).with_title(title.to_string()));
            let editor = cx.new(|cx| {
                let mut editor = Editor::for_multibuffer(
                    multi_buffer,
                    Some(workspace.project().clone()),
                    window,
                    cx,
                );
                editor.set_read_only(true);
                editor
            });
            workspace.split_item(
                workspace::SplitDirection::Right,
                Box::new(editor),
                window,
                cx,
            );
        })
    })
    .detach_and_log_err(cx);
}

fn open_problem_picker(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let settings = CompetitiveProgrammingSettings::get_global(cx);
    let root = settings.workspace.clone();
    let mut problems = storage::list_problems(&root);
    if problems.is_empty() {
        notify(format!("No problems in {}", root.display()), true, cx);
        return;
    }
    // Problems of the current contest come first.
    if let Some(parent) = current_problem_dir(workspace, cx)
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
    {
        problems.sort_by_key(|problem| problem.dir.parent() != Some(parent.as_path()));
    }
    let entries = problems
        .into_iter()
        .map(|problem| {
            let relative = problem
                .dir
                .strip_prefix(&root)
                .unwrap_or(&problem.dir)
                .display()
                .to_string();
            PickerEntry {
                label: format!("{relative}  {}  [{}]", problem.meta.name, problem.meta.status),
                path: problem.source_path(),
            }
        })
        .collect();
    toggle_picker(workspace, "Open a problem…", entries, window, cx);
}

fn open_solutions_picker(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(dir) = current_problem_dir(workspace, cx) else {
        not_in_problem(cx);
        return;
    };
    let solutions = dir.join("solutions");
    let mut entries = std::fs::read_dir(&solutions)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.is_file()
                        && !matches!(
                            path.extension().and_then(|extension| extension.to_str()),
                            Some("png" | "jpg" | "webp")
                        )
                })
                .map(|path| PickerEntry {
                    label: path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        notify(
            "No solutions yet: start the receiver, open the problem in the browser and click \
             \"Fetch top 3 solutions\" in the cptui extension."
                .to_string(),
            true,
            cx,
        );
        return;
    }
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    toggle_picker(workspace, "Open a solution…", entries, window, cx);
}

fn toggle_picker(
    workspace: &mut Workspace,
    placeholder: &'static str,
    entries: Vec<PickerEntry>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        let delegate = PathPickerDelegate::new(handle, placeholder, entries);
        Picker::uniform_list(delegate, window, cx).initial_width(rems(48.))
    });
}

struct PickerEntry {
    label: String,
    path: PathBuf,
}

struct PathPickerDelegate {
    workspace: WeakEntity<Workspace>,
    placeholder: &'static str,
    entries: Vec<PickerEntry>,
    candidates: Arc<[StringMatchCandidate]>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl PathPickerDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        placeholder: &'static str,
        entries: Vec<PickerEntry>,
    ) -> Self {
        let candidates = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| StringMatchCandidate::new(index, &entry.label))
            .collect();
        Self {
            workspace,
            placeholder,
            entries,
            candidates,
            matches: Vec::new(),
            selected_index: 0,
        }
    }
}

impl PickerDelegate for PathPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "competitive programming picker"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = index;
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        self.placeholder.into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = if query.is_empty() {
                candidates
                    .iter()
                    .map(|candidate| StringMatch {
                        candidate_id: candidate.id,
                        string: candidate.string.clone(),
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                let mut matches = match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    candidates.len(),
                    &Default::default(),
                    background,
                )
                .await;
                matches.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.candidate_id.cmp(&b.candidate_id))
                });
                matches
            };
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    picker.delegate.selected_index = 0;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|found| self.entries.get(found.candidate_id))
        else {
            return;
        };
        let path = entry.path.clone();
        if let Some(dir) = storage::find_problem_dir(&path) {
            cx.global_mut::<GlobalState>().last_problem = Some(dir);
        }
        self.workspace
            .update(cx, |workspace, cx| open_path(workspace, path, window, cx))
            .log_err();
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
        let found = self.matches.get(index)?;
        Some(
            ListItem::new(index)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    found.string.clone(),
                    found.positions.clone(),
                )),
        )
    }
}
