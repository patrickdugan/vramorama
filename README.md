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

A line for your agent instructions (`CLAUDE.md`, `AGENTS.md`):

> Before starting GPU work run `vramorama` and leave room for what is there. Set
> `VRAMORAMA_OWNER` to a short job name. If you detach a job through WMI, pass the full environment
> (see the vramorama README).

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
The CI workflow uses no third-party actions (it checks out with plain `git`).

## Limits (v0)

- Windows 10/11 only for `ps`, `watch` and `trace`; `report` runs anywhere. A Linux backend (NVML)
  is planned.
- Read-only: it reports, it does not schedule or kill anything yet.
- Processes running elevated or as another user need an elevated vramorama to read their
  environment; their memory is still shown. 32-bit processes are listed without environment.
- Tags are cooperative, not a security boundary: any process can set or clear them.
- WSL2: processes inside WSL do not appear individually (NVML in WSL lists no processes). How their
  memory shows up on the host is untested; at worst it is in the adapter's "elsewhere" figure.
- Microsoft notes that the GPU memory counters can occasionally misreport; they are what Task
  Manager shows.

## Roadmap

1. `vramorama run --vram 9G -- <cmd>`: a shared ledger of VRAM leases with first-come admission (so
   two polite waiters cannot both start on the same free memory), proper detaching that keeps tags,
   and heartbeats. A Claude Code hook that routes WMI detaches through it.
2. A policy for idle holders (for example a `llama-server` at 0% whose session ended), opt-in.
3. A WSL client on the same ledger, an MCP server for agents, and the Linux/NVML backend.

## License

MIT
