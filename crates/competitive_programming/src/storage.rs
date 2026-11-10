//! The on-disk workspace, byte-compatible with cptui's TUI and Neovim plugin:
//!
//! ```text
//! <workspace>/[<contest>/]<id>/{main.cpp, tests/{N.in, N.out, tests.json}, .cptui/problem.toml}
//! ```

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_TEMPLATE: &str = r#"#include <bits/stdc++.h>
using namespace std;

using ll = long long;

void solve() {
    int n;
    cin >> n;
    cout << n << "\n";
}

int main() {
    ios::sync_with_stdio(false);
    cin.tie(nullptr);

    int t = 1;
    // cin >> t;

    while (t--) {
        solve();
    }

    return 0;
}
"#;

/// A problem as Competitive Companion sends it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Task {
    pub name: String,
    pub group: String,
    pub url: String,
    pub interactive: bool,
    pub memory_limit: u64,
    pub time_limit: u64,
    pub tests: Vec<TaskTest>,
    pub batch: TaskBatch,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TaskTest {
    pub input: String,
    pub output: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TaskBatch {
    pub id: String,
    pub size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ProblemMeta {
    pub id: String,
    pub name: String,
    pub group: String,
    pub url: String,
    pub interactive: bool,
    pub memory_limit_mb: u64,
    pub time_limit_ms: u64,
    pub status: String,
    pub source: String,
    pub batch_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestKind {
    Sample,
    Custom,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Testcase {
    pub input: String,
    pub expected: String,
    pub kind: TestKind,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub dir: PathBuf,
    pub meta: ProblemMeta,
    pub tests: Vec<Testcase>,
}

impl Problem {
    pub fn source_path(&self) -> PathBuf {
        self.dir.join(&self.meta.source)
    }

    pub fn tests_dir(&self) -> PathBuf {
        self.dir.join("tests")
    }

    pub fn input_path(&self, test: usize) -> PathBuf {
        self.tests_dir().join(format!("{test}.in"))
    }

    pub fn expected_path(&self, test: usize) -> PathBuf {
        self.tests_dir().join(format!("{test}.out"))
    }
}

#[derive(Serialize, Deserialize, Default)]
struct TestsIndex {
    entries: Vec<TestsIndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct TestsIndexEntry {
    number: usize,
    #[serde(rename = "type")]
    kind: TestKind,
}

/// The same rules as cptui's `config::sanitize`.
pub fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|character| {
            if character == ' ' {
                '-'
            } else if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = replaced.trim_matches(|character| matches!(character, '.' | '-' | '_'));
    if trimmed.is_empty() {
        "problem".to_string()
    } else {
        trimmed.to_string()
    }
}

/// "A. Foo" -> "A", "C1 - Bar" -> "C1"; otherwise a sanitized slug of at most
/// 24 characters.
pub fn problem_id(name: &str) -> String {
    let trimmed = name.trim();
    let code_length = trimmed
        .chars()
        .take(3)
        .take_while(|character| character.is_ascii_alphanumeric())
        .count();
    if code_length > 0
        && trimmed
            .chars()
            .nth(code_length)
            .is_some_and(|separator| matches!(separator, '.' | ' ' | '-' | ':' | '\t'))
    {
        return trimmed[..code_length].to_uppercase();
    }
    sanitize(trimmed).chars().take(24).collect()
}

/// "Codeforces - Codeforces Round 999" -> "Codeforces-Round-999".
pub fn contest_name(group: &str) -> String {
    let name = group
        .rfind(" - ")
        .map_or(group, |index| &group[index + 3..]);
    sanitize(name)
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

pub fn encode_meta(meta: &ProblemMeta) -> String {
    [
        format!("id = {}", toml_string(&meta.id)),
        format!("name = {}", toml_string(&meta.name)),
        format!("group = {}", toml_string(&meta.group)),
        format!("url = {}", toml_string(&meta.url)),
        format!("interactive = {}", meta.interactive),
        format!("memory_limit_mb = {}", meta.memory_limit_mb),
        format!("time_limit_ms = {}", meta.time_limit_ms),
        format!("status = {}", toml_string(&meta.status)),
        format!("source = {}", toml_string(&meta.source)),
        format!("batch_id = {}", toml_string(&meta.batch_id)),
        String::new(),
    ]
    .join("\n")
}

pub fn decode_meta(raw: &str) -> Result<ProblemMeta> {
    let mut meta: ProblemMeta = toml::from_str(raw).context("invalid .cptui/problem.toml")?;
    if meta.source.is_empty() {
        meta.source = "main.cpp".to_string();
    }
    Ok(meta)
}

fn write(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))
}

pub struct CreatedProblem {
    pub dir: PathBuf,
    pub meta: ProblemMeta,
}

/// Creates (or refreshes) a problem from a Competitive Companion task.
pub fn create_problem(
    workspace: &Path,
    source: &str,
    template: &str,
    task: &Task,
) -> Result<CreatedProblem> {
    let is_contest = task.batch.size > 1;
    let mut parent = workspace.to_path_buf();
    if is_contest {
        let contest = contest_name(&task.group);
        parent = workspace.join(&contest);
        let started_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        write(
            &parent.join(".cptui/contest.toml"),
            &format!(
                "name = {}\nbatch_id = {}\nstarted_at = \"{started_at}\"\n",
                toml_string(&contest),
                toml_string(&task.batch.id)
            ),
        )?;
    }
    let id = problem_id(&task.name);
    let dir = parent.join(sanitize(&id));
    let meta = ProblemMeta {
        name: if task.name.is_empty() {
            id.clone()
        } else {
            task.name.clone()
        },
        id,
        group: task.group.clone(),
        url: task.url.clone(),
        interactive: task.interactive,
        memory_limit_mb: task.memory_limit,
        time_limit_ms: task.time_limit,
        status: "Unopened".to_string(),
        source: source.to_string(),
        batch_id: task.batch.id.clone(),
    };
    let source_path = dir.join(&meta.source);
    if !source_path.exists() {
        write(&source_path, template)?;
    }
    let tests = task
        .tests
        .iter()
        .map(|test| Testcase {
            input: test.input.clone(),
            expected: test.output.clone(),
            kind: TestKind::Sample,
        })
        .collect::<Vec<_>>();
    save_meta(&dir, &meta)?;
    save_tests(&dir, &tests)?;
    Ok(CreatedProblem { dir, meta })
}

pub fn save_meta(dir: &Path, meta: &ProblemMeta) -> Result<()> {
    write(&dir.join(".cptui/problem.toml"), &encode_meta(meta))
}

/// The problem directory containing `path`: the closest ancestor with a
/// `.cptui/problem.toml`.
pub fn find_problem_dir(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(".cptui/problem.toml").is_file())
        .map(Path::to_path_buf)
}

pub fn load_problem(dir: &Path) -> Result<Problem> {
    let raw = fs::read_to_string(dir.join(".cptui/problem.toml"))
        .with_context(|| format!("no problem in {}", dir.display()))?;
    Ok(Problem {
        dir: dir.to_path_buf(),
        meta: decode_meta(&raw)?,
        tests: load_tests(dir)?,
    })
}

pub fn load_tests(dir: &Path) -> Result<Vec<Testcase>> {
    let tests_dir = dir.join("tests");
    let index: TestsIndex = fs::read_to_string(tests_dir.join("tests.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let mut numbers = Vec::new();
    if let Ok(entries) = fs::read_dir(&tests_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(number) = name
                .strip_suffix(".in")
                .and_then(|stem| stem.parse::<usize>().ok())
            {
                numbers.push(number);
            }
        }
    }
    numbers.sort_unstable();
    let mut tests = Vec::new();
    for number in numbers {
        let kind = index
            .entries
            .iter()
            .find(|entry| entry.number == number)
            .map_or(TestKind::Sample, |entry| entry.kind);
        tests.push(Testcase {
            input: fs::read_to_string(tests_dir.join(format!("{number}.in"))).unwrap_or_default(),
            expected: fs::read_to_string(tests_dir.join(format!("{number}.out")))
                .unwrap_or_default(),
            kind,
        });
    }
    Ok(tests)
}

/// Rewrites the tests as contiguous `1..=N` files plus cptui's index.
pub fn save_tests(dir: &Path, tests: &[Testcase]) -> Result<()> {
    let tests_dir = dir.join("tests");
    fs::create_dir_all(&tests_dir)
        .with_context(|| format!("failed to create {}", tests_dir.display()))?;
    for entry in fs::read_dir(&tests_dir)?.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let is_test_file = name
            .strip_suffix(".in")
            .or_else(|| name.strip_suffix(".out"))
            .is_some_and(|stem| stem.parse::<usize>().is_ok());
        if is_test_file {
            fs::remove_file(entry.path())
                .with_context(|| format!("failed to remove {}", entry.path().display()))?;
        }
    }
    let mut index = TestsIndex::default();
    for (position, test) in tests.iter().enumerate() {
        let number = position + 1;
        write(&tests_dir.join(format!("{number}.in")), &test.input)?;
        write(&tests_dir.join(format!("{number}.out")), &test.expected)?;
        index.entries.push(TestsIndexEntry {
            number,
            kind: test.kind,
        });
    }
    write(&tests_dir.join("tests.json"), &serde_json::to_string(&index)?)
}

/// Sort key so that problem 2 comes before 10 and A before B.
fn natural_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut key = String::new();
    let mut digits = String::new();
    for character in text.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if !digits.is_empty() {
            key.push_str(&format!("{digits:0>12}"));
            digits.clear();
        }
        key.push(character);
    }
    if !digits.is_empty() {
        key.push_str(&format!("{digits:0>12}"));
    }
    key
}

fn sort_problems(problems: &mut [Problem]) {
    problems.sort_by_cached_key(|problem| natural_key(&problem.dir));
}

/// The problems sharing `dir`'s parent folder (its contest), in order.
pub fn contest_problems(dir: &Path) -> Vec<Problem> {
    let Some(parent) = dir.parent() else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    if let Ok(entries) = fs::read_dir(parent) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Ok(problem) = load_problem(&entry.path())
            {
                problems.push(problem);
            }
        }
    }
    sort_problems(&mut problems);
    problems
}

/// Every problem in the workspace, at `<problem>` or `<contest>/<problem>`.
pub fn list_problems(workspace: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut directories = vec![(workspace.to_path_buf(), 0)];
    while let Some((directory, depth)) = directories.pop() {
        if let Ok(problem) = load_problem(&directory) {
            problems.push(problem);
            continue;
        }
        if depth >= 3 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let skipped = entry
                .file_name()
                .to_str()
                .is_some_and(|name| matches!(name, "tests" | "solutions" | ".git" | ".cptui"));
            if path.is_dir() && !skipped {
                directories.push((path, depth + 1));
            }
        }
    }
    sort_problems(&mut problems);
    problems
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_names_match_cptui() {
        assert_eq!(problem_id("A. Watermelon"), "A");
        assert_eq!(problem_id("c1 - Bar"), "C1");
        assert_eq!(problem_id("Two Sum"), "TWO");
        assert_eq!(problem_id("Watermelon Problem"), "Watermelon-Problem");
        assert_eq!(
            contest_name("Codeforces - Codeforces Round 999 (Div. 2)"),
            "Codeforces-Round-999-_Div.-2"
        );
        assert_eq!(sanitize("..héllo.."), "h_llo");
        assert_eq!(sanitize("***"), "problem");
    }

    #[test]
    fn test_meta_round_trip() {
        let meta = ProblemMeta {
            id: "A".into(),
            name: "A. \"Quoted\"".into(),
            group: "Codeforces".into(),
            url: "https://codeforces.com/contest/1/problem/A".into(),
            interactive: false,
            memory_limit_mb: 256,
            time_limit_ms: 1000,
            status: "Unopened".into(),
            source: "main.cpp".into(),
            batch_id: "abc".into(),
        };
        assert_eq!(decode_meta(&encode_meta(&meta)).ok(), Some(meta));
    }

    #[test]
    fn test_create_and_load_problem() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let task: Task = serde_json::from_str(
            r#"{"name":"B. Two","group":"Codeforces - Round 5","url":"u","timeLimit":2000,
                "tests":[{"input":"1\n","output":"2\n"}],"batch":{"id":"x","size":2}}"#,
        )?;
        let created = create_problem(workspace.path(), "main.cpp", DEFAULT_TEMPLATE, &task)?;
        assert_eq!(created.dir, workspace.path().join("Round-5/B"));
        let problem = load_problem(&created.dir)?;
        assert_eq!(problem.meta.time_limit_ms, 2000);
        assert_eq!(problem.tests.len(), 1);
        assert_eq!(problem.tests[0].expected, "2\n");

        let mut tests = problem.tests.clone();
        tests.push(Testcase {
            input: "5\n".into(),
            expected: String::new(),
            kind: TestKind::Custom,
        });
        save_tests(&created.dir, &tests)?;
        assert_eq!(load_tests(&created.dir)?, tests);
        assert_eq!(
            find_problem_dir(&created.dir.join("main.cpp")),
            Some(created.dir.clone())
        );
        assert_eq!(list_problems(workspace.path()).len(), 1);
        Ok(())
    }
}
