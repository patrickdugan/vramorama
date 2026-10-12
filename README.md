# vramorama

**Who is holding my GPU, and my RAM?** Per-process VRAM and RAM on a single workstation, with the
agent session that launched each process: Claude Code, Claude Cowork, the ChatGPT desktop app's
Codex agent, or the Codex CLI.

When several coding agents (and you) share one GPU, the card fills up overnight and nobody can say
what is on it. On Windows `nvidia-smi` cannot even tell you how much memory each process holds:
under the WDDM driver model NVML reports it as `N/A`. vramorama reads the numbers Windows keeps for
Task Manager, then works out *who* owns each process, and how much each agent session holds, on the
card and in RAM, counting every Python job, server and shell the session started:

```text
> vramorama sessions
NVIDIA GeForce RTX 5080 Laptop GPU: 15,802 of 15,915 MiB in use
RAM: 30,911 of 31,864 MiB in use (953 available); 86,439 of 112,080 MiB committed

SESSION                    APP              STATE   VRAM MiB  SHARE   RAM MiB   AGENT PROCS  TITLE / JOBS
claude a91d07c4            Claude Code      live       9,479    60%     7,024      71     6  “Nightly eval queue” · lr-sweep #234228b0
chatgpt 3c4d5e6f           ChatGPT (Codex)  live       6,317    40%     3,232       –     4  “Fine-tune the reranker”
claude 5e6f7a8b            Claude Code      live           –     0%       904     136     7  “Data cleaning”
WSL 2 virtual machine      –                –              –     0%     4,256       –     1
  and 5 more holding no GPU memory and under 256 MiB of RAM each (710 MiB in all; --min-ram 0 lists them)

RAM is each process's private working set (Task Manager's Memory column); AGENT is the agent's own process.

  Claude Code              VRAM   9,479 MiB in 1 · RAM   7,928 MiB in jobs + 207 agent · 7 session(s)
  Claude Cowork            VRAM       0 MiB in 0 · RAM       0 MiB in jobs + 0 agent · 0 session(s) (running; no session holds GPU memory or RAM)
                           Cowork sessions run in a virtual machine with no GPU access, so they hold no VRAM here.
  ChatGPT (Codex)          VRAM   6,317 MiB in 1 · RAM   3,232 MiB in jobs + 0 agent · 1 session(s)
  Outside agent sessions   VRAM       0 MiB in 0 · RAM   4,256 MiB in jobs + 0 agent · 1 group(s)
```

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
  tagged    no (no VRAMORAMA_OWNER / CLAUDE_CODE_SESSION_ID / CODEX_THREAD_ID / AI_AGENT in its environment)
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

`cargo install` puts it in `%USERPROFILE%\.cargo\bin`. Or clone, `cargo build --release --locked --offline`,
and copy `target\release\vramorama.exe` to a folder on your `PATH` (for example
`%LOCALAPPDATA%\Programs\vramorama`). Both Windows Rust toolchains work: MSVC
(with its usual Build Tools linker) and GNU (self-contained, no Visual Studio needed). Nothing else
is required: no SDK headers, no import libraries beyond what Rust itself links.

## Commands

| Command | What it does |
|---|---|
| `vramorama gui` | A live page in your browser: memory by agent session and by app, an hour of history, every process with its session, job and idle state, watermarks, leases, and one-click trace. See [The GUI](#the-gui). |
| `vramorama` / `vramorama ps` | Processes holding GPU memory now, with owner, start time, working folder and command. `--json`, `--all` (include integrated GPUs), `--min MIB` (default 16), `--wide`, `--trace` (trace every untagged process). |
| `vramorama sessions` | GPU memory and RAM per agent session (its app, title, whether it is still running, its share of the card, its jobs' RAM and its agent's own, its processes and labelled jobs), and per agent app. Lists sessions holding GPU memory or at least `--min-ram` MiB of RAM (default 256). `--json` for agents and scripts. |
| `vramorama ram` | Processes of agent sessions by RAM, with commit, VRAM, start time, session and command; the agents' own processes; and untagged processes of 1 GiB or more. `--min MIB` (default 64 here), `--json`. See [RAM](#ram). |
| `vramorama watch` | Refreshes every `--interval` seconds (default 5) and keeps a **running watermark**: each process's peak and how long it has been idle, and each owner's current and peak total. `--log FILE` appends one JSON line per sample; `--quiet` only logs. |
| `vramorama report FILE` | Summarises a watch log: GiB-hours (and how many of them were *idle*, held at under 1% GPU), peak, GPU% and time held per owner, or with `--by session` per agent session. Answers "what sat on the card all night?" Gaps (sleep, a stopped watcher) are capped so they do not count as use. `--json`. |
| `vramorama trace PID` | For an untagged process: its full launch chain, and the agent transcript that typed the command. |
| `vramorama run --vram 9G -- CMD…` | Waits for a VRAM lease (first come, first served), then runs `CMD` tagged with it. `--detach [--out LOG]` starts it outside the session. See [Sharing the card](#sharing-the-card-vramorama-run). |
| `vramorama leases` | The lease ledger, how much a new request could get right now, memory held by processes outside any lease, and any spill. `--json`. |
| `vramorama hook` | A Claude Code `PreToolUse` hook that stops WMI / Task Scheduler launches that would strip a job's tags. See [The agent hook](#the-agent-hook). |
| `vramorama idle` | Processes that held memory but did no GPU work for `--window` seconds (default 20), each judged *stale* or *idle*. See [Idle holders](#idle-holders). |
| `vramorama reclaim [PID…]` | Re-checks idle holders (all stale ones if no PID is given) and prints the command that would free them. It never stops anything itself. |

Leave a watcher running overnight:

```powershell
Start-Process vramorama -ArgumentList "watch","--interval","30","--log","$HOME\vram.jsonl","--quiet" -WindowStyle Hidden
# next morning
vramorama report $HOME\vram.jsonl
```

## The GUI

```powershell
vramorama gui            # opens http://127.0.0.1:7787/#t=<token> in your browser
vramorama gui --port 0 --no-open   # any free port; just print the link
```

The page shows, for each GPU, memory in use as a bar split by agent session (titled as the
session's app titles it), what a new lease could get, and how much is held idle or stale; then
**System memory**, RAM in use as a bar split by the same sessions, with what is available, what
the agents' jobs and the agents themselves hold, and the commit charge, and a warning when RAM or
commit runs short. Below that, **Agent sessions**: a tile per agent app on this machine (Claude Code, Claude Cowork, the
ChatGPT app, the Codex CLI, and everything outside agent sessions) with what its sessions hold,
and a row per session with its app, whether it is still running, its memory and share of the card,
peak, GiB-hours, idle GiB-hours, RAM and RAM peak, and the labelled jobs inside it. Click a session
to filter the process tables to it. Then a timeline of each session's GPU memory, or RAM, over the
last 5 minutes to an hour; every GPU process with its session, job, memory and peak, GPU %, RAM,
idle time and verdict, start time and command; every process holding RAM (see [RAM](#ram));
per-session watermarks; and the lease ledger, with the session that asked for each lease.
**By job** switches the bar, timeline and watermarks to group by job label instead.
**Trace** on an untagged process runs `vramorama trace` and shows the launch chain and the session
that launched it. A stale process gets **Copy stop command**; the page never stops anything itself.

It is served by vramorama itself, with the standard library's TCP listener, and the page is
embedded in the binary: no web framework, no scripts or fonts from the network. Access:

- It listens on `127.0.0.1` only and rejects requests whose `Host` is not that address
  (DNS rebinding).
- The data endpoints need a random 128-bit token (from the system RNG), printed in the link after
  `#t=`. Browsers never send the URL fragment to a server, and the page sends the token in a
  header, which other web pages cannot add to a request without the CORS permission vramorama
  never grants.
- Read-only: the API can read state and run `trace`; nothing changes the ledger or touches a
  process. A content security policy blocks the page from loading or sending anything elsewhere.

## How ownership works

Environment variables are inherited by every child process, so tags set where a job starts follow
it down the process tree with no wrapper and no cooperation from the job. vramorama reads the
environment of each GPU process (from its PEB, as Process Explorer and psutil do) and looks for:

| Variable | Set by | Shown as |
|---|---|---|
| `VRAMORAMA_OWNER` | you, a script, or an agent's instructions | the job's label; the memory still counts against the session that started it |
| `CLAUDE_CODE_SESSION_ID` | Claude Code, in every shell it starts | `claude <first 8>`, `(live)`/`(ended)` from `CLAUDE_PID`, and the session's title |
| `CLAUDE_CODE_SESSION_ID` with `CLAUDE_CODE_ENTRYPOINT=local-agent` | Claude Cowork | `cowork <first 8>`, as above |
| `CODEX_THREAD_ID` | Codex: the agent in the ChatGPT desktop app, and the Codex CLI, on every command it runs | `chatgpt <last 8>` or `codex <last 8>`, the thread's name, and `(live)`/`(ended)` from the `codex.exe` that ran it |
| `AI_AGENT` | some agent harnesses | the value |

Tag anything by hand:

```powershell
$env:VRAMORAMA_OWNER = "sweep-lr-1e-4"; python train.py
```

### Agent apps

- **Claude Code** (the CLI, the desktop app's Code tab, IDE extensions). The title is the one the
  desktop app's sidebar shows (its session records in `claude-code-sessions`), else the one in the
  transcript (`/rename`, or the automatic title).
- **Claude Cowork.** Cowork normally runs its sessions inside a Hyper-V virtual machine, which has
  no GPU, so those sessions hold no VRAM on the host and have nothing to attribute; vramorama
  lists Cowork as running (its `cowork-svc.exe` service) with 0 MiB. Cowork sessions that run on
  the host are attributed like Claude Code sessions, titled from `local-agent-mode-sessions`,
  with their transcript under the session's `CLAUDE_CONFIG_DIR`.
- **The ChatGPT desktop app** runs Codex: `ChatGPT.exe` starts `codex.exe app-server`, which runs
  each command, by default inside its Windows sandbox as the `CodexSandboxOffline` or
  `CodexSandboxOnline` user. The sandboxed command still carries the environment, including
  `CODEX_THREAD_ID`, and vramorama reads it without elevation. A thread is *live* while that
  `codex.exe` runs; closing the app ends it. Thread names come from `~/.codex/session_index.jsonl`.
  Codex thread ids are UUIDv7, whose first digits are a timestamp shared by threads started in the
  same minute, so vramorama shortens them to their last 8 digits.
- **The Codex CLI**: the same, without the app.

When one agent runs another (a Claude session that runs `codex exec`), the commands carry both
tags, and vramorama attributes them to the agent nearer in the process tree.

Both desktop apps are MSIX packages, and Windows redirects files a packaged app creates under
`AppData` into its package folder (`%LOCALAPPDATA%\Packages\<package>\LocalCache`). vramorama looks
for the Claude app's session records in both places, so titles show up whichever app or terminal
vramorama runs from.

### Jobs that lose their tags

Agents often detach long jobs so they outlive the session, and the usual trick on Windows is
`Invoke-CimMethod Win32_Process -MethodName Create`. That starts the job from the WMI service with a
**fresh** environment, so every tag is gone, and the launcher's parent exits soon after.
vramorama shows these as `untagged, via WmiPrvSE.exe …` (while the WMI host lives) or
`untagged, from <process> (parent exited)`, and `trace` recovers the owner by searching agent
transcripts (`~/.claude/projects`, `~/.codex/sessions`, Cowork's `local-agent-mode-sessions`) for
the job's command line, and its ancestors' command lines. When an agent built the command from variables, the full line never
matches, so `trace` also searches for the script or module being run (`-File queue.ps1`,
`-m package.module`, `train.py`). It reads only transcripts modified after the job started. A
session is reported as `launched` only if the text appears in one of its tool calls timestamped
within 30 minutes *before* the process started; launches are ranked by how close the call was.
Sessions that only saw the command in some tool's output, typed it later, or typed it long before
are listed as `mentioned`.

To keep tags across a WMI launch, pass your whole environment. `EnvironmentVariables` replaces the
environment rather than adding to it, so passing only the tags leaves the job without `PATH`:

```powershell
$envList = [string[]](Get-ChildItem env: | ForEach-Object { "$($_.Name)=$($_.Value)" })
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ EnvironmentVariables = $envList }
Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $cmd; ProcessStartupInformation = $startup }
```

## RAM

GPU jobs are rarely only GPU jobs: data loaders, tokenizers, evaluation servers and the model's
host-side copies live in system memory, and an agent session runs Python, Node, browsers and shells
of its own. When several sessions share a machine, RAM runs out as often as VRAM does, and on
Windows that does not fail either: the memory manager pages processes to disk and everything,
GPU jobs included, slows down.

`vramorama ram`, `vramorama sessions` and the GUI count, for each agent session:

- **Its jobs**: every process carrying the session's tags (the same inherited variables as for GPU
  memory, so a `vramorama run` job, its workers, an MCP server and a test runner all count).
- **The agent itself**: Claude Code's own `claude.exe`. It does not carry its session id, but the
  shells it starts name it in `CLAUDE_PID`, and the desktop app's session record names it too.
  It is counted apart from the jobs. The Codex app server is shared by every thread, so it is not
  split between them.
- **Big untagged processes**: anything of 1 GiB or more outside every session, so a job detached
  through WMI or the WSL 2 virtual machine (`vmmemWSL`, which holds the memory of everything
  running inside WSL) does not hide.

RAM is the **private working set**, what Task Manager's Memory column shows: the process's own
pages in RAM, not counting pages shared with other processes (mapped model files, DLLs). **Commit**
is private memory reserved in RAM or the page file; when the system's commit charge reaches its
limit, allocations fail. The GUI warns when less than 5% of RAM is available or commit is above
90% of its limit.

A Claude session can outlive the `claude.exe` its jobs were started from (resuming it, or the
desktop app restarting, starts a new one); its jobs count as live, and are never called stale,
while another Claude Code process runs the session.

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
- **Settle.** The memory must stay available for `--settle` seconds (default 15) before admission,
  and the committed total (in use plus reserved) must not grow by more than 256 MiB meanwhile; if
  it does, the settle period starts again. Leased jobs filling their own reservations do not move
  the committed total. A job started *without* `vramorama run` that is still loading its model
  does, and admitting a lease on top of it is how two jobs end up on memory only one of them can
  have. `--headroom` (default 512 MiB) is left for the desktop and drivers. `--wait SECS` gives up
  after a while (exit 2). `--adapter NAME` picks a GPU by name; the default is the one with the
  most memory.
- **Reservations lapse.** A lease's unallocated remainder counts as reserved for 10 minutes after
  admission, long enough to start and load a model. After that the lease is charged only for what
  its processes hold (`leases` shows it as `holding`), so a queue runner that leased memory and
  sat empty, or a job that asked for more than it uses, does not keep memory from everyone else.
- **Spill stops admission.** While any process on the card has spilled into system memory (see
  [Spill](#spill)), nothing is admitted.
- **The lease tag.** The job runs with `VRAMORAMA_LEASE=<id>` (and `VRAMORAMA_OWNER` with
  `--owner`), inherited by everything it starts, so memory is counted against the lease through
  launcher shims and worker processes. The lease ends when the job and every process carrying the
  tag have exited, or, while waiting, when the `run` process dies. There are no heartbeats to miss.
- **Attached or detached.** Without `--detach`, `run` waits for the job and exits with its exit
  code. With `--detach`, it returns once the job has started and prints the lease and pid. It first
  tries to leave the session's job object (`CREATE_BREAKAWAY_FROM_JOB`). Agent hosts often forbid
  that (Claude Code's shells do), and then it starts the job through WMI, like agents do by hand,
  but passing the whole environment so the session and lease tags survive. Output goes to `--out`.
- **The ledger** is one JSON file per lease in `~\.vramorama\leases` (`%USERPROFILE%`), changed
  only while holding `ledger.lock`, a file opened without sharing. Windows releases that lock when
  its holder exits, even after a crash. Set `VRAMORAMA_HOME` to use another directory. Each lease
  records the agent session that asked for it, so `leases` and the GUI show whose job is queued.
- **One queue for every app.** Up to 0.5 the ledger was in `%LOCALAPPDATA%\vramorama`. Inside the
  Claude and ChatGPT desktop apps (MSIX packages) Windows redirects that folder into each app's
  package folder, so each app's agents queued in a ledger the others could not see, and a terminal
  had a third. The profile folder is not redirected (it is why `~/.claude` and `~/.codex` work for
  everyone). Leases still in the old ledger are read, never written: new requests queue behind
  its waiting leases and count its reservations, and `leases` marks them with `*`. Running
  processes whose lease tag is in no ledger vramorama can see are reported, since that is what a
  split queue looks like.

## Spill

On Windows a discrete card that is full does not fail the next allocation: the driver places it in
shared (system) memory instead, behind PCIe. Nothing errors and nothing is logged; every kernel that
touches that memory runs many times slower. Jobs that would have died with `CUDA error: out of
memory` crawl for hours instead. In one jam a 9 GB fine-tuning job that normally finishes in 75
minutes made no visible progress in 8 hours, and the jobs sharing the card slowed down with it.

vramorama reads the `GPU Process Memory\Shared Usage` counter alongside dedicated memory. A CUDA
context alone holds about 80 MiB there and pinned host buffers add some more, so a process counts as
**spilled** only when it holds at least 512 MiB of shared memory *and* the card's dedicated memory is
within 1 GiB of full. `ps`, `watch`, `leases` and the GUI then print a warning naming the spilled
processes, the JSON outputs list them under `spilled`, and `run` admits nothing until the spill clears.
vramorama still stops nothing itself: pausing a job is your call.

`leases` also says how much memory is held by processes not started through `vramorama run`. Those
are respected but cannot be queued; when an agent starts GPU jobs directly, that line is where it
shows.

## The agent hook

`vramorama hook` reads a Claude Code `PreToolUse` event on stdin. If the `Bash` or `PowerShell`
command launches through WMI (`Invoke-CimMethod`/`Invoke-WmiMethod … Win32_Process … Create`,
`wmic process call create`) or Task Scheduler without passing its environment, it exits 2 and
explains on stderr how to use `vramorama run --detach` or pass the environment. Claude Code blocks
the call and shows the agent that explanation. Everything else passes silently, and a hook that
cannot read its input passes too. The explanation names vramorama by its absolute path, so agents in sessions that started before it was on `PATH` can still follow it. Add it to `~/.claude/settings.json`, using the absolute path to the exe so the hook does not depend on `PATH` either:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|PowerShell",
        "hooks": [{ "type": "command", "command": "C:/Users/you/AppData/Local/Programs/vramorama/vramorama.exe", "args": ["hook"], "timeout": 10 }]
      }
    ]
  }
}
```

A line for your agent instructions (`CLAUDE.md`, `AGENTS.md`):

> Start GPU jobs with `vramorama run --vram <estimate> -- <command>` (add `--detach --out <log>` for
> jobs that must outlive the session) instead of polling for free memory yourself. Run `vramorama`
> to see what is on the card and who started it, and `vramorama sessions` for how much each agent
> session holds.

## Idle holders

A process is *idle* when its busiest GPU engine stays under 1% for the whole window. `idle` then
judges each one:

- **stale**: it carries an agent session tag, and that session has ended (the Claude Code
  process named by `CLAUDE_PID` is gone, or for Codex the `codex.exe` that ran it). This is the
  only verdict that needs positive evidence.
- **idle**: everything else. Something may still want it: a live session, a `VRAMORAMA_OWNER`
  label, a lease, or no tag at all. Untagged processes are never called stale. Their launch chain
  being cut proves nothing: every browser GPU helper started by a short-lived launcher looks
  exactly like that. Run `trace` on them instead.

`reclaim` repeats the checks for the PIDs you name, or for every stale holder of at least 256 MiB,
and refuses busy, system and possibly-wanted processes (`--force` includes the last group with a
warning). For the rest it prints a `Stop-Process -Id …` line with each process's start time, so you
can confirm the pid has not been reused. vramorama does not stop processes itself; that decision
stays with whoever runs the command.

## How it works

- **Memory and utilisation:** the `GPU Process Memory(*)\Dedicated Usage`, `GPU Engine(*)\Utilization
  Percentage` and `GPU Adapter Memory(*)\Dedicated Usage` performance counters (PDH), which cover
  every vendor. GPU% is the busiest engine, as in Task Manager. The `Shared Usage` counters of the
  same two sets show spill; without them vramorama works as before, minus the spill check.
- **Adapters:** DXGI, to name each adapter and its dedicated memory. By default only adapters with at
  least 1 GiB of dedicated memory are shown, which hides integrated GPUs; `--all` shows them.
- **RAM:** one `NtQuerySystemInformation(SystemProcessInformation)` call returns every process's
  private working set, working set, commit and start time without opening any process;
  `GlobalMemoryStatusEx` gives installed RAM, available RAM and the commit charge. To attribute
  RAM, vramorama reads the environment of every process it can open, once per process (cached by
  pid and start time), and keeps only the tags.
- **Processes:** a Toolhelp snapshot for the tree; `NtQueryInformationProcess` and
  `ReadProcessMemory` for the command line, working folder and environment; `GetProcessTimes` to
  rule out recycled pids when walking parents.

## Supply chain

vramorama has **no dependencies**: `[dependencies]` is empty, `Cargo.lock` lists only vramorama,
and there is no `build.rs` and there are no proc macros. `cargo build --locked --offline` works on a
machine that has never contacted a registry. Windows APIs are declared by hand; only `kernel32` is
linked, and `pdh.dll`, `dxgi.dll` and `ntdll.dll` are resolved at run time, so no binding crates or
import libraries are involved. All `unsafe` code is in `src/sys.rs`, `src/gpu.rs`, `src/procs.rs` and the two Windows calls in `src/gui.rs` (the system RNG and opening the browser).
The WMI fallback of `run --detach` calls the Windows PowerShell that ships with Windows. The CI
workflow uses no third-party actions (it checks out with plain `git`).

## Limits

- Windows 10/11 only, except `report` and `hook`, which run anywhere. The GUI needs a browser on
  the same machine. A Linux backend (NVML) is
  planned.
- `run` schedules only the jobs started through it, and nothing in vramorama stops or kills a
  process. Other GPU users are respected (their memory counts as in use) but not queued. The settle
  growth check protects new leases from a job that is still loading, but a job started outside
  `run` can still start on memory a lease has reserved and push the card into spill.
- The spill rule is a threshold, not a measurement of where each allocation lives: a process with
  more than 512 MiB of pinned host buffers on a nearly full card is reported as spilled too.
- A server that waits idle between requests (an inference server between evaluation batches) is
  *idle* by these rules. The verdict only says nobody appears to own it, not that nobody uses it.
- A job that allocates more than its lease asked for is not stopped; the excess simply counts as in
  use for everyone else.
- Processes running elevated or as another user need an elevated vramorama to read their
  environment; their memory is still shown. 32-bit processes are listed without environment.
- Tags are cooperative, not a security boundary: any process can set or clear them.
- WSL2: processes inside WSL do not appear individually (NVML in WSL lists no processes). How their
  GPU memory shows up on the host is untested; at worst it is in the adapter's "elsewhere" figure.
  Their RAM is all in the `vmmemWSL` process, shown as the WSL 2 virtual machine, and cannot be
  split by session.
- Microsoft notes that the GPU memory counters can occasionally misreport; they are what Task
  Manager shows.

## Roadmap

1. A WSL client on the same ledger, so jobs inside WSL queue with Windows ones.
2. Server awareness for `idle`: open client connections on a listening port count as use.
3. An MCP server for agents, and the Linux/NVML backend.

## License

MIT
