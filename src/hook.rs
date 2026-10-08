//! `vramorama hook`: a Claude Code PreToolUse hook that stops shell commands which would start a
//! process with a clean environment (WMI, Task Scheduler), since such a process can no longer be
//! traced to the session that started it.

use crate::json;

pub const REASON: &str = "vramorama: this starts the process through WMI or Task Scheduler, which gives it a fresh \
environment: it loses CLAUDE_CODE_SESSION_ID and nobody can tell later which session owns its GPU memory. \
Instead use `vramorama run --vram <size> --detach --out <logfile> -- <command>` (waits for free VRAM, keeps the \
tags, outlives the session), or pass your whole environment: \
$s = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ EnvironmentVariables = \
[string[]](Get-ChildItem env: | ForEach-Object { \"$($_.Name)=$($_.Value)\" }) } and add \
ProcessStartupInformation = $s to the Win32_Process Create arguments.";

/// The reason to block, if the hook input is a shell tool call that launches through WMI or
/// Task Scheduler without carrying its environment along.
pub fn check(input: &str) -> Option<&'static str> {
    let v = json::parse(input.trim_start_matches('\u{feff}')).ok()?;
    if !matches!(v.str("tool_name")?, "Bash" | "PowerShell") {
        return None;
    }
    let cmd = v.get("tool_input")?.str("command")?.to_ascii_lowercase();
    let wmi_call = ["invoke-cimmethod", "invoke-wmimethod", "[wmiclass]"].iter().any(|k| cmd.contains(k));
    let wmi =
        (cmd.contains("win32_process") && wmi_call && cmd.contains("create")) || cmd.contains("process call create");
    let scheduler = cmd.contains("register-scheduledtask") || (cmd.contains("schtasks") && cmd.contains("/create"));
    if !(wmi || scheduler) {
        return None;
    }
    // Already passing its environment, or launching through vramorama.
    if cmd.contains("processstartupinformation") || cmd.contains("environmentvariables") || cmd.contains("vramorama") {
        return None;
    }
    Some(REASON)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tool: &str, command: &str) -> String {
        json::obj([
            ("session_id", "x".into()),
            ("hook_event_name", "PreToolUse".into()),
            ("tool_name", tool.into()),
            ("tool_input", json::obj([("command", command.into())])),
        ])
        .to_string()
    }

    #[test]
    fn blocks_clean_environment_launches() {
        let wmi = r#"$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "python -u q.py" }"#;
        assert_eq!(check(&input("PowerShell", wmi)), Some(REASON));
        assert!(check(&input("Bash", "wmic process call create \"python q.py\"")).is_some());
        assert!(check(&input("PowerShell", "Register-ScheduledTask -TaskName q -Action $a")).is_some());
        assert!(check(&input("Bash", "schtasks /Create /TN q /TR python.exe")).is_some());
    }

    #[test]
    fn allows_everything_else() {
        let with_env = r#"Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $c; ProcessStartupInformation = $s }"#;
        assert_eq!(check(&input("PowerShell", with_env)), None);
        let query = r#"Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'create' }"#;
        assert_eq!(check(&input("PowerShell", query)), None);
        assert_eq!(check(&input("PowerShell", "vramorama run --vram 9G --detach -- python q.py")), None);
        assert_eq!(check(&input("Bash", "python train.py")), None);
        assert_eq!(check(&input("Write", "Win32_Process Create")), None);
        assert_eq!(check("not json"), None);
    }
}
