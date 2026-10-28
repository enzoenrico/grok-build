//! Runs Cursor's `.cursor/worktrees.json` setup in a freshly created worktree.
//! The file is looked up in the new worktree first, then the source checkout. On Unix `setup-worktree-unix` wins over
//! `setup-worktree`; on Windows `setup-worktree-windows` does. A list is shell commands run in order in the worktree,
//! and a string is a script path relative to the `.cursor/` directory. `ROOT_WORKTREE_PATH` names the source checkout.
//! Setup is repository-controlled code, so it runs only for a source folder the user explicitly trusted.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

const FILE: &str = "worktrees.json";
const OS_KEY: &str = if cfg!(windows) {
    "setup-worktree-windows"
} else {
    "setup-worktree-unix"
};
const OUTPUT_TAIL_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SetupSteps {
    Commands(Vec<String>),
    Script(PathBuf),
}

/// The setup for this worktree, or `None` when neither checkout has a `.cursor/worktrees.json` with a setup key.
pub(crate) fn load(worktree: &Path, source_root: &Path) -> Result<Option<SetupSteps>, String> {
    let Some(path) = [worktree, source_root]
        .iter()
        .map(|dir| dir.join(".cursor").join(FILE))
        .find(|p| p.is_file())
    else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some((key, entry)) = [OS_KEY, "setup-worktree"]
        .iter()
        .find_map(|key| value.get(*key).map(|v| (*key, v)))
    else {
        return Ok(None);
    };
    match entry {
        serde_json::Value::String(script) => {
            let dir = path.parent().unwrap_or(worktree);
            Ok(Some(SetupSteps::Script(dir.join(script))))
        }
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{}: {key} entries must be strings", path.display()))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|commands| Some(SetupSteps::Commands(commands))),
        _ => Err(format!(
            "{}: {key} must be a list of commands or a script path",
            path.display()
        )),
    }
}

/// Run each step in `worktree`, stopping at the first failure.
pub(crate) async fn run(
    steps: &SetupSteps,
    worktree: &Path,
    source_root: &Path,
) -> Result<(), String> {
    let commands: Vec<Command> = match steps {
        SetupSteps::Commands(lines) => lines.iter().map(|line| shell(line)).collect(),
        SetupSteps::Script(script) => vec![script_command(script)],
    };
    let labels: Vec<String> = match steps {
        SetupSteps::Commands(lines) => lines.clone(),
        SetupSteps::Script(script) => vec![script.display().to_string()],
    };
    for (mut command, label) in commands.into_iter().zip(labels) {
        let output = command
            .current_dir(worktree)
            .env("ROOT_WORKTREE_PATH", source_root)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("`{label}` could not start: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let tail: String = {
                let trimmed = stderr.trim();
                let skip = trimmed.chars().count().saturating_sub(OUTPUT_TAIL_CHARS);
                trimmed.chars().skip(skip).collect()
            };
            return Err(format!("`{label}` failed ({}): {tail}", output.status));
        }
    }
    Ok(())
}

fn shell(line: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(line);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(line);
        c
    }
}

fn script_command(script: &Path) -> Command {
    let is_ps1 = script
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
    if cfg!(windows) && is_ps1 {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script);
        c
    } else if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(script);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg(script);
        c
    }
}

#[cfg(test)]
#[path = "cursor_setup_tests.rs"]
mod tests;
