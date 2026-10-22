//! `m31c -heapwatch`: read a program's heap watch ledger
//! (docs/heap-watch-design.md; the runtime half is runtime/heapwatch.c).
//!
//! A compiled program keeps counters for every object it makes, by
//! allocation site, in a file it maps: `<program>.<pid>.ledger`, in
//! `$M31_HEAPWATCH_DIR` or `${TMPDIR:-/tmp}/m31-heapwatch-<uid>`. This reads
//! that file -- from a live process, or from a dead one, since the file is
//! the counters themselves and survives a crash or `kill -9` -- and says
//! which sites are leaking: a SUSPECT is a site whose oldest survivor is
//! much older than its objects normally live.
//!
//! The file layout is runtime/heapwatch.c's; every offset below is a field
//! there, and HW_VERSION changes with it. Read with plain file I/O: the
//! process writes through its mapping, and a read sees the page cache it
//! writes to.

use super::cli;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

static OPERANDS: [cli::Operand; 1] = [cli::Operand {
    name: "ledger",
    need: cli::Need::Required,
    help: "a .ledger file, or the pid of a running program",
}];

pub static SPEC: cli::Spec = cli::Spec {
    prog: "m31c -heapwatch",
    summary: "report what a program's heap holds, by allocation site",
    opts: &[
        cli::Opt {
            long: "all",
            short: None,
            kind: cli::Kind::Flag,
            placeholder: "",
            help: "list every site with live objects, not only the ones worth a look",
        },
        cli::Opt {
            long: "trace",
            short: None,
            kind: cli::Kind::Values,
            placeholder: "TYPE",
            help:
                "record who holds these types (Type or Type@file.m31:line) in the running process",
        },
        cli::Opt {
            long: "untrace",
            short: None,
            kind: cli::Kind::Values,
            placeholder: "TYPE",
            help: "stop recording who holds these types",
        },
        cli::Opt {
            long: "depth",
            short: None,
            kind: cli::Kind::Int,
            placeholder: "N",
            help: "follow holders this many levels up, tracing their sites as needed (default 3)",
        },
        cli::Opt {
            long: "paths",
            short: None,
            kind: cli::Kind::Flag,
            placeholder: "",
            help: "print who holds the traced objects (asking a running process first)",
        },
    ],
    operands: &OPERANDS,
    commands: &[],
    version: None,
    completions: false,
    epilog: HELP,
};

const HELP: &str = "A SUSPECT is a site whose oldest live object is older than
max(M31_HEAPWATCH_MIN minutes, M31_HEAPWATCH_FACTOR x the site's own p99
lifetime) -- 10 and 10 by default -- or than the limit M31_AGE gives it.
\"never freed\" is a site nothing of which has died yet: no baseline, so it is
listed for a person to judge. \"forever\" is a site or object marked as living
long on purpose (M31_AGE=Type:forever, heapwatch.forever(x)).

--trace asks a running process, through its ledger's control page, to record
who holds objects of a type from now on (M31_HEAPWATCH_TRACE=Type does the
same at start). The holders are written to <program>.<pid>.trace when a traced
site becomes a suspect or an epoch closes with its objects; --paths prints
that file, asking a running process for the paths as they are now first.";

const VERSION: u32 = 1;
const PAGE: usize = 4096;
const LEVELS: usize = 7;
const CELLS: usize = 8;
const LIFE: usize = 16;
const SLOT_FOREVER: u32 = 1;
const SLOT_LIMIT: u32 = 2;

pub fn run(parsed: &cli::Parsed) -> ExitCode {
    let path = match find(&parsed.operands[0]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("m31c -heapwatch: {e}");
            return ExitCode::FAILURE;
        }
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("m31c -heapwatch: cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let ledger = match Ledger::parse(&bytes) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("m31c -heapwatch: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut requests: Vec<(u32, String)> = Vec::new();
    for t in parsed.values("trace") {
        requests.push((1, t.clone()));
    }
    for t in parsed.values("untrace") {
        requests.push((2, t.clone()));
    }
    let depth = parsed.value("depth").and_then(|d| d.parse::<u32>().ok());
    if parsed.flag("paths") && ledger.running && alive(ledger.pid) {
        requests.push((3, String::new()));
    }
    if !requests.is_empty() || depth.is_some() {
        if !ledger.running || !alive(ledger.pid) {
            eprintln!(
                "m31c -heapwatch: {} (pid {}) is not running; trace can only be changed in a running process",
                ledger.program, ledger.pid
            );
            return ExitCode::FAILURE;
        }
        if let Err(e) = control(&path, &requests, depth.unwrap_or(0)) {
            eprintln!("m31c -heapwatch: {e}");
            return ExitCode::FAILURE;
        }
        for (op, spec) in &requests {
            match op {
                1 => println!("trace on for {spec} in pid {}", ledger.pid),
                2 => println!("trace off for {spec} in pid {}", ledger.pid),
                _ => {}
            }
        }
    }
    if parsed.flag("paths") {
        let trace = path.with_extension("trace");
        match std::fs::read_to_string(&trace) {
            Ok(t) => print!("{t}"),
            Err(_) => println!("no trace output yet: {}", trace.display()),
        }
        return ExitCode::SUCCESS;
    }
    if !requests.is_empty() {
        return ExitCode::SUCCESS;
    }
    // Read again: a request just applied may have changed what is traced.
    let bytes = std::fs::read(&path).unwrap_or(bytes);
    let Ok(ledger) = Ledger::parse(&bytes) else {
        return ExitCode::FAILURE;
    };
    print!("{}", report(&ledger, &path, parsed.flag("all")));
    ExitCode::SUCCESS
}

/// Write requests into a running process's control page and wait for it to
/// apply them (runtime/heapwatch.c, "the control page", has the layout).
/// Plain file writes: the process reads the same page through its mapping.
/// `<ledger>.lock`, made with create_new, keeps two tools from writing at
/// once.
fn control(path: &Path, requests: &[(u32, String)], depth: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    const OPS: usize = 31;
    if requests.len() > OPS {
        return Err(format!("at most {OPS} trace changes at a time"));
    }
    let lock = path.with_extension("ledger.lock");
    let mut tries = 0;
    let mut lock_file = loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(f) => break f,
            Err(_) if tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                return Err(format!(
                    "another m31c -heapwatch holds {} ({e}); remove it if none is running",
                    lock.display()
                ))
            }
        }
    };
    let _ = write!(lock_file, "{}", std::process::id());
    let result = (|| -> Result<u32, String> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("cannot open {} for writing: {e}", path.display()))?;
        let mut page = vec![0u8; 64];
        f.read_exact_at(&mut page, 0)
            .map_err(|e| format!("cannot read the control page: {e}"))?;
        if &page[0..8] != b"M31HWCTL" {
            return Err("the ledger has no control page".into());
        }
        let seq = u32_at(&page, 16).wrapping_add(1);
        let w = |at: u64, b: &[u8]| {
            f.write_all_at(b, at)
                .map_err(|e| format!("cannot write the control page: {e}"))
        };
        w(12, &std::process::id().to_le_bytes())?;
        for (i, (op, spec)) in requests.iter().enumerate() {
            let mut r = [0u8; 128];
            r[0..4].copy_from_slice(&op.to_le_bytes());
            let s = spec.as_bytes();
            let n = s.len().min(123);
            r[4..4 + n].copy_from_slice(&s[..n]);
            w(64 + 128 * i as u64, &r)?;
        }
        w(24, &depth.to_le_bytes())?;
        w(28, &(requests.len() as u32).to_le_bytes())?;
        // Last: the process acts once `seq` moves.
        w(16, &seq.to_le_bytes())?;
        w(12, &0u32.to_le_bytes())?;
        Ok(seq)
    })();
    let _ = std::fs::remove_file(&lock);
    let seq = result?;
    // The process looks at the page once a second.
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    for _ in 0..200 {
        let mut b = [0u8; 4];
        if f.read_exact_at(&mut b, 20).is_ok() && u32::from_le_bytes(b) == seq {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Err("the process did not take the request within 10 s".into())
}

/// The ledger a command line names: a path (with or without `.ledger`), or a
/// pid, looked for where programs put them.
fn find(what: &str) -> Result<PathBuf, String> {
    let p = Path::new(what);
    if p.is_file() {
        return Ok(p.to_path_buf());
    }
    let with = PathBuf::from(format!("{what}.ledger"));
    if with.is_file() {
        return Ok(with);
    }
    if !what.is_empty() && what.chars().all(|c| c.is_ascii_digit()) {
        let suffix = format!(".{what}.ledger");
        for dir in ledger_dirs() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().ends_with(&suffix) {
                    return Ok(e.path());
                }
            }
        }
        return Err(format!(
            "no ledger for pid {what} in $M31_HEAPWATCH_DIR or the default directory"
        ));
    }
    Err(format!("no such ledger: {what}"))
}

/// `$M31_HEAPWATCH_DIR`, then every `m31-heapwatch-*` under the temporary
/// directory (the runtime's default adds the user id, which std cannot ask
/// for; a directory that is not ours is simply unreadable).
fn ledger_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("M31_HEAPWATCH_DIR") {
        if !d.is_empty() {
            out.push(PathBuf::from(d));
        }
    }
    let tmp = std::env::var("TMPDIR")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    if let Ok(entries) = std::fs::read_dir(&tmp) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("m31-heapwatch-")
            })
            .map(|e| e.path())
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .unwrap_or(0)
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    u32_at(b, at) as i32
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .map(|s| {
            let mut a = [0u8; 8];
            a.copy_from_slice(s);
            u64::from_le_bytes(a)
        })
        .unwrap_or(0)
}

fn cstr_at(b: &[u8], at: usize, max: usize) -> String {
    let end = b.len().min(at.saturating_add(max));
    let s = b.get(at..end).unwrap_or(&[]);
    let n = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf8_lossy(&s[..n]).into_owned()
}

/// One allocation site: what it counts, from the slot table.
pub(crate) struct Slot {
    pub type_name: String,
    pub file: String,
    pub line: u32,
    pub flags: u32,
    pub limit: u32,
}

/// What a site holds, summed over every shard.
#[derive(Default)]
pub(crate) struct Sum {
    pub live: i64,
    pub forever: i64,
    pub freed: u64,
    /// Minutes: the upper bound of the p99 lifetime bucket; 1 means under a
    /// minute.
    pub p99: u64,
    pub oldest: Option<u64>,
    pub older_than: i64,
    pub limit: u64,
}

pub(crate) struct Ledger<'a> {
    b: &'a [u8],
    pub pid: u64,
    pub start_unix: i64,
    pub tick_unix: i64,
    pub minute: u32,
    pub minute_ms: u32,
    pub n_shards: u32,
    pub n_slots: u32,
    pub count_size: usize,
    off_slots: usize,
    off_strings: usize,
    off_epochs: usize,
    off_shards: usize,
    stride: usize,
    pub floor_min: u32,
    pub factor: u32,
    pub running: bool,
    pub lost_slots: u32,
    off_traced: usize,
    pub edges: u32,
    pub edges_dropped: u32,
    /// M31_HEAPWATCH_SAMPLE: the mean bytes between sampled objects; 1
    /// (or 0, from a program that predates sampling) counts every object.
    pub sample_bytes: u32,
    pub program: String,
}

impl<'a> Ledger<'a> {
    pub fn parse(b: &'a [u8]) -> Result<Ledger<'a>, String> {
        let h = PAGE;
        if b.len() < 2 * PAGE || &b[h..h + 8] != b"M31HWLDG" {
            return Err("not a heap watch ledger (or the program had not started yet)".into());
        }
        let version = u32_at(b, h + 8);
        if version != VERSION {
            return Err(format!(
                "ledger version {version}; this m31c reads version {VERSION}"
            ));
        }
        let l = Ledger {
            b,
            pid: u64_at(b, h + 16),
            start_unix: u64_at(b, h + 24) as i64,
            tick_unix: u64_at(b, h + 32) as i64,
            minute: u32_at(b, h + 40),
            minute_ms: u32_at(b, h + 44),
            n_shards: u32_at(b, h + 48),
            n_slots: u32_at(b, h + 56),
            count_size: u32_at(b, h + 60) as usize,
            off_slots: u64_at(b, h + 64) as usize,
            off_strings: u64_at(b, h + 80) as usize,
            off_epochs: u64_at(b, h + 88) as usize,
            off_shards: u64_at(b, h + 96) as usize,
            stride: u64_at(b, h + 104) as usize,
            floor_min: u32_at(b, h + 120),
            factor: u32_at(b, h + 124),
            running: u32_at(b, h + 128) != 0,
            lost_slots: u32_at(b, h + 132),
            off_traced: u64_at(b, h + 72) as usize,
            edges: u32_at(b, h + 140),
            edges_dropped: u32_at(b, h + 144),
            sample_bytes: u32_at(b, h + 148),
            program: cstr_at(b, h + 152, 128),
        };
        if l.count_size != 320 || l.off_shards + l.n_shards as usize * l.stride > b.len() {
            return Err("the ledger's layout does not match its size".into());
        }
        Ok(l)
    }

    /// Whether trace is on for slot `s`.
    pub fn traced(&self, s: u32) -> bool {
        self.b
            .get(self.off_traced + s as usize)
            .is_some_and(|&v| v != 0)
    }

    pub fn string(&self, off: u32) -> String {
        cstr_at(self.b, self.off_strings + off as usize, 4096)
    }

    pub fn slot(&self, s: u32) -> Slot {
        let at = self.off_slots + s as usize * 32;
        Slot {
            type_name: self.string(u32_at(self.b, at)),
            file: self.string(u32_at(self.b, at + 4)),
            line: u32_at(self.b, at + 8),
            flags: u32_at(self.b, at + 12),
            limit: u32_at(self.b, at + 16),
        }
    }

    /// The epochs that closed with leaks: (name, times, minute, last).
    pub fn epochs(&self) -> Vec<(String, u64, u32, String)> {
        let mut out = Vec::new();
        for i in 0..64 {
            let at = self.off_epochs + i * 128;
            let times = u64_at(self.b, at + 48);
            if times == 0 {
                continue;
            }
            out.push((
                cstr_at(self.b, at, 48),
                times,
                u32_at(self.b, at + 56),
                cstr_at(self.b, at + 64, 64),
            ));
        }
        out
    }

    /// Slot `s` added over every shard, the way runtime/heapwatch.c's
    /// hw_sum_slot does: each nonzero wheel cell is an aligned block of
    /// minutes; a block inside a coarser block some shard holds is merged
    /// into it, so an object's +1 on one shard and -1 on another meet.
    pub fn sum(&self, s: u32) -> Sum {
        let mut out = Sum::default();
        let mut life = [0u64; LIFE];
        let mut blocks: Vec<(usize, u64, i64)> = Vec::new();
        for i in 0..self.n_shards as usize {
            let head = self.off_shards + i * self.stride;
            if u32_at(self.b, head + 4) == 0 {
                continue;
            }
            let sm = u32_at(self.b, head) as u64;
            let c = head + 64 + s as usize * self.count_size;
            out.forever += i32_at(self.b, c + 288) as i64;
            for (k, l) in life.iter_mut().enumerate() {
                *l += u32_at(self.b, c + 224 + 4 * k) as u64;
            }
            for lv in 0..LEVELS {
                let big_k = sm >> (3 * lv);
                for idx in 0..CELLS {
                    let v = i32_at(self.b, c + (lv * CELLS + idx) * 4) as i64;
                    if v == 0 {
                        continue;
                    }
                    let k = big_k.wrapping_sub(big_k.wrapping_sub(idx as u64) & 7);
                    match blocks.iter_mut().find(|b| b.0 == lv && b.1 == k) {
                        Some(b) => b.2 += v,
                        None => blocks.push((lv, k, v)),
                    }
                }
            }
        }
        for lv in 0..LEVELS - 1 {
            for i in 0..blocks.len() {
                if blocks[i].0 != lv || blocks[i].2 == 0 {
                    continue;
                }
                for up in lv + 1..LEVELS {
                    let k = blocks[i].1 >> (3 * (up - lv));
                    if let Some(j) = blocks.iter().position(|b| b.0 == up && b.1 == k) {
                        blocks[j].2 += blocks[i].2;
                        blocks[i].2 = 0;
                        break;
                    }
                }
            }
        }
        out.freed = life.iter().sum();
        if out.freed > 0 {
            let want = (out.freed * 99).div_ceil(100);
            let mut seen = 0;
            for (b, n) in life.iter().enumerate() {
                seen += n;
                if seen >= want {
                    out.p99 = if b == 0 { 1 } else { 1 << b };
                    break;
                }
            }
        }
        let slot = self.slot(s);
        out.limit = if slot.flags & SLOT_LIMIT != 0 {
            slot.limit as u64
        } else {
            (out.p99 * self.factor as u64).max(self.floor_min as u64)
        };
        out.live = out.forever;
        let now = self.minute as u64;
        for (lv, k, v) in blocks {
            if v <= 0 {
                continue;
            }
            out.live += v;
            let span = 1u64 << (3 * lv);
            let last = (k + 1) * span - 1;
            let young = now.saturating_sub(last);
            if out.oldest.is_none_or(|o| young > o) {
                out.oldest = Some(young);
            }
            if young > out.limit {
                out.older_than += v;
            }
        }
        out
    }
}

/// An age in minutes of the program's clock, as a report prints it.
pub(crate) fn age(minutes: u64) -> String {
    if minutes == 0 {
        "<1m".to_string()
    } else if minutes < 120 {
        format!("{minutes}m")
    } else if minutes < 2880 {
        format!("{}h", minutes / 60)
    } else {
        format!("{}d", minutes / 1440)
    }
}

/// Whether process `pid` is still there: `kill -0`, which every Unix has
/// as a command, since std has no call for it.
fn alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The whole report, as text.
fn report(l: &Ledger, path: &Path, all: bool) -> String {
    let mut o = String::new();
    let now = now_unix();
    let state = if !l.running {
        "exited".to_string()
    } else if alive(l.pid) {
        "running".to_string()
    } else {
        format!(
            "not running -- it crashed or was killed; last seen {} ago",
            age(((now - l.tick_unix).max(0) / 60) as u64)
        )
    };
    o.push_str(&format!(
        "{} (pid {}), {state}; up {}, minute {} of its clock\n",
        l.program,
        l.pid,
        age(((l.tick_unix - l.start_unix).max(0) / 60) as u64),
        l.minute
    ));
    if l.minute_ms != 60000 {
        o.push_str(&format!("  (a minute is {} ms in this run)\n", l.minute_ms));
    }
    if l.sample_bytes > 1 {
        o.push_str(&format!(
            "sampled: one object in every {} bytes made, on average; counts are estimates\n",
            l.sample_bytes
        ));
    }
    let _ = path;

    struct Row {
        kind: &'static str,
        name: String,
        site: String,
        text: String,
    }
    let mut rows: Vec<Row> = Vec::new();
    for s in 1..l.n_slots {
        let slot = l.slot(s);
        let sum = l.sum(s);
        // Every object a site made is alive or has died at some age.
        let made = sum.live.max(0) as u64 + sum.freed;
        if sum.live <= 0 && !(all && made > 0) {
            continue;
        }
        let site = format!("{}:{}", slot.file, slot.line);
        let p99 = if sum.freed == 0 {
            "none".to_string()
        } else if sum.p99 <= 1 {
            "<1m".to_string()
        } else {
            age(sum.p99)
        };
        let old = sum.oldest.unwrap_or(0);
        let (kind, text) = if slot.flags & SLOT_FOREVER != 0 {
            ("forever", format!("{} alive", sum.live))
        } else if sum.live - sum.forever > 0
            && (sum.freed > 0 || slot.flags & SLOT_LIMIT != 0)
            && old > sum.limit
        {
            (
                "SUSPECT",
                format!(
                    "{} alive > {} (limit {}, p99 lifetime {p99})",
                    sum.older_than,
                    age(old),
                    age(sum.limit)
                ),
            )
        } else if sum.forever > 0 && sum.forever == sum.live {
            ("forever", format!("{} alive (marked)", sum.live))
        } else if sum.freed == 0 && sum.live > 0 {
            (
                "never",
                format!("{} alive, never freed, oldest > {}", sum.live, age(old)),
            )
        } else if all {
            (
                "ok",
                format!(
                    "{} alive, {made} made, oldest > {}, p99 lifetime {p99}",
                    sum.live,
                    age(old)
                ),
            )
        } else {
            continue;
        };
        let text = if sum.forever > 0 && kind != "forever" {
            format!("{text}; {} marked forever", sum.forever)
        } else {
            text
        };
        rows.push(Row {
            kind,
            name: slot.type_name,
            site,
            text,
        });
    }
    let order = |k: &str| match k {
        "SUSPECT" => 0,
        "never" => 1,
        "forever" => 2,
        _ => 3,
    };
    rows.sort_by_key(|r| order(r.kind));
    let wn = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let ws = rows.iter().map(|r| r.site.len()).max().unwrap_or(0);
    for r in &rows {
        o.push_str(&format!(
            "{:<8} {:<wn$}  {:<ws$}  {}\n",
            r.kind, r.name, r.site, r.text
        ));
    }
    if rows.is_empty() {
        o.push_str("nothing to report\n");
    }
    for (name, times, minute, last) in l.epochs() {
        let s = if times == 1 { "" } else { "s" };
        o.push_str(&format!(
            "epoch \"{name}\" closed with leaks {times} time{s}; last at minute {minute}: {last}\n"
        ));
    }
    let traced: Vec<String> = (1..l.n_slots)
        .filter(|&s| l.traced(s))
        .map(|s| {
            let slot = l.slot(s);
            // A slot of one of the runtime's own kinds has no line: trace
            // knows a List by its kind (runtime/heapwatch.c, "sampling").
            if slot.line == 0 {
                slot.type_name
            } else {
                format!("{} {}:{}", slot.type_name, slot.file, slot.line)
            }
        })
        .collect();
    if !traced.is_empty() {
        o.push_str(&format!(
            "trace on for {}; {} edges recorded{}\n",
            traced.join(", "),
            l.edges,
            if l.edges_dropped > 0 {
                format!(", {} dropped at the cap", l.edges_dropped)
            } else {
                String::new()
            }
        ));
    }
    if l.lost_slots > 0 {
        o.push_str(&format!(
            "({} sites found the ledger full and are not counted)\n",
            l.lost_slots
        ));
    }
    o
}
