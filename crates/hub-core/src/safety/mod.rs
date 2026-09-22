//! Pre-write guard: target-tool process detection before a migration writes.
//!
//! A migration writes directly into the target tool's session files or
//! databases; if the target tool is running at the same time, concurrent
//! writes can conflict (ZCode uses two SQLite databases, where concurrent
//! writes also risk corruption). The UI calls [`is_tool_running`] when the
//! user clicks the migrate button; on a hit it warns first and only lets the
//! migration through after the user confirms "migrate anyway". This module
//! performs read-only probing only and has no side effects.

use crate::ir::Tool;

/// Returns whether a target tool process is currently running
/// (macOS / Linux / Windows).
///
/// Each tool is configured with a list of exact process names to probe; any
/// hit counts as running. Probing is best effort: when the platform's probe
/// utility is missing or fails, this returns false (fail open) — a missing
/// guard must not block the user, and the residual conflict risk is covered
/// by the writers' atomic writes / transactional backups.
pub fn is_tool_running(tool: Tool) -> bool {
    let (names, ignore_case) = tool_process_config(tool);
    names
        .iter()
        .any(|name| is_process_running(name, ignore_case))
}

/// Per-tool detection config: (exact process names, case-insensitive match).
///
/// Unix: the Claude Code CLI process is `claude`, the Codex CLI process is
/// `codex`, and the ZCode desktop app is `ZCode` with casing that varies
/// across versions/platforms, so it is matched case-insensitively.
/// Windows: process image names carry a `.exe` suffix (`tasklist` filtering
/// is case-insensitive by itself, so the flag is unused there).
#[cfg(unix)]
fn tool_process_config(tool: Tool) -> (&'static [&'static str], bool) {
    match tool {
        Tool::ClaudeCode => (&["claude"], false),
        Tool::Codex => (&["codex"], false),
        Tool::ZCode => (&["zcode"], true),
    }
}

/// Windows variant of the per-tool config, see the unix version above.
#[cfg(windows)]
fn tool_process_config(tool: Tool) -> (&'static [&'static str], bool) {
    match tool {
        Tool::ClaudeCode => (&["claude.exe"], false),
        Tool::Codex => (&["codex.exe"], false),
        Tool::ZCode => (&["ZCode.exe"], true),
    }
}

// ---------- Unix (macOS + Linux) ----------

/// Two-layer probe for a single exact process name: `pgrep -x` as the
/// primary layer, a full `ps` process-name scan as the fallback.
///
/// `pgrep -f` is deliberately not used (it matches full command lines): it
/// easily produces false hits from unrelated processes — even from the
/// arguments of our own process. Exact process names are the stable choice.
///
/// Why the ps fallback: on some macOS installations `pgrep` has blind spots
/// (e.g. it cannot see the Claude Code CLI main process), while `ps` goes
/// through the kinfo path and is unaffected. The two layers are OR-ed:
/// - either layer hits -> running (prefer a false alarm the user can
///   confirm over a miss that lets a conflict through);
/// - neither hits / the utilities fail -> not running (fail open).
#[cfg(unix)]
fn is_process_running(name: &str, ignore_case: bool) -> bool {
    pgrep_exact(name, ignore_case) || ps_comm_has(name, ignore_case)
}

/// `pgrep -x` (optionally `-i`) probes by exact process name: exit code 0
/// counts as a hit. pgrep excludes itself, so there is no self-match.
#[cfg(unix)]
fn pgrep_exact(name: &str, ignore_case: bool) -> bool {
    let mut command = std::process::Command::new("pgrep");
    command.arg("-x");
    if ignore_case {
        command.arg("-i");
    }
    command.arg(name);
    // Exit code 0 = hit, 1 = no hit; any other failure (pgrep missing,
    // bad arguments) counts as no hit.
    command
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Full `ps -axo comm=` process-name scan (works on macOS and Linux).
///
/// macOS prints the executable's absolute path (e.g.
/// `/Applications/ZCode.app/.../ZCode`), Linux prints the bare process
/// name; the last path segment is compared exactly (optionally ignoring
/// case) either way.
#[cfg(unix)]
fn ps_comm_has(name: &str, ignore_case: bool) -> bool {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "comm="])
        .output()
    else {
        return false;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().any(|line| {
        let process_name = line.trim().rsplit('/').next().unwrap_or_default();
        if ignore_case {
            process_name.eq_ignore_ascii_case(name)
        } else {
            process_name == name
        }
    })
}

// ---------- Windows ----------

/// Windows probe via `tasklist`: exact image-name filter, CSV output.
/// The `_ignore_case` parameter is accepted for signature parity with the
/// unix probe — tasklist filters and CSV parsing below are already
/// case-insensitive.
#[cfg(windows)]
fn is_process_running(image_name: &str, _ignore_case: bool) -> bool {
    let Ok(output) = std::process::Command::new("tasklist")
        .args(tasklist_args(image_name))
        .output()
    else {
        // tasklist itself unavailable: fail open (see module docs).
        return false;
    };
    output.status.success()
        && tasklist_output_has_name(&String::from_utf8_lossy(&output.stdout), image_name)
}

/// Builds the `tasklist` arguments: filter by exact image name, emit CSV
/// without a header (pure function, unit-testable).
///
/// Note that tasklist also exits 0 when nothing matches (it prints an
/// "INFO: No tasks..." line instead), so the stdout has to be inspected —
/// see [`tasklist_output_has_name`].
// Compiled on every platform so its unit tests run on every host.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn tasklist_args(image_name: &str) -> Vec<String> {
    vec![
        "/FI".to_string(),
        format!("IMAGENAME eq {image_name}"),
        "/FO".to_string(),
        "CSV".to_string(),
        "/NH".to_string(),
    ]
}

/// Parses `/FO CSV /NH` tasklist output: any line whose first CSV field
/// equals the image name (case-insensitive, surrounding quotes stripped)
/// counts as a hit (pure function, unit-testable). The "INFO: No tasks are
/// running..." message never matches, and neither do other CSV columns.
// Compiled on every platform so its unit tests run on every host.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn tasklist_output_has_name(stdout: &str, image_name: &str) -> bool {
    stdout.lines().any(|line| {
        let first_field = line.split(',').next().unwrap_or_default();
        first_field
            .trim_matches('"')
            .eq_ignore_ascii_case(image_name)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TC-SAFETY-01: a process that certainly exists is detected — on macOS
    /// PID 1 is launchd; on Linux systemd / init / the kthreadd kernel
    /// thread are tried, any hit passes. Runs the full two-layer probe
    /// (some macOS pgrep builds cannot see launchd, which doubles as a
    /// check of the ps fallback).
    #[cfg(unix)]
    #[test]
    fn tc_safety_01_detects_existing_process() {
        #[cfg(target_os = "macos")]
        let candidates = ["launchd"];
        #[cfg(not(target_os = "macos"))]
        let candidates = ["systemd", "init", "kthreadd"];
        assert!(
            candidates
                .iter()
                .any(|&name| is_process_running(name, false)),
            "system resident process not detected, probing path broken"
        );
    }

    /// TC-SAFETY-02: a process that does not exist is not detected. Uses a
    /// random long name derived from a uuid so it can never collide with a
    /// real process; both the case-sensitive and insensitive paths are
    /// exercised.
    #[cfg(unix)]
    #[test]
    fn tc_safety_02_misses_nonexistent_process() {
        let name = format!("hub-no-proc-{}", uuid::Uuid::new_v4());
        assert!(!is_process_running(&name, false));
        assert!(!is_process_running(&name, true));
    }

    /// TC-SAFETY-03: every tool has a non-empty detection config and the
    /// full `is_tool_running` path runs. The return value is not asserted —
    /// whether the tools actually run depends on the environment (e.g. the
    /// developer having claude open).
    #[test]
    fn tc_safety_03_tool_configs_runnable() {
        for tool in [Tool::ClaudeCode, Tool::Codex, Tool::ZCode] {
            let (names, _) = tool_process_config(tool);
            assert!(
                !names.is_empty(),
                "{tool:?} process config must not be empty"
            );
            let _ = is_tool_running(tool);
        }
    }

    /// TC-SAFETY-04: tasklist argument construction — exact image-name
    /// filter, CSV output format, no header. Runs on every host (pure
    /// function) so the Windows probe is regression-tested from macOS too.
    #[test]
    fn tc_safety_04_tasklist_args() {
        assert_eq!(
            tasklist_args("claude.exe"),
            vec!["/FI", "IMAGENAME eq claude.exe", "/FO", "CSV", "/NH"]
        );
    }

    /// TC-SAFETY-05: tasklist CSV parsing — a matching line hits (regardless
    /// of quoting and case), unrelated executables and the "no tasks" INFO
    /// line never hit.
    #[test]
    fn tc_safety_05_tasklist_output_parsing() {
        let stdout = "\"ZCode.exe\",\"1234\",\"Console\",\"1\",\"123,456 K\"\r\n\
                      \"explorer.exe\",\"99\",\"Console\",\"1\",\"45,678 K\"\r\n";
        assert!(tasklist_output_has_name(stdout, "ZCode.exe"));
        // Case-insensitive both ways
        assert!(tasklist_output_has_name(stdout, "zcode.exe"));
        assert!(!tasklist_output_has_name(stdout, "code.exe"));
        assert!(!tasklist_output_has_name(stdout, "explorer.exe.cmd"));
        // tasklist prints this INFO line (exit code 0) when nothing matches
        let no_match = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(!tasklist_output_has_name(no_match, "claude.exe"));
        assert!(!tasklist_output_has_name("", "claude.exe"));
    }

    /// TC-SAFETY-06 (Windows only): the full Windows probe detects the
    /// always-present svchost.exe. Only runs on a real Windows host.
    #[cfg(windows)]
    #[test]
    fn tc_safety_06_windows_detects_existing_process() {
        assert!(is_process_running("svchost.exe", true));
    }
}
