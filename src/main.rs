//! vramorama: who is holding my GPU?

// Off Windows only `report` exists, so most of the shared code is unused there.
#![cfg_attr(not(windows), allow(dead_code))]

mod json;
mod owner;
mod parse;
mod report;
mod transcripts;

#[cfg(windows)]
mod app;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod procs;
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

OPTIONS
  --json        machine-readable output
  --all         include integrated and software adapters
  --min MIB     hide processes holding less than MIB (default 16)
  --wide        do not truncate to the console width
  --trace       run `trace` for every untagged process

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
    pub positional: Vec<String>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { min_mib: 16, interval: 5.0, ..Default::default() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--json" => o.json = true,
            "--all" => o.all = true,
            "--wide" => o.wide = true,
            "--trace" => o.trace = true,
            "--quiet" | "-q" => o.quiet = true,
            "--min" => o.min_mib = value("--min")?.parse().map_err(|_| "--min takes a whole number of MiB")?,
            "--interval" | "-n" => {
                o.interval = value("--interval")?.parse().map_err(|_| "--interval takes seconds")?;
                if !(0.5..=86_400.0).contains(&o.interval) {
                    return Err("--interval must be between 0.5 and 86400 seconds".into());
                }
            }
            "--log" => o.log = Some(value("--log")?),
            s if s.starts_with('-') => return Err(format!("unknown option {s}; see --help")),
            s => o.positional.push(s.to_string()),
        }
    }
    Ok(o)
}

fn run(args: &[String]) -> Result<(), String> {
    let (cmd, rest) = match args.first().map(String::as_str) {
        None => ("ps", args),
        Some(s) if s.starts_with('-') && !matches!(s, "-h" | "--help" | "-V" | "--version") => ("ps", args),
        Some(s) => (s, &args[1..]),
    };
    match cmd {
        "-h" | "--help" | "help" => {
            print!("{HELP}");
            Ok(())
        }
        "-V" | "--version" | "version" => {
            println!("vramorama {}", env!("CARGO_PKG_VERSION"));
            Ok(())
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
            Ok(())
        }
        "ps" | "watch" | "trace" => os_command(cmd, parse_opts(rest)?),
        other => Err(format!("unknown command {other}; see --help")),
    }
}

#[cfg(windows)]
fn os_command(cmd: &str, o: Opts) -> Result<(), String> {
    match cmd {
        "ps" => app::ps(&o),
        "watch" => app::watch(&o),
        _ => {
            let [pid] = o.positional.as_slice() else { return Err("usage: vramorama trace PID".into()) };
            app::trace(pid.parse().map_err(|_| format!("not a pid: {pid}"))?)
        }
    }
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
    if let Err(e) = run(&args) {
        eprintln!("vramorama: {e}");
        std::process::exit(2);
    }
}
