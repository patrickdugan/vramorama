//! vramorama: who is holding my GPU?

// Off Windows only `report` exists, so most of the shared code is unused there.
#![cfg_attr(not(windows), allow(dead_code))]

mod hook;
mod idle;
mod json;
mod ledger;
mod owner;
mod parse;
mod report;
mod transcripts;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod gui;
#[cfg(windows)]
mod procs;
#[cfg(windows)]
mod run;
#[cfg(windows)]
mod scan;
#[cfg(windows)]
mod sys;

const HELP: &str = "\
vramorama: who is holding my GPU?

USAGE
  vramorama [ps] [--json] [--all] [--min MIB] [--wide] [--trace]
      Processes holding GPU memory now, with the agent session that launched each one.
  vramorama watch [--interval SECS] [--log FILE] [--quiet] [--all] [--min MIB]
      Refresh every SECS (default 5), keeping each process's and each owner's peak
      (the running watermark). --log appends one JSON line per sample.
  vramorama report FILE [--json]
      Summarise a watch log: GiB-hours, peak and time held per owner.
  vramorama trace PID
      Find the agent transcript whose session launched PID (for untagged processes).
  vramorama run --vram SIZE [--owner NAME] [--detach [--out FILE]] [--wait SECS]
                [--settle SECS] [--headroom MIB] [--adapter NAME] [--quiet] -- COMMAND...
      Wait until SIZE (e.g. 9G, 6000M) fits on the card, first come first served, then
      run COMMAND tagged VRAMORAMA_LEASE=<id> and keep the reservation until it and its
      children exit. --detach starts it outside this session (output to --out).
  vramorama leases [--json] [--headroom MIB] [--adapter NAME]
      Current leases, and how much a new request could get now.
  vramorama hook
      Claude Code PreToolUse hook: blocks WMI / Task Scheduler launches that would
      strip a job's session tags (exit code 2 with the reason on stderr).
  vramorama idle [--window SECS] [--min MIB] [--json]
      Processes that held memory but did no GPU work for SECS (default 20). Stale means
      the agent session that started it has ended; untagged processes are never stale.
  vramorama gui [--port N] [--no-open] [--interval SECS] [--all] [--min MIB]
      A live page in your browser: memory by owner, an hour of history, processes,
      idle and stale holders, leases, and trace. Served on 127.0.0.1 (default port
      7787; 0 picks a free one) with an access token; read-only.
  vramorama reclaim [PID...] [--force] [--window SECS]
      Re-check idle holders (all stale ones if no PID is given) and print the command
      that would free their memory. vramorama never stops a process itself.

OPTIONS
  --json        machine-readable output
  --all         include integrated and software adapters
  --min MIB     hide processes holding less than MIB (default 16; 256 for idle)
  --wide        do not truncate to the console width
  --trace       run `trace` for every untagged process
  --headroom    MiB left free for the desktop and drivers when admitting (default 512)
  --settle      seconds the memory must stay free before admission (default 5)

OWNERSHIP
  A process is tagged by environment variables it inherited:
    VRAMORAMA_OWNER          any label you choose (wins over the rest)
    CLAUDE_CODE_SESSION_ID   set by Claude Code in every shell it starts
    AI_AGENT                 set by some agent harnesses
  Jobs launched through WMI (Invoke-CimMethod Win32_Process) or Task Scheduler start
  with a clean environment and lose these tags; `trace` searches transcripts instead.
";

#[derive(Default)]
pub struct Opts {
    pub json: bool,
    pub all: bool,
    pub min_mib: u64,
    pub wide: bool,
    pub trace: bool,
    pub interval: f64,
    pub log: Option<String>,
    pub quiet: bool,
    pub headroom_mib: u64,
    pub adapter: Option<String>,
    pub window: Option<f64>,
    pub force: bool,
    pub min_given: bool,
    pub port: Option<u16>,
    pub no_open: bool,
    pub positional: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { min_mib: 16, interval: 5.0, headroom_mib: 512, ..Default::default() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--json" => o.json = true,
            "--all" => o.all = true,
            "--wide" => o.wide = true,
            "--trace" => o.trace = true,
            "--quiet" | "-q" => o.quiet = true,
            "--min" => {
                o.min_mib = value("--min")?.parse().map_err(|_| "--min takes a whole number of MiB")?;
                o.min_given = true;
            }
            "--force" => o.force = true,
            "--no-open" => o.no_open = true,
            "--port" => o.port = Some(value("--port")?.parse().map_err(|_| "--port takes a number from 0 to 65535")?),
            "--window" => {
                let w: f64 = value("--window")?.parse().map_err(|_| "--window takes seconds")?;
                if !(2.0..=3600.0).contains(&w) {
                    return Err("--window must be between 2 and 3600 seconds".into());
                }
                o.window = Some(w);
            }
            "--interval" | "-n" => {
                o.interval = value("--interval")?.parse().map_err(|_| "--interval takes seconds")?;
                if !(0.5..=86_400.0).contains(&o.interval) {
                    return Err("--interval must be between 0.5 and 86400 seconds".into());
                }
            }
            "--log" => o.log = Some(value("--log")?),
            "--adapter" => o.adapter = Some(value("--adapter")?),
            "--headroom" => o.headroom_mib = value("--headroom")?.parse().map_err(|_| "--headroom takes MiB")?,
            s if s.starts_with('-') => return Err(format!("unknown option {s}; see --help")),
            s => o.positional.push(s.to_string()),
        }
    }
    Ok(o)
}

fn dispatch(args: &[String]) -> Result<i32, String> {
    let (cmd, rest) = match args.first().map(String::as_str) {
        None => ("ps", args),
        Some(s) if s.starts_with('-') && !matches!(s, "-h" | "--help" | "-V" | "--version") => ("ps", args),
        Some(s) => (s, &args[1..]),
    };
    match cmd {
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(0)
        }
        "-V" | "--version" | "version" => {
            println!("vramorama {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        "hook" => {
            // A hook that fails must not block the agent, so unreadable input is a pass.
            let mut input = String::new();
            if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
                return Ok(0);
            }
            if !hook::should_block(&input) {
                return Ok(0);
            }
            let exe = std::env::current_exe().map_or_else(|_| "vramorama".into(), |p| p.display().to_string());
            eprintln!("{}", hook::reason(&exe));
            Ok(2)
        }
        "report" => {
            let o = parse_opts(rest)?;
            let [path] = o.positional.as_slice() else { return Err("usage: vramorama report FILE".into()) };
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            let r = report::summarise(&text);
            if o.json {
                println!("{}", report::to_json(&r));
            } else {
                print!("{}", report::render(&r, if o.wide { usize::MAX } else { width() }));
            }
            Ok(0)
        }
        "run" => os_run(rest),
        "ps" | "watch" | "trace" | "leases" | "idle" | "gui" => os_command(cmd, parse_opts(rest)?).map(|()| 0),
        "reclaim" => os_reclaim(parse_opts(rest)?),
        other => Err(format!("unknown command {other}; see --help")),
    }
}

#[cfg(windows)]
fn os_command(cmd: &str, o: Opts) -> Result<(), String> {
    match cmd {
        "ps" => app::ps(&o),
        "watch" => app::watch(&o),
        "leases" => run::leases(o.json, o.adapter.as_deref(), o.headroom_mib),
        "gui" => gui::gui(gui::GuiOpts {
            port: o.port.unwrap_or(7787),
            open: !o.no_open,
            interval: if o.interval == 5.0 { 2.0 } else { o.interval },
            all: o.all,
            min_mib: o.min_mib,
        }),
        "idle" => {
            let o = Opts { min_mib: if o.min_given { o.min_mib } else { 256 }, ..o };
            let window = o.window.unwrap_or(20.0);
            app::idle(&o, window)
        }
        _ => {
            let [pid] = o.positional.as_slice() else { return Err("usage: vramorama trace PID".into()) };
            app::trace(pid.parse().map_err(|_| format!("not a pid: {pid}"))?)
        }
    }
}

/// `run` options come before `--`; everything after it is the command.
#[cfg(windows)]
fn os_run(args: &[String]) -> Result<i32, String> {
    let split = args.iter().position(|a| a == "--");
    let (flags, argv) = match split {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => return Err("usage: vramorama run --vram SIZE [options] -- COMMAND...".into()),
    };
    if argv.is_empty() {
        return Err("no command after --".into());
    }
    let mut o = run::RunOpts {
        vram_mib: 0,
        owner: None,
        detach: false,
        out: None,
        wait_secs: None,
        settle_secs: 5.0,
        headroom_mib: 512,
        adapter: None,
        quiet: false,
        argv,
    };
    let mut it = flags.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        let secs =
            |s: String, name: &str| s.parse::<f64>().ok().filter(|v| *v >= 0.0).ok_or(format!("{name} takes seconds"));
        match a.as_str() {
            "--vram" => o.vram_mib = ledger::parse_size_mib(&value("--vram")?)?,
            "--owner" => o.owner = Some(value("--owner")?),
            "--detach" => o.detach = true,
            "--out" => o.out = Some(value("--out")?),
            "--wait" => o.wait_secs = Some(secs(value("--wait")?, "--wait")?),
            "--settle" => o.settle_secs = secs(value("--settle")?, "--settle")?,
            "--headroom" => o.headroom_mib = value("--headroom")?.parse().map_err(|_| "--headroom takes MiB")?,
            "--adapter" => o.adapter = Some(value("--adapter")?),
            "--quiet" | "-q" => o.quiet = true,
            s => return Err(format!("unknown run option {s}; see --help")),
        }
    }
    if o.vram_mib == 0 {
        return Err("run needs --vram SIZE (for example --vram 9G)".into());
    }
    if o.out.is_some() && !o.detach {
        return Err("--out only applies with --detach".into());
    }
    run::run(&o)
}

#[cfg(windows)]
fn os_reclaim(o: Opts) -> Result<i32, String> {
    let pids = o
        .positional
        .iter()
        .map(|p| p.parse::<u32>().map_err(|_| format!("not a pid: {p}")))
        .collect::<Result<Vec<_>, _>>()?;
    app::reclaim(&pids, o.force, o.window.unwrap_or(10.0))
}

#[cfg(not(windows))]
fn os_reclaim(_o: Opts) -> Result<i32, String> {
    Err("`reclaim` needs Windows in this version.".into())
}

#[cfg(not(windows))]
fn os_run(_args: &[String]) -> Result<i32, String> {
    Err("`run` needs Windows in this version; a Linux (NVML) backend is planned.".into())
}

#[cfg(not(windows))]
fn os_command(cmd: &str, _o: Opts) -> Result<(), String> {
    Err(format!("`{cmd}` needs Windows in this version; a Linux (NVML) backend is planned. `report` works everywhere."))
}

#[cfg(windows)]
fn width() -> usize {
    sys::console().unwrap_or(160)
}

#[cfg(not(windows))]
fn width() -> usize {
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(160)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("vramorama: {e}");
            std::process::exit(2);
        }
    }
}
