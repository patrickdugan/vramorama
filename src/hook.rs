//! `vramorama hook`: a Claude Code PreToolUse hook that stops shell commands which would start a
//! process with a clean environment (WMI, Task Scheduler), since such a process can no longer be
//! traced to the session that started it.

use crate::json;

/// The explanation the agent sees. `exe` is how to call vramorama: its absolute path, because a
/// session started before vramorama was put on PATH cannot find it by name.
pub fn reason(exe: &str) -> String {
    format!(
        "vramorama: this starts the process through WMI or Task Scheduler, which gives it a fresh environment: it \
loses CLAUDE_CODE_SESSION_ID and nobody can tell later which session owns its GPU memory. Instead use \
`& \"{exe}\" run --vram <size> --detach --out <logfile> -- <command>` (waits for free VRAM, keeps the tags, \
outlives the session), or pass your whole environment: $s = New-CimInstance -ClassName Win32_ProcessStartup \
-ClientOnly -Property @{{ EnvironmentVariables = [string[]](Get-ChildItem env: | ForEach-Object {{ \
\"$($_.Name)=$($_.Value)\" }}) }} and add ProcessStartupInformation = $s to the Win32_Process Create arguments."
    )
}

/// Whether the hook input is a shell tool call that launches through WMI or Task Scheduler
/// without carrying its environment along.
pub fn should_block(input: &str) -> bool {
    let Ok(v) = json::parse(input.trim_start_matches('\u{feff}')) else { return false };
    if !matches!(v.str("tool_name"), Some("Bash" | "PowerShell")) {
        return false;
    }
    let Some(cmd) = v.get("tool_input").and_then(|t| t.str("command")) else { return false };
    let cmd = cmd.to_ascii_lowercase();
    let wmi_call = ["invoke-cimmethod", "invoke-wmimethod", "[wmiclass]"].iter().any(|k| cmd.contains(k));
    let wmi =
        (cmd.contains("win32_process") && wmi_call && cmd.contains("create")) || cmd.contains("process call create");
    let scheduler = cmd.contains("register-scheduledtask") || (cmd.contains("schtasks") && cmd.contains("/create"));
    // Already passing its environment, or launching through vramorama.
    let carries_env =
        cmd.contains("processstartupinformation") || cmd.contains("environmentvariables") || cmd.contains("vramorama");
    (wmi || scheduler) && !carries_env
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
        assert!(should_block(&input("PowerShell", wmi)));
        assert!(should_block(&format!("\u{feff}{}", input("PowerShell", wmi))));
        assert!(should_block(&input("Bash", "wmic process call create \"python q.py\"")));
        assert!(should_block(&input("PowerShell", "Register-ScheduledTask -TaskName q -Action $a")));
        assert!(should_block(&input("Bash", "schtasks /Create /TN q /TR python.exe")));
    }

    #[test]
    fn allows_everything_else() {
        let with_env = r#"Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $c; ProcessStartupInformation = $s }"#;
        assert!(!should_block(&input("PowerShell", with_env)));
        let query = r#"Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -match 'create' }"#;
        assert!(!should_block(&input("PowerShell", query)));
        assert!(!should_block(&input("PowerShell", "vramorama run --vram 9G --detach -- python q.py")));
        assert!(!should_block(&input("Bash", "python train.py")));
        assert!(!should_block(&input("Write", "Win32_Process Create")));
        assert!(!should_block("not json"));
    }

    #[test]
    fn reason_names_the_executable() {
        let r = reason(r"C:\Tools\vramorama.exe");
        assert!(r.contains(r#"& "C:\Tools\vramorama.exe" run --vram"#));
        assert!(r.contains(r#"@{ EnvironmentVariables = [string[]](Get-ChildItem env:"#));
    }
}
