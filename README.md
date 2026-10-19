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
| `vramorama thrash` | Watches the card for `--window` seconds (default 30) and says whether processes are taking turns on it, swapping their memory in and out over PCIe. See [Thrash](#thrash). `--json`. |
| `vramorama watch` | Refreshes every `--interval` seconds (default 5) and keeps a **running watermark**: each process's peak and how long it has been idle, and each owner's current and peak total. `--log FILE` appends one JSON line per sample; `--quiet` only logs. |
| `vramorama report FILE` | Summarises a watch log: GiB-hours (and how many of them were *idle*, held at under 1% GPU), peak, GPU% and time held per owner, or with `--by session` per agent session. Answers "what sat on the card all night?" Gaps (sleep, a stopped watcher) are capped so they do not count as use. `--json`. |
| `vramorama trace PID` | For an untagged process: its full launch chain, and the agent transcript that typed the command. |
| `vramorama run --vram 9G -- CMD…` | Waits for a VRAM lease (first come, first served, with backfill), then runs `CMD` tagged with it. `--detach [--out LOG]` starts it outside the session; `--strict-fifo` turns backfill off; `--restart N`, `--guard-power SECS` and `--enforce-vram MIB` supervise the job (see [Supervised runs](#supervised-runs)). See [Sharing the card](#sharing-the-card-vramorama-run). | Holds the machine awake while the job runs (`--allow-display-off`, `--no-keep-awake`) and waits while commit charge is at `--max-commit` percent of its limit. See [Crashes and risks](#crashes-and-risks).
| `vramorama power` | Board power, the enforced power limit and the driver's default limit for each NVIDIA card, whether it is power-capped, and the GPU resets Windows logged in the last 24 h. `--json`. See [Power cap](#power-cap). |
| `vramorama crashes` | Why the machine rebooted: each unexpected reboot in the event log (default last 30 days) with its decoded bugcheck, the driver module blamed, whether it was in Modern Standby, GPU resets before it, what a `watch --log` file shows for the minutes before, and a verdict. Read-only. `--days N`, `--last N`, `--log FILE`, `--json`. See [Crashes and risks](#crashes-and-risks). |
| `vramorama doctor` | Read-only risk checks with advice: screen-off timeout on a Modern Standby PC, display-driver age, commit charge and free RAM, spill into shared GPU memory, power cap, recent GPU resets. Changes no setting. `--json`. |
| `vramorama leases` | The lease ledger, how much a new request could get right now, memory held by processes outside any lease, and any spill. `--json`. |
| `vramorama hook` | A Claude Code `PreToolUse` hook that stops WMI / Task Scheduler launches that would strip a job's tags. See [The agent hook](#the-agent-hook). |
| `vramorama idle` | Processes that held memory but did no GPU work for `--window` seconds (default 20), each judged *stale* or *idle*. See [Idle holders](#idle-holders). |
| `vramorama reclaim [PID…]` | Re-checks idle holders (all stale ones if no PID is given) and prints the command that would free them. With `--ram` it frees RAM instead and stops supervised jobs from the top (see [Supervisors](#supervisors)). It never stops anything itself. |

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
per-session watermarks; the lease ledger, with the session that asked for each lease; and a **Crashes and risks** card with the `doctor` findings and the latest reboots with their verdicts (read from the event log every two minutes).
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

### Supervisors

Killing a 9 GiB model server in Task Manager does nothing for long when the script that launched
it starts it again. The RAM comes back, and so does the process. What frees it is stopping the
*tree*, from the topmost process that belongs to the job.

`ram` follows each process of 256 MiB or more up through its parents. A parent that is a real
program, and not a launcher, is a **supervisor**: it can start the job again. Shells, `cmd /c`
wrappers and `vramorama run` are launchers, and so is a venv's `python.exe` shim (same command
line as the process it starts). `ram` prints one line per supervised job with the supervisors, the
size of the whole tree, and the command that stops it:

```text
Supervised: stopping these alone frees nothing for long, because a process above them starts them again.
  34564 9,017 MiB, restarted by python.exe 18852; stop the whole job (6 processes, 9,197 MiB): taskkill /PID 23168 /T /F
```

- The walk stops below the system (`services`, `explorer`, `wmiprvse`, the Hyper-V and WSL hosts),
  terminals and interactive shells, and the agent apps themselves, so a stop command never
  includes them. It also stops below vramorama's own ancestors, so the command never kills the
  shell that ran it. A parent that started after its child is a recycled pid and ends the walk.
  System and protected processes (WSL's VM, memory compression) are not shown as supervised.
- `ram --json` adds `supervised`, `supervisors`, `stop_root`, `stop_command` and `tree_ram_mib`.
- `reclaim --ram [PID…]` prints the tree-stop command for the PIDs you name (`--force` for a job
  whose session is still running), or for every job of an ended session holding 1 GiB or more. The
  GPU `reclaim` does the same for a supervised holder instead of printing a `Stop-Process` that
  would be undone.
- `watch` notices respawns as they happen: the same command line started again under the same
  live parent prints a `WARNING` with the number of restarts and the command that stops the job.
  A one-off `ram` cannot see history, so it reports who *would* restart a process, and `watch` who
  *did*.

`ram`, `reclaim` and `watch` never stop anything; they print the command. The only process vramorama
ever stops is a job that `vramorama run` started itself, and only when you asked for it with
`--guard-power` or `--enforce-vram` (see [run](#sharing-the-card-vramorama-run)).

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
  job from starting on memory the previous one is about to take. First come, first served, with
  one exception (backfill, next point).
- **Backfill.** A large request at the head of the queue used to hold back every smaller one, so a
  12 GiB request that could not start for six hours left 5 GiB of a 16 GiB card idle the whole time. Now a
  smaller request may go ahead of earlier waiting ones that cannot start yet, if it fits now *and*
  each earlier request would still fit on an otherwise empty card (total less headroom) with it
  running. It can then delay the head only for as long as it runs, never push it out of reach.
  An earlier request that does fit now is still served first. `run` says when it jumped the queue.
  `--strict-fifo` restores the old rule.
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
- **Spill, thrash and a power cap stop admission.** While any process on the card has spilled into
  system memory (see [Spill](#spill)), processes are taking turns on it (see [Thrash](#thrash)), or
  an NVIDIA card is power-capped (see [Power cap](#power-cap)), nothing is admitted. Nor is anything
  while the system commit charge is at or above `--max-commit` percent of its limit (default 92).
- **The lease tag.** The job runs with `VRAMORAMA_LEASE=<id>` (and `VRAMORAMA_OWNER` with
  `--owner`), inherited by everything it starts, so memory is counted against the lease through
  launcher shims and worker processes. The lease ends when the job and every process carrying the
  tag have exited, or, while waiting, when the `run` process dies. There are no heartbeats to miss.
- **Awake while it runs.** Attached, `run` holds the machine out of sleep and Modern Standby, and the
  screen on, from admission to the end of the job (see [Crashes and risks](#staying-out-of-modern-standby-while-a-job-runs)).
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

## Supervised runs

Long fine-tuning runs on a consumer GPU die for reasons that have nothing to do with the job: the card
gets power-capped (see [Power cap](#power-cap)), a long kernel passes Windows' two-second limit and the
driver resets the GPU (a TDR, event 153), or a neighbour's job grows past what it asked for. Three
opt-in flags on `run` turn the lease into a small supervisor. The job stays attached to `run`, and
vramorama only ever stops a job that `run` itself started:

```powershell
vramorama run --vram 11G --restart 5 --guard-power 30 --enforce-vram 512 -- python train.py --auto-resume
```

- `--restart N` starts the job again, up to N times, after **a GPU reset** (a `nvlddmkm` 153,
  `amdkmdag` 4101 or `Display` 4101 event logged while the job ran, read with `wevtutil`) or a power-guard
  stop. It waits 60 s, then until the card is no longer power-capped (an hour at most), and keeps the
  lease meanwhile. The job must resume from its own checkpoint (`--auto-resume` here). An ordinary
  failure, with no reset logged, is **not** retried, so a bug cannot loop; neither is a job stopped for
  exceeding its lease.
- `--guard-power SECS` stops the job (and its process tree) after the card has been power-capped for SECS
  seconds. A capped card would run it twenty times slower and the next long kernel would reset the GPU
  for everyone. With `--restart` it starts again when the cap clears.
- `--enforce-vram MIB` stops the job when its processes hold more than the lease plus MIB. Without it a
  job that allocates more than it asked for is not touched.

A guard stop returns exit code 3. Supervision needs `run` to stay alive, so it cannot be combined with
`--detach`; start `run` itself in the background instead. The decisions are pure functions with tests
(`guard.rs`); `scripts/smoke.sh` checks the whole path on a real GPU.

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

## Power cap

A laptop GPU can be held to a fraction of its rated power. On an RTX 5080 Laptop (2026-10-15) the
enforced limit dropped to 15 W against a default of 80 W, several times after the machine woke from
Modern Standby, and stayed there until it was woken again. The card then draws about 19 W at 100% use and
runs roughly twenty times slower. Worse, one long kernel can pass the two seconds Windows allows a GPU
operation before it resets the card (a TDR, `nvlddmkm` event 153), and a reset kills the running jobs of
every session on the card, not only the one that tipped it over. Three training runs died that way.

vramorama reads board power, the enforced limit and the default limit from NVML (`nvml.dll`, shipped
with the driver, loaded at run time like the other Windows libraries). A card counts as **capped** when
the enforced limit is at most half the default. Clock readings are no help: under WDDM `nvidia-smi`
shows about 200 MHz even while the card draws 100 W.

```text
> vramorama power
NVIDIA GeForce RTX 5080 Laptop GPU: drawing 17 W at 100% use; enforced limit 15 W, default limit 80 W: CAPPED
```

`ps` and `leases` print a warning, `leases --json` has a `power` object, and `run` admits nothing
while a card is capped. A job that is already running is not touched: vramorama stops nothing itself.

## Thrash

Spill is one way a full card fails silently; taking turns is the other. When the jobs on a card
together need more than it has, the WDDM video memory manager can let each run in turn: before one
runs, the other's allocations are evicted to system memory, and they come back when it runs
again. Each job's dedicated memory then swings by gigabytes every few seconds, opposite to the
other's, while the card stays full, and the evicted memory shows up in their RAM. Every turn moves
gigabytes over PCIe, so both jobs run far slower than either would alone, and nothing errors. In
one case two training jobs swung between about 4 and 12 GB every few seconds on a 16 GB card for
hours, while the RAM they were evicted into ran out.

vramorama calls it **thrashing** when, over a window of samples, the card is within 1 GiB of full
for at least half of them and at least two processes each swing by 1 GiB or more, change direction
at least three times, and move opposite to each other (their changes correlate at −0.5 or less). A
job loading its model only grows, and jobs that allocate and free memory every step do not move
opposite to anyone; neither counts. The RAM of each process is checked too: when it rises as the
process's VRAM falls, that is where the evicted memory went.

- `vramorama thrash [--window SECS]` watches for 30 seconds (sampling every second) and names the
  processes, their swing, how often they took turns, and their sessions.
- `watch` and the GUI keep a minute of samples and warn as soon as it happens; the GUI marks the
  processes **Thrashing**.
- `run` admits nothing while the card is thrashing, as with spill.

Replayed against the hour of samples a GUI had recorded during that case, the rule flagged the
whole episode as one stretch, from the first full window until the jobs left the card, and nothing
in the remaining 45 minutes, which included periods with the card full but steady.

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
can confirm the pid has not been reused. `reclaim` does not stop processes itself; that decision
stays with whoever runs the command. (vramorama stops a process only when it is a job `run`
started and you passed `--guard-power` or `--enforce-vram`.)

## Crashes and risks

On a laptop that runs long GPU jobs, "the machine rebooted" has several unrelated causes, and VRAM
contention between jobs is only one of them. Two commands, both read-only, tell them apart. They read
what Windows already recorded; nothing is installed, and no setting, driver or process is touched.

### `vramorama crashes`

Reads the System and Application event logs (through `wevtapi.dll`, loaded at run time like the other
Windows libraries) and builds one incident per unexpected reboot:

- **When it stopped.** Kernel-Power 41 is only written at the next boot. The time the machine was last
  alive is in the payload of EventLog 6008, so that is the crash time.
- **What the bugcheck was.** The code and parameters from Kernel-Power 41 and WER 1001 decoded by name
  (`0x10E VIDEO_MEMORY_MANAGEMENT_INTERNAL`, `0x116 VIDEO_TDR_FAILURE`, `0x117`, `0x119`, `0x3B`, `0x1A`,
  `0x50`, `0x124`, `0x9F`, `0xEF`, and others), the exception behind a `0x3B`, the minidump path, and
  **the module and function** the Windows Error Reporting fault bucket names
  (`AV_nvlddmkm!UvmChannelUpdateProgress`). No bugcheck at all means a hang or power loss.
- **Whether the machine was in Modern Standby.** Kernel-Power 41 records `ConnectedStandbyInProgress`,
  and Kernel-Power 506/507 say since when. A standby entered and never exited before the crash is
  reported as a crash *while resuming*.
- **GPU resets before it.** `nvlddmkm` 153, `Display`/`amdkmdag` 4101, and the `LiveKernelEvent` reports
  (0x117 and 0x141, GPU hangs Windows recovered from) in the ten minutes before. A reset logs three events
  within a second or two; they count once.
- **What held the GPU and RAM.** If a `watch --log` file covers the time (`--log FILE`, or any `.jsonl`
  in `~\.vramorama`), the incident shows the card, RAM and commit charge in the five minutes before, and
  which sessions held how much. `watch` logs write a `system` object (RAM and commit) from 0.11; older logs
  still give the card and the holders.

```text
> vramorama crashes --last 1
Crash forensics, last 30 days (System and Application event logs)
13 unexpected reboot(s): 9 blue screen(s), 4 with no bugcheck (hang or power loss)
Verdicts: 6 standby, 4 gpu-fault, 2 hard-hang, 1 tdr-cascade
6 of 13 happened while the machine was in Modern Standby or asleep
GPU resets and hangs logged: 56 x GPU reset event, 24 x GPU timeout (0x117), 14 x GPU engine timeout (0x141); ...

2026-10-19 06:03  blue screen 0x3B SYSTEM_SERVICE_EXCEPTION, access violation
  blamed:   nvlddmkm!UvmChannelUpdateProgress (NVIDIA display driver)
  power:    in Modern Standby since 2026-10-19 01:18 (4h44m before the crash)
  GPU:      no GPU resets in the 10m before
  watch:    no watch log (record one with `vramorama watch --quiet --log FILE`)
  dump:     C:\Windows\Minidump\101926-21421-01.dmp
  verdict:  [standby] GPU driver fault 4h44m after it entered Modern Standby (screen off). Display-off power
            management is the trigger, not VRAM contention. ...
```

With a watch log that covers the crash, the `watch:` line becomes the state of the machine just before it,
and the verdict can change:

```text
  watch:    40 sample(s), last 5s before: card 15,700 of 15,915 MiB (peak 15,700), commit 95%, RAM free min 700 MiB
            held by: claude 11111111 9,000 MiB; chatgpt 22222222 6,500 MiB
            spilled to system memory: pid 4242 2,400 MiB
  verdict:  [memory-pressure] memory pressure before the crash (2,400 MiB spilled from the card into system
            memory; commit charge reached 95% of the commit limit; the card was full ...
```

Each incident ends in a verdict. They are tried in this order, and the first that the evidence supports wins:

| Verdict | Means |
|---|---|
| `memory-pressure` | The watch log shows spill into shared GPU memory, commit charge at 92% of its limit or more, under 512 MiB of free RAM, or a full card with two or more jobs. Contention is a credible cause. |
| `standby` | Modern Standby (or sleep) was in progress, or the crash came as the machine resumed, and the failure is a GPU, driver or power one or no bugcheck. The trigger is display-off power management, not the workload. |
| `tdr-cascade` | Two or more GPU resets in the ten minutes before. |
| `single-workload` | A GPU or display-driver fault with at most one job on the card and no spill or commit pressure: not contention. Needs a watch log. |
| `gpu-fault` | A GPU or display-driver fault without a log that settles whether the card was shared (or with several jobs but no pressure). |
| `hard-hang`, `forced-power-off` | No bugcheck was written (the power button had been held, for the second). A freeze with no dump points at the GPU, firmware or power delivery. |
| `other-bugcheck` | A bugcheck that is not about the graphics stack (memory, hardware, power transitions of other devices). |

To have the minutes before the next crash on record, keep a watcher running (each sample is flushed to the file as it is taken):
`vramorama watch --quiet --interval 5 --log $HOME\.vramorama\watch.jsonl`. `crashes --json` has every field,
`--days N` sets the window (default 30) and `--last N` the incidents listed (default 10).

### `vramorama doctor`

Checks the things that make those crashes likelier and says what to do about each. It never changes a
setting:

- **Screen-off and sleep timeouts** (`powrprof.dll`, the active plan, on AC and on battery) on a machine
  with **Modern Standby** (`CallNtPowerInformation`). On such a machine a screen-off timeout starts Modern
  Standby even when sleep is set to Never, and the GPU is power-gated while a job keeps running. Escalates
  to a risk when `crashes` found reboots in standby.
- **The display driver's version, release and age** from the registry, for every adapter (an NVIDIA
  `32.0.15.7705` is release 577.05).
- **Commit charge against its limit and free RAM**: the "going into swap" case.
- **Spill** into shared GPU memory, and the **power cap**, as in [Spill](#spill) and
  [Power cap](#power-cap).
- **GPU resets** in the last 24 hours and 7 days, and the crash record.

`--json` has the same findings; the [GUI](#the-gui) shows them as a **Crashes and risks** card.

### Staying out of Modern Standby while a job runs

`vramorama run` now asks Windows to keep the machine awake from admission until the job (and any
`--restart`) ends: `SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)`. It
changes no power setting or plan, Windows drops the request when the process ends however it ends, and the
job is never touched. The screen stays on, because the screen turning off is what starts Modern Standby on
these laptops; `--allow-display-off` holds only the system (the screen may switch off, and some machines
then enter standby anyway) and `--no-keep-awake` asks for nothing. A `--detach`ed job outlives `run`, so
nothing holds the machine awake for it.

`run` also waits, instead of starting, while the system commit charge is at or above `--max-commit PCT`
of its limit (default 92, `0` turns it off): a job that grows from there pushes the machine into swap, and
an allocation that fails inside a driver is a blue screen rather than an error message.

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
linked, and `pdh.dll`, `dxgi.dll`, `ntdll.dll`, `wevtapi.dll`, `powrprof.dll`, `advapi32.dll` and `nvml.dll` are resolved at run time, so no binding crates or
import libraries are involved. All `unsafe` code is in `src/sys.rs`, `src/gpu.rs`, `src/procs.rs`, `src/evt.rs`, `src/sysinfo.rs` and the two Windows calls in `src/gui.rs` (the system RNG and opening the browser).
The WMI fallback of `run --detach` calls the Windows PowerShell that ships with Windows. The CI
workflow uses no third-party actions (it checks out with plain `git`).

## Limits

- Windows 10/11 only, except `report` and `hook`, which run anywhere. The GUI needs a browser on
  the same machine. A Linux backend (NVML) is
  planned.
- `run` schedules only the jobs started through it, and nothing in vramorama stops or kills a
  process except, when asked with `--guard-power` or `--enforce-vram`, the job `run` itself started. Other GPU users are respected (their memory counts as in use) but not queued. The settle
  growth check protects new leases from a job that is still loading, but a job started outside
  `run` can still start on memory a lease has reserved and push the card into spill.
- The power-cap rule is a threshold on the enforced limit (at most half the default). It reads NVIDIA
  cards only, and it cannot tell a cap from a deliberately low limit set with `nvidia-smi -pl`.
- The spill rule is a threshold, not a measurement of where each allocation lives: a process with
  more than 512 MiB of pinned host buffers on a nearly full card is reported as spilled too.
- A server that waits idle between requests (an inference server between evaluation batches) is
  *idle* by these rules. The verdict only says nobody appears to own it, not that nobody uses it.
- `crashes` reads what Windows logged. It can name the module a fault bucket names and say what ran
  just before, but it does not open the minidump: for the exact call stack use WinDbg on the path it prints. Event logs
  roll over, so old crashes disappear; Windows writes the bucket (and so the blamed module) a minute or two after the
  reboot, and not always. The verdict is a reading of the evidence, not a proof: a standby fault and measured memory
  pressure can both be true, and the first matching verdict is reported.
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

1. Read the minidump itself (bugcheck parameters decoded per code, the faulting stack) and the
   LiveKernelReports, and a flight recorder the GUI can write on its own (a rolling watch log).
2. A WSL client on the same ledger, so jobs inside WSL queue with Windows ones.
3. Server awareness for `idle`: open client connections on a listening port count as use.
4. An MCP server for agents, and the Linux/NVML backend.

## License

MIT
