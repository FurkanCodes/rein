//! A check's output, read into errors once, here: each tool writes its own
//! shape, and only errors are kept, never warnings or notes. Paths come
//! out relative to the project, as the model names files.

use std::path::Path;

use serde::Deserialize;

use super::{Problem, Reads};

/// The most of a failed check's output kept to say how it failed.
const TAIL: usize = 1500;

/// The errors in what a check that ran as `reads` says printed, `stdout`
/// and `stderr`, with exit `code`, in the project at `root`. One that
/// failed without an error rein can read is a [`Problem::Failed`].
pub(super) fn problems(reads: Reads, root: &Path, stdout: &str, stderr: &str, code: Option<i32>) -> Vec<Problem> {
    let mut found = match reads {
        Reads::Cargo => cargo(stdout),
        Reads::Tsc => lines(stdout, tsc),
        Reads::Go => lines(stderr, go),
        Reads::Mypy => lines(stdout, mypy),
        Reads::Pyright => pyright(stdout),
        Reads::ExitCode => Vec::new(),
    };
    for problem in &mut found {
        if let Problem::At { file, .. } = problem {
            *file = relative(root, file);
        }
    }
    // `--all-targets` checks a library twice, as itself and with its tests.
    let mut unique: Vec<Problem> = Vec::new();
    for problem in found {
        if !unique.contains(&problem) {
            unique.push(problem);
        }
    }
    if unique.is_empty() && code != Some(0) {
        unique.push(Problem::Failed { code, tail: tail(if stderr.trim().is_empty() { stdout } else { stderr }) });
    }
    unique
}

/// `path`, from the project's root where it's in the project.
fn relative(root: &Path, path: &str) -> String {
    let path = path.strip_prefix("./").unwrap_or(path);
    Path::new(path).strip_prefix(root).map_or_else(|_| path.to_owned(), |rest| rest.display().to_string())
}

/// The last lines of `text`, plain, at most [`TAIL`] bytes.
fn tail(text: &str) -> String {
    let plain: Vec<String> = text.lines().map(crate::background::plain).filter(|line| !line.trim().is_empty()).collect();
    let mut kept = Vec::new();
    let mut size = 0;
    for line in plain.iter().rev() {
        size += line.len() + 1;
        if size > TAIL {
            break;
        }
        kept.push(line.as_str());
    }
    kept.reverse();
    kept.join("\n")
}

fn lines(text: &str, read: impl Fn(&str) -> Option<Problem>) -> Vec<Problem> {
    text.lines().filter_map(read).collect()
}

#[derive(Deserialize)]
struct CargoLine {
    reason: String,
    message: Option<Rustc>,
}

#[derive(Deserialize)]
struct Rustc {
    message: String,
    level: String,
    code: Option<RustcCode>,
    spans: Vec<Span>,
}

#[derive(Deserialize)]
struct RustcCode {
    code: String,
}

#[derive(Deserialize)]
struct Span {
    file_name: String,
    line_start: u32,
    column_start: u32,
    is_primary: bool,
}

/// rustc's errors, as cargo gives them with `--message-format=json`: those
/// at a place, so not "aborting due to 2 previous errors".
fn cargo(stdout: &str) -> Vec<Problem> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<CargoLine>(line).ok())
        .filter(|line| line.reason == "compiler-message")
        .filter_map(|line| line.message)
        .filter(|message| message.level.starts_with("error"))
        .filter_map(|message| {
            let span = message.spans.iter().find(|span| span.is_primary)?;
            Some(Problem::At { file: span.file_name.clone(), line: span.line_start, column: span.column_start, code: message.code.map(|code| code.code), message: message.message })
        })
        .collect()
}

/// `src/a.ts(10,5): error TS2304: Cannot find name 'foo'.`
fn tsc(line: &str) -> Option<Problem> {
    let (place, rest) = line.split_once("): error ")?;
    let (file, at) = place.rsplit_once('(')?;
    let (line_no, column) = at.split_once(',')?;
    let (code, message) = rest.split_once(": ")?;
    Some(Problem::At { file: file.to_owned(), line: line_no.parse().ok()?, column: column.parse().ok()?, code: Some(code.to_owned()), message: message.to_owned() })
}

/// `./a.go:3:2: undefined: x`, or the same after `vet: `.
fn go(line: &str) -> Option<Problem> {
    let line = line.strip_prefix("vet: ").unwrap_or(line);
    let (file, rest) = line.split_once(".go:")?;
    let mut parts = rest.splitn(3, ':');
    let (line_no, column, message) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.trim());
    Some(Problem::At { file: format!("{file}.go"), line: line_no, column, code: None, message: message.to_owned() })
}

/// `a.py:3:5: error: Name "x" is not defined  [name-defined]`.
fn mypy(line: &str) -> Option<Problem> {
    let (place, rest) = line.split_once(": error: ")?;
    let mut parts = place.rsplitn(3, ':');
    let (last, middle, first) = (parts.next()?, parts.next()?, parts.next());
    let (file, line_no, column) = match first {
        Some(file) => (file, middle.parse().ok()?, last.parse().ok()?),
        None => (middle, last.parse().ok()?, 1),
    };
    let (message, code) = match rest.rsplit_once("  [") {
        Some((message, code)) => (message, code.strip_suffix(']').map(str::to_owned)),
        None => (rest, None),
    };
    Some(Problem::At { file: file.to_owned(), line: line_no, column, code, message: message.to_owned() })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PyrightReport {
    general_diagnostics: Vec<PyrightDiagnostic>,
}

#[derive(Deserialize)]
struct PyrightDiagnostic {
    file: String,
    severity: String,
    message: String,
    range: Option<PyrightRange>,
    rule: Option<String>,
}

#[derive(Deserialize)]
struct PyrightRange {
    start: PyrightPosition,
}

#[derive(Deserialize)]
struct PyrightPosition {
    line: u32,
    character: u32,
}

/// pyright's `--outputjson` report: its lines and columns count from 0.
fn pyright(stdout: &str) -> Vec<Problem> {
    let Ok(report) = serde_json::from_str::<PyrightReport>(stdout) else { return Vec::new() };
    report
        .general_diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == "error")
        .map(|diagnostic| {
            let (line, column) = diagnostic.range.map_or((1, 1), |range| (range.start.line.saturating_add(1), range.start.character.saturating_add(1)));
            Problem::At { file: diagnostic.file, line, column, code: diagnostic.rule, message: diagnostic.message }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(file: &str, line: u32, column: u32, code: Option<&str>, message: &str) -> Problem {
        Problem::At { file: file.into(), line, column, code: code.map(str::to_owned), message: message.into() }
    }

    #[test]
    fn cargos_errors_are_read_from_its_json_and_its_noise_left_out() {
        let root = Path::new("/p");
        let error = r#"{"reason":"compiler-message","package_id":"x","manifest_path":"/p/Cargo.toml","target":{},"message":{"rendered":"error[E0308]: mismatched types\n","$message_type":"diagnostic","children":[],"code":{"code":"E0308","explanation":"..."},"level":"error","message":"mismatched types","spans":[{"byte_end":10,"byte_start":5,"column_end":9,"column_start":5,"expansion":null,"file_name":"src/lib.rs","is_primary":true,"label":"expected `usize`","line_end":3,"line_start":3,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;
        let warning = error.replace(r#""level":"error""#, r#""level":"warning""#);
        let aborting = r#"{"reason":"compiler-message","message":{"message":"aborting due to 1 previous error","level":"error","code":null,"spans":[]}}"#;
        let done = r#"{"reason":"build-finished","success":false}"#;
        let stdout = [error, &warning, aborting, error, done].join("\n");
        assert_eq!(problems(Reads::Cargo, root, &stdout, "", Some(101)), [at("src/lib.rs", 3, 5, Some("E0308"), "mismatched types")]);
        let clean = problems(Reads::Cargo, root, &format!("{warning}\n{done}"), "", Some(0));
        assert!(clean.is_empty(), "warnings aren't errors: {clean:?}");
    }

    #[test]
    fn a_check_that_fails_without_an_error_says_how_it_ended() {
        let broke = problems(Reads::Cargo, Path::new("/p"), "", "\x1b[31merror\x1b[0m: failed to parse manifest at `/p/Cargo.toml`\n", Some(101));
        assert_eq!(broke, [Problem::Failed { code: Some(101), tail: "error: failed to parse manifest at `/p/Cargo.toml`".into() }]);
        assert!(problems(Reads::ExitCode, Path::new("/p"), "ok\n", "", Some(0)).is_empty());
    }

    #[test]
    fn tsc_go_mypy_and_pyright_are_read_as_each_writes() {
        let root = Path::new("/p");
        let tsc_out = "src/a.ts(10,5): error TS2304: Cannot find name 'foo'.\n  more about it\nsrc/b.ts(1,1): error TS1005: ';' expected.\n";
        assert_eq!(
            problems(Reads::Tsc, root, tsc_out, "", Some(2)),
            [at("src/a.ts", 10, 5, Some("TS2304"), "Cannot find name 'foo'."), at("src/b.ts", 1, 1, Some("TS1005"), "';' expected.")]
        );
        let go_out = "# example.com/m\nvet: ./a.go:3:2: undefined: x\n./b/c.go:7:10: missing return\n";
        assert_eq!(problems(Reads::Go, root, "", go_out, Some(1)), [at("a.go", 3, 2, None, "undefined: x"), at("b/c.go", 7, 10, None, "missing return")]);
        let mypy_out = "a.py:3:5: error: Name \"x\" is not defined  [name-defined]\na.py:4: note: see this\nb.py:9: error: Missing return\n";
        assert_eq!(
            problems(Reads::Mypy, root, mypy_out, "", Some(1)),
            [at("a.py", 3, 5, Some("name-defined"), "Name \"x\" is not defined"), at("b.py", 9, 1, None, "Missing return")]
        );
        let pyright_out = r#"{"version":"1","generalDiagnostics":[{"file":"/p/a.py","severity":"error","message":"\"x\" is not defined","range":{"start":{"line":2,"character":4},"end":{"line":2,"character":5}},"rule":"reportUndefinedVariable"},{"file":"/p/a.py","severity":"warning","message":"unused","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}],"summary":{}}"#;
        assert_eq!(problems(Reads::Pyright, root, pyright_out, "", Some(1)), [at("a.py", 3, 5, Some("reportUndefinedVariable"), "\"x\" is not defined")]);
    }
}
