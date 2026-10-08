# vramorama

**Who is holding my GPU?** Per-process VRAM on a single workstation, with the agent session that
launched each process.

When several coding agents (and you) share one GPU, the card fills up overnight and nobody can say
what is on it. On Windows `nvidia-smi` cannot even tell you how much memory each process holds:
under the WDDM driver model NVML reports it as `N/A`. vramorama reads the numbers Windows keeps for
Task Manager, then works out *who* owns each process:

```text
> vramorama
NVIDIA GeForce RTX 5080 Laptop GPU: 9,860 MiB of 15,915 in use; 9,846 in the processes below, 14 elsewhere
    PID  VRAM MiB  GPU%  STARTED      OWNER                                         COMMAND
   8352     8,776    97  10-08 12:26  untagged, from cmd.exe 33612 (parent exited)  python.exe [bench] -u scripts/xbench.py j --arm J-u4 --seed 1
   2876     1,070    85  10-08 14:10  claude a91d07c4 (live) “Nightly eval queue”   python.exe [sweeps] -u hold.py 1024 45

Untagged processes: `vramorama trace <PID>` searches agent transcripts for the session that launched them.
```

```text
> vramorama trace 8352
pid 8352 python.exe  started 10-08 12:26  in C:\Users\me\GitHub\bench
  command   "C:\...\python.exe" -u scripts/xbench.py j --arm J-u4 --seed 1
  tagged    no (no VRAMORAMA_OWNER / CLAUDE_CODE_SESSION_ID / AI_AGENT in its environment)
  ancestry  python.exe 8352 <- python.exe 32132 <- python.exe 29884 <- python.exe 14160 <- cmd.exe 33612 <- [exited 22684]
  search    13 transcripts modified since 10-08 12:03
  launched  claude session 5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d “Model research with Qwen” (tool call 6s before the process started)
            C:\Users\me\.claude\projects\C--Users-me-GitHub-bench\5f3c1e2a-7d41-4b8e-9a6c-1e2f3a4b5c6d.jsonl:9382
            via python.exe 14160: -u scripts\run_queue.py --queue configs\queues\u6.yaml
  mentioned claude session a91d07c4-2b3e-4f5a-8c9d-0e1f2a3b4c5d “GPU usage manager”
            C:\Users\me\.claude\projects\C--Users-me-GitHub-tools\a91d07c4-2b3e-4f5a-8c9d-0e1f2a3b4c5d.jsonl:77
            via python.exe 14160: -u scripts\run_queue.py --queue configs\queues\u6.yaml
```

## Install

```sh
cargo install --locked --git https://github.com/patrickdugan/vramorama
```

Or clone and `cargo build --release --locked --offline`. Both Windows Rust toolchains work: MSVC
(with its usual Build Tools linker) and GNU (self-contained, no Visual Studio needed). Nothing else
is required: no SDK headers, no import libraries beyond what Rust itself links.

## Commands

| Command | What it does |
|---|---|
| `vramorama` / `vramorama ps` | Processes holding GPU memory now, with owner, start time, working folder and command. `--json`, `--all` (include integrated GPUs), `--min MIB` (default 16), `--wide`, `--trace` (trace every untagged process). |
| `vramorama watch` | Refreshes every `--interval` seconds (default 5) and keeps a **running watermark**: each process's peak, and each owner's current and peak total. `--log FILE` appends one JSON line per sample; `--quiet` only logs. |
| `vramorama report FILE` | Summarises a watch log: GiB-hours, peak, GPU% and time held per owner. Answers "what sat on the card all night?" Gaps (sleep, a stopped watcher) are capped so they do not count as use. `--json`. |
| `vramorama trace PID` | For an untagged process: its full launch chain, and the agent transcript that typed the command. |
| `vramorama run --vram 9G -- CMD…` | Waits for a VRAM lease (first come, first served), then runs `CMD` tagged with it. `--detach [--out LOG]` starts it outside the session. See [Sharing the card](#sharing-the-card-vramorama-run). |
| `vramorama leases` | The lease ledger and how much a new request could get right now. `--json`. |
| `vramorama hook` | A Claude Code `PreToolUse` hook that stops WMI / Task Scheduler launches that would strip a job's tags. See [The agent hook](#the-agent-hook). |

Leave a watcher running overnight:

```powershell
Start-Process vramorama -ArgumentList "watch","--interval","30","--log","$HOME\vram.jsonl","--quiet" -WindowStyle Hidden
# next morning
vramorama report $HOME\vram.jsonl
```

## How ownership works

Environment variables are inherited by every child process, so tags set where a job starts follow
it down the process tree with no wrapper and no cooperation from the job. vramorama reads the
environment of each GPU process (from its PEB, as Process Explorer and psutil do) and looks for:

| Variable | Set by | Shown as |
|---|---|---|
| `VRAMORAMA_OWNER` | you, a script, or an agent's instructions | the label, verbatim (takes priority) |
| `CLAUDE_CODE_SESSION_ID` | Claude Code, in every shell it starts | `claude <first 8>`, `(live)`/`(ended)` from `CLAUDE_PID`, and the session's title from its transcript |
| `AI_AGENT` | some agent harnesses | the value |

Tag anything by hand:

```powershell
$env:VRAMORAMA_OWNER = "sweep-lr-1e-4"; python train.py
```

### Jobs that lose their tags

Agents often detach long jobs so they outlive the session, and the usual trick on Windows is
`Invoke-CimMethod Win32_Process -MethodName Create`. That starts the job from the WMI service with a
**fresh** environment, so every tag is gone, and the launcher's parent exits soon after.
vramorama shows these as `untagged, via WmiPrvSE.exe …` (while the WMI host lives) or
`untagged, from <process> (parent exited)`, and `trace` recovers the owner by searching agent
transcripts (`~/.claude/projects`, `~/.codex/sessions`) for the job's command line, and its
ancestors' command lines. It reads only transcripts modified after the job started. A session is
reported as `launched` only if the command appears in one of its tool calls timestamped *before*
the process started; launches are ranked by how close the call was to the start. Sessions that
only saw the command in some tool's output, or typed it later, are listed as `mentioned`.

To keep tags across a WMI launch, pass your whole environment. `EnvironmentVariables` replaces the
environment rather than adding to it, so passing only the tags leaves the job without `PATH`:

```powershell
$envList = [string[]](Get-ChildItem env: | ForEach-Object { "$($_.Name)=$($_.Value)" })
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ EnvironmentVariables = $envList }
Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd; ProcessStartupInformation = $startup }
```

## Sharing the card: `vramorama run`

Agents that each poll "is there 9 GB free?" race each other: two of them see the same free memory
and both start, and the slower one dies with `CUDA error: out of memory` an hour in. `run` makes
the check and the claim one step:

```text
> vramorama run --vram 9G --detach --out train.log -- python train.py --config big.yaml
vramorama: lease 234228b0 waits for 9,216 MiB on NVIDIA GeForce RTX 5080 Laptop GPU: 5,740 MiB available (9,663 in use, 0 reserved by running jobs, 512 headroom)
vramorama: lease 234228b0 admitted after 1h12m; starting python train.py --config big.yaml
lease 234228b0 pid 42620 (started through WMI, outside this session)
```

- **Admission.** A request is admitted when `card total − in use − reserved − headroom ≥ request`
  and no earlier request is still waiting. *In use* is everything on the card, leased or not, so
  jobs that do not use vramorama are still respected. *Reserved* is the part of each admitted
  lease its processes have not allocated yet (a model still loading), which is what stops the next
  job from starting on memory the previous one is about to take. Strict first come, first served:
  a large request at the head of the queue holds back smaller ones, so it cannot be starved.
- **Settle.** The memory must stay available for `--settle` seconds (default 5) before admission,
  to ride out other programs' brief dips. `--headroom` (default 512 MiB) is left for the desktop
  and drivers. `--wait SECS` gives up after a while (exit 2). `--adapter NAME` picks a GPU by name;
  the default is the one with the most memory.
- **The lease tag.** The job runs with `VRAMORAMA_LEASE=<id>` (and `VRAMORAMA_OWNER` with
  `--owner`), inherited by everything it starts, so memory is counted against the lease through
  launcher shims and worker processes. The lease ends when the job and every process carrying the
  tag have exited, or, while waiting, when the `run` process dies. There are no heartbeats to miss.
- **Attached or detached.** Without `--detach`, `run` waits for the job and exits with its exit
  code. With `--detach`, it returns once the job has started and prints the lease and pid. It first
  tries to leave the session's job object (`CREATE_BREAKAWAY_FROM_JOB`). Agent hosts often forbid
  that (Claude Code's shells do), and then it starts the job through WMI, like agents do by hand,
  but passing the whole environment so the session and lease tags survive. Output goes to `--out`.
- **The ledger** is one JSON file per lease in `%LOCALAPPDATA%\vramorama\leases`, changed only
  while holding `ledger.lock`, a file opened without sharing. Windows releases that lock when its
  holder exits, even after a crash. Set `VRAMORAMA_HOME` to use another directory.

## The agent hook

`vramorama hook` reads a Claude Code `PreToolUse` event on stdin. If the `Bash` or `PowerShell`
command launches through WMI (`Invoke-CimMethod`/`Invoke-WmiMethod … Win32_Process … Create`,
`wmic process call create`) or Task Scheduler without passing its environment, it exits 2 and
explains on stderr how to use `vramorama run --detach` or pass the environment. Claude Code blocks
the call and shows the agent that explanation. Everything else passes silently, and a hook that
cannot read its input passes too. Add it to `~/.claude/settings.json` (vramorama on `PATH`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|PowerShell",
        "hooks": [{ "type": "command", "command": "vramorama", "args": ["hook"] }]
      }
    ]
  }
}
```

A line for your agent instructions (`CLAUDE.md`, `AGENTS.md`):

> Start GPU jobs with `vramorama run --vram <estimate> -- <command>` (add `--detach --out <log>` for
> jobs that must outlive the session) instead of polling for free memory yourself. Run `vramorama`
> to see what is on the card and who started it.

## How it works

- **Memory and utilisation:** the `GPU Process Memory(*)\Dedicated Usage`, `GPU Engine(*)\Utilization
  Percentage` and `GPU Adapter Memory(*)\Dedicated Usage` performance counters (PDH), which cover
  every vendor. GPU% is the busiest engine, as in Task Manager.
- **Adapters:** DXGI, to name each adapter and its dedicated memory. By default only adapters with at
  least 1 GiB of dedicated memory are shown, which hides integrated GPUs; `--all` shows them.
- **Processes:** a Toolhelp snapshot for the tree; `NtQueryInformationProcess` and
  `ReadProcessMemory` for the command line, working folder and environment; `GetProcessTimes` to
  rule out recycled pids when walking parents.

## Supply chain

vramorama has **no dependencies**: `[dependencies]` is empty, `Cargo.lock` lists only vramorama,
and there is no `build.rs` and there are no proc macros. `cargo build --locked --offline` works on a
machine that has never contacted a registry. Windows APIs are declared by hand; only `kernel32` is
linked, and `pdh.dll`, `dxgi.dll` and `ntdll.dll` are resolved at run time, so no binding crates or
import libraries are involved. All `unsafe` code is in `src/sys.rs`, `src/gpu.rs` and `src/procs.rs`.
The WMI fallback of `run --detach` calls the Windows PowerShell that ships with Windows. The CI
workflow uses no third-party actions (it checks out with plain `git`).

## Limits

- Windows 10/11 only, except `report` and `hook`, which run anywhere. A Linux backend (NVML) is
  planned.
- `run` schedules only the jobs started through it; it never stops or kills anything. Other GPU
  users are respected (their memory counts as in use) but not queued.
- A job that allocates more than its lease asked for is not stopped; the excess simply counts as in
  use for everyone else.
- Processes running elevated or as another user need an elevated vramorama to read their
  environment; their memory is still shown. 32-bit processes are listed without environment.
- Tags are cooperative, not a security boundary: any process can set or clear them.
- WSL2: processes inside WSL do not appear individually (NVML in WSL lists no processes). How their
  memory shows up on the host is untested; at worst it is in the adapter's "elsewhere" figure.
- Microsoft notes that the GPU memory counters can occasionally misreport; they are what Task
  Manager shows.

## Roadmap

1. A policy for idle holders (for example a `llama-server` at 0% whose session ended), opt-in.
2. A WSL client on the same ledger, so jobs inside WSL queue with Windows ones.
3. An MCP server for agents, and the Linux/NVML backend.

## License

MIT
