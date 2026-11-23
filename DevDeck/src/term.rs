//! Integrated terminal: Windows ConPTY backend + a compact VT/xterm emulator.
//!
//! The shell runs attached to a pseudo console. A reader thread blocks in
//! `ReadFile` on the output pipe (zero CPU when idle) and hands bytes to the UI
//! thread, which feeds them through [`Screen::feed`].

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{ClosePseudoConsole, CreatePseudoConsole, ResizePseudoConsole, COORD, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

// ================================================================== backend

pub enum TermEvent {
    Data(Vec<u8>),
    Exited,
}

pub struct Pty {
    hpc: HPCON,
    input: HANDLE,
    process: HANDLE,
    pub pid: u32,
}

unsafe impl Send for Pty {}

impl Pty {
    /// Starts `cmdline` attached to a new pseudo console of `cols` x `rows`.
    pub fn spawn(cmdline: &str, cwd: &Path, cols: u16, rows: u16, hwnd: HWND, msg: u32, pending: Arc<AtomicBool>) -> Result<(Pty, Receiver<TermEvent>), String> {
        unsafe {
            let (mut in_read, mut in_write, mut out_read, mut out_write): (HANDLE, HANDLE, HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if CreatePipe(&mut in_read, &mut in_write, std::ptr::null(), 0) == 0 || CreatePipe(&mut out_read, &mut out_write, std::ptr::null(), 0) == 0 {
                return Err("Unable to create pipes".into());
            }
            let mut hpc: HPCON = 0;
            let size = COORD { X: cols.max(2) as i16, Y: rows.max(1) as i16 };
            let hr = CreatePseudoConsole(size, in_read, out_write, 0, &mut hpc);
            // the pseudo console owns duplicates of these now
            CloseHandle(in_read);
            CloseHandle(out_write);
            if hr < 0 {
                CloseHandle(in_write);
                CloseHandle(out_read);
                return Err(format!("CreatePseudoConsole failed (0x{hr:08X}). Windows 10 1809 or later is required."));
            }

            let mut attr_size = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size);
            let mut attr_buf = vec![0u8; attr_size];
            let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
            if InitializeProcThreadAttributeList(attrs, 1, 0, &mut attr_size) == 0
                || UpdateProcThreadAttribute(attrs, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize, hpc as *const _, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null())
                    == 0
            {
                ClosePseudoConsole(hpc);
                CloseHandle(in_write);
                CloseHandle(out_read);
                return Err("Unable to set up the pseudo console attribute".into());
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            // do not let the child inherit our own (possibly redirected) std handles
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.lpAttributeList = attrs;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let mut cmd: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
            let dir: Vec<u16> = cwd.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            let ok = CreateProcessW(
                std::ptr::null(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                std::ptr::null(),
                if cwd.is_dir() { dir.as_ptr() } else { std::ptr::null() },
                &si.StartupInfo,
                &mut pi,
            );
            DeleteProcThreadAttributeList(attrs);
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                ClosePseudoConsole(hpc);
                CloseHandle(in_write);
                CloseHandle(out_read);
                return Err(format!("Unable to start '{cmdline}': {err}"));
            }
            CloseHandle(pi.hThread);

            let (tx, rx) = channel();
            let out = out_read as isize;
            let hwnd_v = hwnd as isize;
            std::thread::Builder::new()
                .name("pty-reader".into())
                .stack_size(128 * 1024)
                .spawn(move || {
                    let mut buf = vec![0u8; 32 * 1024];
                    loop {
                        let mut n = 0u32;
                        let ok = ReadFile(out as HANDLE, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut());
                        if ok == 0 || n == 0 {
                            break;
                        }
                        if tx.send(TermEvent::Data(buf[..n as usize].to_vec())).is_err() {
                            break;
                        }
                        if !pending.swap(true, Ordering::AcqRel) {
                            PostMessageW(hwnd_v as HWND, msg, 0, 0);
                        }
                    }
                    let _ = tx.send(TermEvent::Exited);
                    PostMessageW(hwnd_v as HWND, msg, 0, 0);
                    CloseHandle(out as HANDLE);
                })
                .map_err(|e| e.to_string())?;
            Ok((Pty { hpc, input: in_write, process: pi.hProcess, pid: pi.dwProcessId }, rx))
        }
    }

    pub fn write(&self, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let mut n = 0u32;
            let ok = unsafe { WriteFile(self.input, data[off..].as_ptr(), (data.len() - off) as u32, &mut n, std::ptr::null_mut()) };
            if ok == 0 || n == 0 {
                break;
            }
            off += n as usize;
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        unsafe {
            ResizePseudoConsole(self.hpc, COORD { X: cols.max(2) as i16, Y: rows.max(1) as i16 });
        }
    }

    pub fn exit_code(&self) -> Option<u32> {
        let mut code = 0u32;
        unsafe {
            if GetExitCodeProcess(self.process, &mut code) != 0 && code != 259 {
                Some(code)
            } else {
                None
            }
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // ClosePseudoConsole may wait for the output pipe to drain; the reader
        // thread keeps draining, but do it off the UI thread to be safe.
        let (hpc, input, process) = (self.hpc, self.input as isize, self.process as isize);
        std::thread::spawn(move || unsafe {
            ClosePseudoConsole(hpc);
            CloseHandle(input as HANDLE);
            let mut code = 0u32;
            if GetExitCodeProcess(process as HANDLE, &mut code) != 0 && code == 259 {
                TerminateProcess(process as HANDLE, 1);
            }
            CloseHandle(process as HANDLE);
        });
    }
}

// ------------------------------------------------------------------- shells

#[derive(Clone, Debug, PartialEq)]
pub struct Shell {
    pub name: String,
    pub cmd: String,
}

fn which(exe: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Shells available on this machine, default first (like VS Code's profiles).
pub fn detect_shells() -> Vec<Shell> {
    let mut v = Vec::new();
    if let Some(p) = which("pwsh.exe") {
        v.push(Shell { name: "PowerShell 7".into(), cmd: format!("\"{}\" -NoLogo", p.display()) });
    }
    let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let ps = format!(r"{sysroot}\System32\WindowsPowerShell\v1.0\powershell.exe");
    if Path::new(&ps).is_file() {
        v.push(Shell { name: "PowerShell".into(), cmd: format!("\"{ps}\" -NoLogo") });
    }
    let comspec = std::env::var("ComSpec").unwrap_or_else(|_| format!(r"{sysroot}\System32\cmd.exe"));
    v.push(Shell { name: "Command Prompt".into(), cmd: format!("\"{comspec}\"") });
    // Git Bash next to git.exe (…\Git\cmd\git.exe -> …\Git\bin\bash.exe)
    // git.exe may live in Git\cmd, Git\bin or Git\mingw64\bin: walk up to the install root
    let mut bash = None;
    if let Some(git) = which("git.exe") {
        for dir in git.ancestors().skip(1).take(4) {
            let b = dir.join("bin").join("bash.exe");
            if b.is_file() && dir.join("git-bash.exe").is_file() || b.is_file() && dir.join("usr").is_dir() {
                bash = Some(b);
                break;
            }
        }
    }
    if bash.is_none() {
        for base in [std::env::var("ProgramFiles").ok(), std::env::var("LOCALAPPDATA").ok().map(|l| format!(r"{l}\Programs"))].into_iter().flatten() {
            let b = Path::new(&base).join("Git").join("bin").join("bash.exe");
            if b.is_file() {
                bash = Some(b);
                break;
            }
        }
    }
    if let Some(b) = bash {
        v.push(Shell { name: "Git Bash".into(), cmd: format!("\"{}\" --login -i", b.display()) });
    }
    if let Some(wsl) = which("wsl.exe") {
        v.push(Shell { name: "WSL".into(), cmd: format!("\"{}\"", wsl.display()) });
    }
    v
}

/// A coding agent CLI that runs as a terminal profile in an editor tab.
pub struct Agent {
    /// Profile name of its sessions.
    pub name: &'static str,
    /// Executable name without extension: `<exe>.exe`, or npm's `<exe>.cmd`.
    exe: &'static str,
    /// Where to install it from.
    pub url: &'static str,
    /// Its WinGet package id.
    winget: &'static str,
}

pub const CLAUDE: Agent = Agent { name: "Claude Code", exe: "claude", url: "https://claude.com/claude-code", winget: "Anthropic.ClaudeCode" };
pub const CODEX: Agent = Agent { name: "Codex", exe: "codex", url: "https://github.com/openai/codex", winget: "OpenAI.Codex" };

/// Whether a terminal profile is a coding agent's session.
pub fn is_agent(name: &str) -> bool {
    [&CLAUDE, &CODEX].iter().any(|a| a.name == name)
}

impl Agent {
    /// The CLI as a terminal profile, when it is installed: `<exe>.exe` on PATH, else where a
    /// native installer (%USERPROFILE%\.local\bin) or WinGet (its Links folder) put it, which
    /// this process's PATH may lack when it was installed after DevDeck started; else npm's
    /// `<exe>.cmd`.
    pub fn find(&self) -> Option<Shell> {
        let dir = |var: &str, sub: &[&str]| std::env::var_os(var).map(|v| sub.iter().fold(std::path::PathBuf::from(v), |p, s| p.join(s)));
        let known = [dir("USERPROFILE", &[".local", "bin"]), dir("LOCALAPPDATA", &["Microsoft", "WinGet", "Links"])];
        let exe = format!("{}.exe", self.exe);
        let found = which(&exe).or_else(|| known.into_iter().flatten().map(|d| d.join(&exe)).find(|p| p.is_file()));
        if let Some(p) = found {
            return Some(Shell { name: self.name.into(), cmd: format!("\"{}\"", p.display()) });
        }
        let cmd = format!("{}.cmd", self.exe);
        let appdata = std::env::var_os("APPDATA").map(std::path::PathBuf::from);
        let script = which(&cmd).or_else(|| appdata.map(|a| a.join("npm").join(&cmd)).filter(|p| p.is_file()))?;
        // a batch file runs through cmd.exe; /s strips the outer quotes and keeps the quoted path
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
        Some(Shell { name: self.name.into(), cmd: format!("\"{comspec}\" /d /s /c \"\"{}\"\"", script.display()) })
    }

    /// The files `find` looks for, for messages.
    pub fn exe_names(&self) -> String {
        format!("{0}.exe or {0}.cmd", self.exe)
    }

    /// The command that installs it with WinGet.
    pub fn install_command(&self) -> String {
        format!("winget install -e --id {}", self.winget)
    }

    /// A terminal profile that runs `install_command` in cmd.exe and stays open afterwards,
    /// so its output (and any prompt) can be read.
    pub fn installer(&self) -> Shell {
        let comspec = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into());
        Shell { name: format!("Install {}", self.name), cmd: format!("\"{comspec}\" /d /k {}", self.install_command()) }
    }
}

// ================================================================ emulator

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const UNDERLINE: u8 = 4;
pub const INVERSE: u8 = 8;
pub const ITALIC: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

#[derive(Clone, Copy)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: u8,
}

impl Default for Pen {
    fn default() -> Self {
        Pen { fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

enum State {
    Ground,
    Esc,
    /// ESC + intermediate byte(s); holds the first one (`(` designates G0).
    EscInter(char),
    Csi,
    Osc,
    OscEsc,
}

/// Cells a character takes: 0 for combining marks, 2 for East Asian wide and emoji.
/// A small table of the common ranges, not full Unicode (a missed char takes one cell).
fn width(c: char) -> u8 {
    let c = c as u32;
    if c < 0x300 {
        return 1;
    }
    const ZERO: &[(u32, u32)] = &[(0x300, 0x36F), (0x200B, 0x200F), (0x20D0, 0x20FF), (0xFE00, 0xFE0F)];
    #[rustfmt::skip]
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F), (0x231A, 0x231B), (0x23E9, 0x23EC), (0x23F0, 0x23F0), (0x23F3, 0x23F3), (0x25FD, 0x25FE),
        (0x2614, 0x2615), (0x2648, 0x2653), (0x267F, 0x267F), (0x2693, 0x2693), (0x26A1, 0x26A1), (0x26AA, 0x26AB),
        (0x26BD, 0x26BE), (0x26C4, 0x26C5), (0x26CE, 0x26CE), (0x26D4, 0x26D4), (0x26EA, 0x26EA), (0x26F2, 0x26F3),
        (0x26F5, 0x26F5), (0x26FA, 0x26FA), (0x26FD, 0x26FD), (0x2705, 0x2705), (0x270A, 0x270B), (0x2728, 0x2728),
        (0x274C, 0x274C), (0x274E, 0x274E), (0x2753, 0x2755), (0x2757, 0x2757), (0x2795, 0x2797), (0x27B0, 0x27B0),
        (0x27BF, 0x27BF), (0x2B1B, 0x2B1C), (0x2B50, 0x2B50), (0x2B55, 0x2B55), (0x2E80, 0xA4CF), (0xAC00, 0xD7A3),
        (0xF900, 0xFAFF), (0xFE30, 0xFE4F), (0xFF00, 0xFF60), (0xFFE0, 0xFFE6), (0x1F300, 0x1F64F), (0x1F680, 0x1F6FF),
        (0x1F900, 0x1FAFF), (0x20000, 0x3FFFD),
    ];
    let has = |t: &[(u32, u32)]| t.iter().any(|&(a, b)| c >= a && c <= b);
    if has(ZERO) {
        0
    } else if has(WIDE) {
        2
    } else {
        1
    }
}

/// VT100 DEC Special Graphics for '`'..='~' (line drawing, selected with ESC ( 0).
const DEC_GRAPHICS: [char; 31] = [
    '\u{25C6}', '\u{2592}', '\u{2409}', '\u{240C}', '\u{240D}', '\u{240A}', '\u{B0}', '\u{B1}', '\u{2424}', '\u{240B}', '\u{2518}', '\u{2510}', '\u{250C}', '\u{2514}', '\u{253C}', '\u{23BA}',
    '\u{23BB}', '\u{2500}', '\u{23BC}', '\u{23BD}', '\u{251C}', '\u{2524}', '\u{2534}', '\u{252C}', '\u{2502}', '\u{2264}', '\u{2265}', '\u{3C0}', '\u{2260}', '\u{A3}', '\u{B7}',
];

pub type Line = Vec<Cell>;

pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<Line>,
    alt_saved: Option<(Vec<Line>, (usize, usize))>,
    pub scrollback: VecDeque<Line>,
    pub max_scrollback: usize,
    pub cx: usize,
    pub cy: usize,
    wrap_pending: bool,
    pen: Pen,
    /// DECSC state: cursor, pen and pending wrap.
    saved: (usize, usize, Pen, bool),
    /// G0 is DEC Special Graphics (ESC ( 0): box drawing in place of '`'..='~'.
    g0_graphics: bool,
    top: usize,
    bottom: usize,
    pub cursor_visible: bool,
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    autowrap: bool,
    state: State,
    params: String,
    osc: String,
    /// The string in progress is a DCS/SOS/PM/APC: consumed, never acted on.
    osc_skip: bool,
    utf8: Vec<u8>,
    pub title: String,
    /// Responses the emulator must send back to the application (e.g. DSR).
    pub replies: Vec<u8>,
    /// Incremented whenever lines are pushed to scrollback (used to keep selections stable).
    pub scrolled_total: u64,
    /// Incremented per line dropped from the front of the scrollback, which shifts every
    /// absolute line index down (selections follow it).
    pub trimmed: u64,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Screen {
        let cols = cols.max(2);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            lines: vec![vec![Cell::default(); cols]; rows],
            alt_saved: None,
            scrollback: VecDeque::new(),
            max_scrollback: 3000,
            cx: 0,
            cy: 0,
            wrap_pending: false,
            pen: Pen::default(),
            saved: (0, 0, Pen::default(), false),
            g0_graphics: false,
            top: 0,
            bottom: rows - 1,
            cursor_visible: true,
            app_cursor: false,
            bracketed_paste: false,
            autowrap: true,
            state: State::Ground,
            params: String::new(),
            osc: String::new(),
            osc_skip: false,
            utf8: Vec::new(),
            title: String::new(),
            replies: Vec::new(),
            scrolled_total: 0,
            trimmed: 0,
        }
    }

    pub fn in_alt_screen(&self) -> bool {
        self.alt_saved.is_some()
    }

    /// Erasing, inserting or deleting cells can split a wide char from its spacer cell:
    /// blank a wide char without its spacer and a spacer without its wide char.
    fn fix_wide_pairs(&mut self) {
        let line = &mut self.lines[self.cy];
        for i in 0..line.len() {
            let lost = match line[i].ch {
                '\0' => i == 0 || width(line[i - 1].ch) != 2,
                c => width(c) == 2 && line.get(i + 1).is_none_or(|n| n.ch != '\0'),
            };
            if lost {
                line[i].ch = ' ';
            }
        }
    }

    fn blank(&self) -> Cell {
        Cell { ch: ' ', fg: Color::Default, bg: self.pen.bg, flags: 0 }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        for l in self.lines.iter_mut() {
            l.resize(cols, Cell::default());
        }
        // shrinking: drop the lines below the cursor first (like conhost, which keeps the
        // viewport top), then move top lines into scrollback so the cursor stays visible
        while self.lines.len() > rows {
            if self.lines.len() > self.cy + 1 {
                self.lines.pop();
            } else {
                let l = self.lines.remove(0);
                self.push_scrollback(l);
                self.cy -= 1;
            }
        }
        while self.lines.len() < rows {
            self.lines.push(vec![Cell::default(); cols]);
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.cx = self.cx.min(cols - 1);
        self.cy = self.cy.min(rows - 1);
        self.wrap_pending = false;
        if let Some((saved, _)) = self.alt_saved.as_mut() {
            for l in saved.iter_mut() {
                l.resize(cols, Cell::default());
            }
            saved.resize(rows, vec![Cell::default(); cols]);
        }
    }

    fn push_scrollback(&mut self, mut l: Line) {
        if self.alt_saved.is_some() {
            return;
        }
        // store history lines without their blank tail (most lines are short)
        let keep = l.iter().rposition(|c| *c != Cell::default()).map(|i| i + 1).unwrap_or(0);
        l.truncate(keep);
        l.shrink_to_fit();
        self.scrollback.push_back(l);
        self.scrolled_total += 1;
        while self.scrollback.len() > self.max_scrollback {
            self.scrollback.pop_front();
            self.trimmed += 1;
        }
    }

    // counts are clamped to the region: more would only blank it again (CSI 99999999 S)
    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n.min(self.bottom - self.top + 1) {
            let removed = self.lines.remove(self.top);
            if self.top == 0 {
                self.push_scrollback(removed);
            }
            let b = self.blank();
            self.lines.insert(self.bottom, vec![b; self.cols]);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n.min(self.bottom - self.top + 1) {
            self.lines.remove(self.bottom);
            let b = self.blank();
            self.lines.insert(self.top, vec![b; self.cols]);
        }
    }

    fn line_feed(&mut self) {
        if self.cy == self.bottom {
            self.scroll_up(1);
        } else if self.cy + 1 < self.rows {
            self.cy += 1;
        }
    }

    fn put(&mut self, ch: char) {
        let w = width(ch) as usize;
        if w == 0 {
            return;
        }
        let ch = if self.g0_graphics && ('`'..='~').contains(&ch) { DEC_GRAPHICS[ch as usize - 0x60] } else { ch };
        if self.wrap_pending {
            self.wrap_pending = false;
            if self.autowrap {
                self.cx = 0;
                self.line_feed();
            }
        }
        // a wide char that does not fit in the last column goes to the next line
        if w == 2 && self.cx + 1 >= self.cols {
            if self.autowrap {
                self.cx = 0;
                self.line_feed();
            } else {
                self.cx = self.cols - 2;
            }
        }
        let cell = Cell { ch, fg: self.pen.fg, bg: self.pen.bg, flags: self.pen.flags };
        let (cx, line) = (self.cx, &mut self.lines[self.cy]);
        // overwriting half of a wide char blanks its other half
        if line[cx].ch == '\0' && cx > 0 && width(line[cx - 1].ch) == 2 {
            line[cx - 1].ch = ' ';
        }
        if line.get(cx + w).is_some_and(|c| c.ch == '\0') {
            line[cx + w].ch = ' ';
        }
        line[cx] = cell;
        if w == 2 {
            // spacer cell: drawn by the glyph before it, skipped when copying
            line[cx + 1] = Cell { ch: '\0', ..cell };
        }
        if cx + w >= self.cols {
            self.cx = self.cols - 1;
            self.wrap_pending = true;
        } else {
            self.cx += w;
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        // UTF-8 decode (keeping incomplete sequences between reads)
        let mut bytes = std::mem::take(&mut self.utf8);
        bytes.extend_from_slice(data);
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let len = if b < 0x80 {
                1
            } else if b >> 5 == 0b110 {
                2
            } else if b >> 4 == 0b1110 {
                3
            } else if b >> 3 == 0b11110 {
                4
            } else {
                i += 1;
                continue;
            };
            // a lead byte without its continuation bytes is one bad character: what follows
            // (ASCII, an ESC sequence) is still read
            if (1..len.min(bytes.len() - i)).any(|k| bytes[i + k] & 0xC0 != 0x80) {
                i += 1;
                self.handle('\u{FFFD}');
                continue;
            }
            if i + len > bytes.len() {
                self.utf8 = bytes[i..].to_vec();
                break;
            }
            let ch = std::str::from_utf8(&bytes[i..i + len]).ok().and_then(|s| s.chars().next()).unwrap_or('\u{FFFD}');
            i += len;
            self.handle(ch);
        }
    }

    fn handle(&mut self, ch: char) {
        match self.state {
            State::Ground => match ch {
                '\x1b' => self.state = State::Esc,
                '\r' => {
                    self.cx = 0;
                    self.wrap_pending = false;
                }
                '\n' | '\x0b' | '\x0c' => {
                    self.wrap_pending = false;
                    self.line_feed();
                }
                '\x08' => {
                    self.wrap_pending = false;
                    self.cx = self.cx.saturating_sub(1);
                }
                '\t' => {
                    let next = ((self.cx / 8) + 1) * 8;
                    self.cx = next.min(self.cols - 1);
                }
                '\x07' | '\0' | '\x0e' | '\x0f' => {}
                c if (c as u32) < 0x20 || c == '\x7f' => {}
                c => self.put(c),
            },
            State::Esc => {
                self.state = State::Ground;
                match ch {
                    '[' => {
                        self.params.clear();
                        self.state = State::Csi;
                    }
                    // OSC, and DCS/SOS/PM/APC strings that are consumed without effect
                    ']' | 'P' | 'X' | '^' | '_' => {
                        self.osc.clear();
                        self.osc_skip = ch != ']';
                        self.state = State::Osc;
                    }
                    '7' => self.save_cursor(),
                    '8' => self.restore_cursor(),
                    'M' => {
                        self.wrap_pending = false;
                        if self.cy == self.top {
                            self.scroll_down(1);
                        } else {
                            self.cy = self.cy.saturating_sub(1);
                        }
                    }
                    'D' => {
                        self.wrap_pending = false;
                        self.line_feed();
                    }
                    'E' => {
                        self.wrap_pending = false;
                        self.cx = 0;
                        self.line_feed();
                    }
                    'c' => {
                        let (c, r) = (self.cols, self.rows);
                        let sb = std::mem::take(&mut self.scrollback);
                        let (scrolled, trimmed) = (self.scrolled_total, self.trimmed);
                        *self = Screen::new(c, r);
                        self.scrollback = sb;
                        // counters only grow: the UI diffs them
                        (self.scrolled_total, self.trimmed) = (scrolled, trimmed);
                    }
                    ' '..='/' => self.state = State::EscInter(ch),
                    _ => {}
                }
            }
            State::EscInter(i) => {
                // more intermediates may follow; the next other char ends the sequence
                if !(' '..='/').contains(&ch) {
                    self.state = State::Ground;
                    if i == '(' {
                        self.g0_graphics = ch == '0';
                    }
                }
            }
            State::Csi => {
                // controls inside a sequence: ESC starts a new one, CAN / SUB cancel it, the
                // others (CR, LF, BS ...) run as usual and the sequence goes on
                if ch == '\x1b' {
                    self.state = State::Esc;
                } else if ch == '\x18' || ch == '\x1a' {
                    self.state = State::Ground;
                } else if (ch as u32) < 0x20 {
                    self.state = State::Ground;
                    self.handle(ch);
                    self.state = State::Csi;
                } else if ('\x40'..='\x7e').contains(&ch) {
                    let params = std::mem::take(&mut self.params);
                    self.state = State::Ground;
                    self.csi(&params, ch);
                } else if self.params.len() < 64 {
                    self.params.push(ch);
                }
            }
            State::Osc => match ch {
                '\x07' => {
                    self.osc_done();
                    self.state = State::Ground;
                }
                '\x1b' => self.state = State::OscEsc,
                c => {
                    if self.osc.len() < 2048 && !self.osc_skip {
                        self.osc.push(c)
                    }
                }
            },
            State::OscEsc => {
                self.osc_done();
                self.state = State::Ground;
                // an ESC that is not ST ends the string and starts a new sequence
                if ch != '\\' {
                    self.state = State::Esc;
                    self.handle(ch);
                }
            }
        }
    }

    fn save_cursor(&mut self) {
        self.saved = (self.cx, self.cy, self.pen, self.wrap_pending);
    }

    fn restore_cursor(&mut self) {
        self.cx = self.saved.0.min(self.cols - 1);
        self.cy = self.saved.1.min(self.rows - 1);
        self.pen = self.saved.2;
        // a pending wrap only makes sense in the last column (the grid may have been resized)
        self.wrap_pending = self.saved.3 && self.cx + 1 == self.cols;
    }

    fn osc_done(&mut self) {
        let osc = std::mem::take(&mut self.osc);
        if std::mem::take(&mut self.osc_skip) {
            return;
        }
        if let Some((code, text)) = osc.split_once(';') {
            if code == "0" || code == "2" {
                self.title = text.chars().take(200).collect();
            }
        }
    }

    fn csi(&mut self, raw: &str, f: char) {
        let prefix = raw.chars().next().filter(|c| "<=>?".contains(*c));
        // a prefix or intermediate bytes make it another command sharing the final byte (vim's
        // CSI >4;2m, kitty's CSI >1u / CSI <u, DECSCUSR CSI n SP q...); of those only DEC modes,
        // selective erase (same as erase here) and DECDSR are supported
        let intermediate = raw.bytes().any(|b| (0x20..=0x2F).contains(&b));
        if intermediate || prefix.is_some_and(|c| c != '?' || !"hlJKn".contains(f)) {
            return;
        }
        let body: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == ';' || *c == ':').collect();
        let num = |s: &str| s.parse().unwrap_or(0).min(0xFFFF);
        let ps: Vec<usize> = if body.is_empty() {
            vec![]
        } else if f == 'm' && body.contains(':') {
            // SGR sub-parameters: 4:N is an underline style (4:0 = none), 38:2::R:G:B /
            // 38:5:N a colour; spelled out the way the ';' forms are
            let mut v = Vec::new();
            for g in body.split(';') {
                let s: Vec<usize> = g.split(':').map(num).collect();
                match s.as_slice() {
                    [4, n, ..] => v.push(if *n == 0 { 24 } else { 4 }),
                    [c @ (38 | 48 | 58), 2, rest @ ..] if rest.len() >= 3 => v.extend([*c, 2].iter().chain(&rest[rest.len() - 3..])),
                    [c @ (38 | 48 | 58), 5, n, ..] => v.extend([*c, 5, *n]),
                    _ => v.push(s[0]),
                }
            }
            v
        } else {
            body.split(';').map(|p| num(p.split(':').next().unwrap_or(""))).collect()
        };
        let p = |i: usize, d: usize| -> usize { ps.get(i).copied().filter(|&v| v != 0).unwrap_or(d) };
        // only commands that move the cursor or edit the grid cancel a pending wrap
        if !"hlncqptms".contains(f) {
            self.wrap_pending = false;
        }
        match f {
            'A' => self.cy = self.cy.saturating_sub(p(0, 1)).max(if self.cy >= self.top { self.top } else { 0 }),
            'B' | 'e' => self.cy = (self.cy + p(0, 1)).min(if self.cy <= self.bottom { self.bottom } else { self.rows - 1 }),
            'C' | 'a' => self.cx = (self.cx + p(0, 1)).min(self.cols - 1),
            'D' => self.cx = self.cx.saturating_sub(p(0, 1)),
            'E' => {
                self.cy = (self.cy + p(0, 1)).min(self.rows - 1);
                self.cx = 0;
            }
            'F' => {
                self.cy = self.cy.saturating_sub(p(0, 1));
                self.cx = 0;
            }
            'G' | '`' => self.cx = (p(0, 1) - 1).min(self.cols - 1),
            'd' => self.cy = (p(0, 1) - 1).min(self.rows - 1),
            'H' | 'f' => {
                self.cy = (p(0, 1) - 1).min(self.rows - 1);
                self.cx = (p(1, 1) - 1).min(self.cols - 1);
            }
            'J' => {
                let b = self.blank();
                match ps.first().copied().unwrap_or(0) {
                    0 => {
                        for c in self.cx..self.cols {
                            self.lines[self.cy][c] = b;
                        }
                        for r in self.cy + 1..self.rows {
                            self.lines[r] = vec![b; self.cols];
                        }
                    }
                    1 => {
                        for r in 0..self.cy {
                            self.lines[r] = vec![b; self.cols];
                        }
                        for c in 0..=self.cx.min(self.cols - 1) {
                            self.lines[self.cy][c] = b;
                        }
                    }
                    2 => {
                        for r in 0..self.rows {
                            self.lines[r] = vec![b; self.cols];
                        }
                    }
                    3 => {
                        self.trimmed += self.scrollback.len() as u64;
                        self.scrollback.clear();
                    }
                    _ => {}
                }
                // an erase that splits a wide character leaves no half of it behind
                self.fix_wide_pairs();
            }
            'K' => {
                let b = self.blank();
                let (s, e) = match ps.first().copied().unwrap_or(0) {
                    0 => (self.cx, self.cols),
                    1 => (0, self.cx + 1),
                    _ => (0, self.cols),
                };
                for c in s..e.min(self.cols) {
                    self.lines[self.cy][c] = b;
                }
                self.fix_wide_pairs();
            }
            'X' => {
                let b = self.blank();
                let n = p(0, 1);
                for c in self.cx..(self.cx + n).min(self.cols) {
                    self.lines[self.cy][c] = b;
                }
                self.fix_wide_pairs();
            }
            'P' => {
                let n = p(0, 1).min(self.cols - self.cx);
                let b = self.blank();
                let line = &mut self.lines[self.cy];
                line.drain(self.cx..self.cx + n);
                line.extend(std::iter::repeat(b).take(n));
                self.fix_wide_pairs();
            }
            '@' => {
                let n = p(0, 1).min(self.cols - self.cx);
                let b = self.blank();
                let cols = self.cols;
                let line = &mut self.lines[self.cy];
                for _ in 0..n {
                    line.insert(self.cx, b);
                }
                line.truncate(cols);
                self.fix_wide_pairs();
            }
            'L' => {
                if self.cy >= self.top && self.cy <= self.bottom {
                    let n = p(0, 1).min(self.bottom - self.cy + 1);
                    self.cx = 0;
                    for _ in 0..n {
                        self.lines.remove(self.bottom);
                        let b = self.blank();
                        self.lines.insert(self.cy, vec![b; self.cols]);
                    }
                }
            }
            'M' => {
                if self.cy >= self.top && self.cy <= self.bottom {
                    let n = p(0, 1).min(self.bottom - self.cy + 1);
                    self.cx = 0;
                    for _ in 0..n {
                        self.lines.remove(self.cy);
                        let b = self.blank();
                        self.lines.insert(self.bottom, vec![b; self.cols]);
                    }
                }
            }
            'S' => self.scroll_up(p(0, 1)),
            'T' => self.scroll_down(p(0, 1)),
            'r' => {
                let t = p(0, 1) - 1;
                let b = p(1, self.rows) - 1;
                if t < b && b < self.rows {
                    self.top = t;
                    self.bottom = b;
                } else {
                    self.top = 0;
                    self.bottom = self.rows - 1;
                }
                self.cx = 0;
                self.cy = 0;
            }
            's' => self.save_cursor(),
            'u' => self.restore_cursor(),
            'm' => self.sgr(&ps),
            'h' | 'l' => {
                let on = f == 'h';
                if prefix == Some('?') {
                    for &m in &ps {
                        match m {
                            1 => self.app_cursor = on,
                            7 => self.autowrap = on,
                            25 => self.cursor_visible = on,
                            2004 => self.bracketed_paste = on,
                            47 | 1047 | 1049 => self.set_alt(on),
                            _ => {}
                        }
                    }
                }
            }
            'n' => {
                if ps.first() == Some(&6) {
                    self.replies.extend_from_slice(format!("\x1b[{};{}R", self.cy + 1, self.cx + 1).as_bytes());
                } else if ps.first() == Some(&5) {
                    self.replies.extend_from_slice(b"\x1b[0n");
                }
            }
            'c' => self.replies.extend_from_slice(b"\x1b[?1;0c"),
            _ => {}
        }
    }

    fn set_alt(&mut self, on: bool) {
        if on && self.alt_saved.is_none() {
            let main = std::mem::replace(&mut self.lines, vec![vec![Cell::default(); self.cols]; self.rows]);
            self.alt_saved = Some((main, (self.cx, self.cy)));
        } else if !on {
            if let Some((main, (x, y))) = self.alt_saved.take() {
                self.lines = main;
                self.cx = x.min(self.cols - 1);
                self.cy = y.min(self.rows - 1);
                // a full-screen app that quits mid line drawing must not garble the shell
                self.g0_graphics = false;
                self.wrap_pending = false;
            }
        }
    }

    fn sgr(&mut self, ps: &[usize]) {
        if ps.is_empty() {
            self.pen = Pen::default();
            return;
        }
        let mut i = 0;
        while i < ps.len() {
            let v = ps[i];
            match v {
                0 => self.pen = Pen::default(),
                1 => self.pen.flags |= BOLD,
                2 => self.pen.flags |= DIM,
                3 => self.pen.flags |= ITALIC,
                4 => self.pen.flags |= UNDERLINE,
                7 => self.pen.flags |= INVERSE,
                22 => self.pen.flags &= !(BOLD | DIM),
                23 => self.pen.flags &= !ITALIC,
                24 => self.pen.flags &= !UNDERLINE,
                27 => self.pen.flags &= !INVERSE,
                30..=37 => self.pen.fg = Color::Idx((v - 30) as u8),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Idx((v - 40) as u8),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Idx((v - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Idx((v - 100 + 8) as u8),
                // 58: underline colour, not drawn; its operands are skipped all the same
                38 | 48 | 58 => {
                    let c = match ps.get(i + 1) {
                        Some(5) => {
                            i += 2;
                            ps.get(i).map(|&n| Color::Idx(n.min(255) as u8))
                        }
                        Some(2) => {
                            i += 4;
                            match (ps.get(i - 2), ps.get(i - 1), ps.get(i)) {
                                (Some(&r), Some(&g), Some(&b)) => Some(Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some(c) = c {
                        if v == 38 {
                            self.pen.fg = c;
                        } else if v == 48 {
                            self.pen.bg = c;
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Total number of lines (scrollback + screen).
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.rows
    }

    /// Line by absolute index (0 = oldest scrollback line).
    pub fn line(&self, i: usize) -> Option<&Line> {
        if i < self.scrollback.len() {
            self.scrollback.get(i)
        } else {
            self.lines.get(i - self.scrollback.len())
        }
    }

    pub fn line_text(&self, i: usize, from: usize, to: usize) -> String {
        match self.line(i) {
            // '\0' is the second cell of a wide char
            Some(l) => l[from.min(l.len())..to.min(l.len())].iter().map(|c| c.ch).filter(|&c| c != '\0').collect::<String>(),
            None => String::new(),
        }
    }
}

// ---------------------------------------------------------------- colors

pub fn ansi_color(idx: u8, dark: bool) -> u32 {
    const DARK: [u32; 16] = [0x000000, 0xCD3131, 0x0DBC79, 0xE5E510, 0x2472C8, 0xBC3FBC, 0x11A8CD, 0xE5E5E5, 0x666666, 0xF14C4C, 0x23D18B, 0xF5F543, 0x3B8EEA, 0xD670D6, 0x29B8DB, 0xE5E5E5];
    const LIGHT: [u32; 16] = [0x000000, 0xCD3131, 0x00BC00, 0x949800, 0x0451A5, 0xBC05BC, 0x0598BC, 0x555555, 0x666666, 0xCD3131, 0x14CE14, 0xB5BA00, 0x0451A5, 0xBC05BC, 0x0598BC, 0xA5A5A5];
    let rgb = if idx < 16 {
        if dark { DARK[idx as usize] } else { LIGHT[idx as usize] }
    } else if idx < 232 {
        let i = idx as u32 - 16;
        let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
        (level(i / 36) << 16) | (level((i / 6) % 6) << 8) | level(i % 6)
    } else {
        let g = 8 + (idx as u32 - 232) * 10;
        (g << 16) | (g << 8) | g
    };
    crate::theme::hex(rgb)
}

// ----------------------------------------------------------------- input

/// Translates a non-character key (WM_KEYDOWN) into the VT input sequence.
pub fn key_sequence(vk: u16, ctrl: bool, shift: bool, alt: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let modc = 1 + (shift as u8) + 2 * (alt as u8) + 4 * (ctrl as u8);
    let csi_mod = |base: &str, letter: char| -> Vec<u8> {
        if modc > 1 {
            format!("\x1b[1;{modc}{letter}").into_bytes()
        } else {
            format!("{base}{letter}").into_bytes()
        }
    };
    let arrow = |letter: char| -> Vec<u8> {
        if modc > 1 {
            format!("\x1b[1;{modc}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modc > 1 {
            format!("\x1b[{n};{modc}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    Some(match vk {
        0x26 => arrow('A'),
        0x28 => arrow('B'),
        0x27 => arrow('C'),
        0x25 => arrow('D'),
        0x24 => csi_mod("\x1b[", 'H'),
        0x23 => csi_mod("\x1b[", 'F'),
        0x2D => tilde(2),
        0x2E => tilde(3),
        0x21 => tilde(5),
        0x22 => tilde(6),
        0x70 => csi_mod("\x1bO", 'P'),
        0x71 => csi_mod("\x1bO", 'Q'),
        0x72 => csi_mod("\x1bO", 'R'),
        0x73 => csi_mod("\x1bO", 'S'),
        0x74 => tilde(15),
        0x75 => tilde(17),
        0x76 => tilde(18),
        0x77 => tilde(19),
        0x78 => tilde(20),
        0x79 => tilde(21),
        0x7A => tilde(23),
        0x7B => tilde(24),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &Screen, row: usize) -> String {
        s.lines[row].iter().map(|c| c.ch).filter(|&c| c != '\0').collect::<String>().trim_end().to_string()
    }

    #[test]
    fn prints_and_wraps() {
        let mut s = Screen::new(5, 3);
        s.feed(b"hello world");
        assert_eq!(text(&s, 0), "hello");
        assert_eq!(text(&s, 1), " worl");
        assert_eq!(text(&s, 2), "d");
    }

    #[test]
    fn scrolls_into_history() {
        let mut s = Screen::new(10, 2);
        s.feed(b"a\r\nb\r\nc\r\nd");
        assert_eq!(s.scrollback.len(), 2);
        assert_eq!(text(&s, 0), "c");
        assert_eq!(text(&s, 1), "d");
        assert_eq!(s.line_text(0, 0, 1), "a");
    }

    #[test]
    fn cursor_erase_and_colors() {
        let mut s = Screen::new(10, 3);
        s.feed(b"\x1b[2;3Hxy\x1b[1;31mZ\x1b[0m\x1b[1;1Habcdef\x1b[1;3H\x1b[K");
        assert_eq!(text(&s, 0), "ab");
        assert_eq!(text(&s, 1), "  xyZ");
        assert_eq!(s.lines[1][4].fg, Color::Idx(1));
        assert_eq!(s.lines[1][4].flags & BOLD, BOLD);
        s.feed(b"\x1b[38;2;10;20;30mT\x1b[48;5;200mU");
        assert_eq!(s.lines[0][2].fg, Color::Rgb(10, 20, 30));
        assert_eq!(s.lines[0][3].bg, Color::Idx(200));
        // colon sub-parameters; an underline colour (58) leaves the pen alone
        s.feed(b"\x1b[0m\x1b[2;1H\x1b[4mA\x1b[4:0mB\x1b[38:2::1:2:3mC\x1b[58;2;0;2;0mD\x1b[4:3mE");
        assert_eq!(s.lines[1][0].flags & UNDERLINE, UNDERLINE);
        assert_eq!(s.lines[1][1].flags & UNDERLINE, 0);
        assert_eq!(s.lines[1][2].fg, Color::Rgb(1, 2, 3));
        assert_eq!((s.lines[1][3].fg, s.lines[1][3].flags & DIM), (Color::Rgb(1, 2, 3), 0));
        assert_eq!(s.lines[1][4].flags & UNDERLINE, UNDERLINE);
    }

    #[test]
    fn title_utf8_and_alt_screen() {
        let mut s = Screen::new(10, 2);
        s.feed("\x1b]0;my title\x07h\u{e9}".as_bytes()[..14].as_ref());
        s.feed(&"\x1b]0;my title\x07h\u{e9}".as_bytes()[14..]);
        assert_eq!(s.title, "my title");
        assert_eq!(text(&s, 0), "h\u{e9}");
        s.feed(b"\x1b[?1049h\x1b[HALT");
        assert!(s.in_alt_screen());
        assert_eq!(text(&s, 0), "ALT");
        s.feed(b"\x1b[?1049l");
        assert_eq!(text(&s, 0), "h\u{e9}");
    }

    #[test]
    fn dsr_reply_and_keys() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[3;4H\x1b[6n");
        assert_eq!(s.replies, b"\x1b[3;4R");
        assert_eq!(key_sequence(0x26, false, false, false, false).unwrap(), b"\x1b[A");
        assert_eq!(key_sequence(0x26, false, false, false, true).unwrap(), b"\x1bOA");
        assert_eq!(key_sequence(0x25, true, false, false, false).unwrap(), b"\x1b[1;5D");
        assert_eq!(key_sequence(0x2E, false, false, false, false).unwrap(), b"\x1b[3~");
    }

    #[test]
    fn wide_and_zero_width_chars() {
        let mut s = Screen::new(5, 3);
        s.feed("a\u{4E2D}b".as_bytes());
        assert_eq!(s.lines[0][1].ch, '\u{4E2D}');
        assert_eq!(s.lines[0][2].ch, '\0');
        assert_eq!(s.cx, 4);
        assert_eq!(s.line_text(0, 0, 5).trim_end(), "a\u{4E2D}b");
        // a combining mark takes no cell; a wide char that does not fit wraps first
        s.feed("e\u{301}\u{1F600}".as_bytes());
        assert_eq!(text(&s, 0), "a\u{4E2D}be");
        assert_eq!(s.lines[1][0].ch, '\u{1F600}');
        assert_eq!((s.cx, s.cy), (2, 1));
        // overwriting half of a wide char blanks the other half
        s.feed(b"\x1b[1;3Hx");
        assert_eq!(s.lines[0][1].ch, ' ');
        assert_eq!(s.lines[0][2].ch, 'x');
        // deleting a wide char's first cell leaves no orphan spacer that later blanks a neighbour
        let mut s = Screen::new(6, 2);
        s.feed("a\u{4E2D}b\x1b[1;2H\x1b[P".as_bytes());
        assert_eq!(text(&s, 0), "a b");
        s.feed(b"\x1b[1;3Hx");
        assert_eq!(text(&s, 0), "a x");
    }

    #[test]
    fn prefixed_and_intermediate_csi_ignored() {
        let mut s = Screen::new(10, 5);
        s.feed(b"\x1b[3;4H\x1b[s\x1b[1;1H\x1b[>4;2m\x1b[>1u\x1b[<u\x1b[=1u\x1b[?u\x1b[2 q\x1b[3 A");
        assert_eq!((s.cx, s.cy), (0, 0));
        assert_eq!(s.pen.flags, 0);
        s.feed(b"\x1b[2;4r\x1b[?r\x1b[5;5H\x1b[?S");
        assert_eq!((s.top, s.bottom), (1, 3));
        assert_eq!((s.cx, s.cy), (4, 4));
        s.feed(b"\x1b[>c\x1b[=c");
        assert!(s.replies.is_empty());
        s.feed(b"\x1b[?25l\x1b[u");
        assert!(!s.cursor_visible);
        assert_eq!((s.cx, s.cy), (3, 2));
    }

    #[test]
    fn dcs_and_apc_consumed() {
        let mut s = Screen::new(20, 2);
        s.feed(b"a\x1bP+q544e\x1b\\b\x1b_Gf=24;AAAA\x07c\x1b^pm\x1b\\d\x1bXsos\x1b\\e");
        assert_eq!(text(&s, 0), "abcde");
        // an ESC that is not ST ends the string and starts a new sequence
        s.feed(b"\x1b]0;t\x1b[1;1HZ");
        assert_eq!(text(&s, 0), "Zbcde");
        s.feed(b"\x1b P\x1b%Gx");
        assert_eq!(text(&s, 0), "Zxcde");
    }

    #[test]
    fn scroll_counts_clamped() {
        let mut s = Screen::new(10, 3);
        s.feed(b"a\r\nb\r\nc\x1b[99999999S\x1b[99999999T");
        assert_eq!(s.scrollback.len(), 3);
        assert_eq!(text(&s, 0), "");
        s.feed(b"\x1b[2;5Hx\x1b[L");
        assert_eq!((s.cx, s.cy), (0, 1));
    }

    #[test]
    fn pending_wrap_saved_and_kept() {
        let mut s = Screen::new(3, 3);
        s.feed(b"abc\x1b7\x1b[1;1H\x1b8\x1b[?25h\x1b[0md");
        assert_eq!(text(&s, 0), "abc");
        assert_eq!(text(&s, 1), "d");
        s.feed(b"\x1b[3;1Hxyz\x1bMq");
        assert_eq!(text(&s, 1), "d q");
    }

    #[test]
    fn dec_special_graphics() {
        let mut s = Screen::new(10, 2);
        s.feed(b"\x1b(0lqk\x1b(Bq\x1b(0x");
        assert_eq!(text(&s, 0), "\u{250C}\u{2500}\u{2510}q\u{2502}");
        s.feed(b"\x1b[?1049h\x1b[?1049lq");
        assert_eq!(s.lines[0][5].ch, 'q');
    }

    #[test]
    fn shrink_drops_blank_rows_below_cursor() {
        let mut s = Screen::new(10, 5);
        s.feed(b"a\r\nb");
        s.resize(10, 3);
        assert!(s.scrollback.is_empty());
        assert_eq!((text(&s, 0), s.cy), ("a".into(), 1));
        s.feed(b"\r\nc\r\nd");
        s.resize(10, 2);
        assert_eq!(s.scrollback.len(), 2);
        assert_eq!(text(&s, 1), "d");
    }
}

#[cfg(test)]
mod replay {
    #[test]
    fn replay_capture() {
        let Ok(path) = std::env::var("REPLAY") else { return };
        let data = std::fs::read(path).unwrap();
        let mut s = super::Screen::new(107, 15);
        let i = data.windows(14).position(|w| w == b"git log --stat").unwrap_or(0);
        s.feed(&data[..i]);
        s.resize(122, 15);
        s.feed(&data[i..]);
        for (n, l) in s.lines.iter().enumerate() {
            println!("{n:2}|{}", l.iter().map(|c| c.ch).collect::<String>().trim_end());
        }
        println!("cursor {},{} scrollback {}", s.cx, s.cy, s.scrollback.len());
    }
}
