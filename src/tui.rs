use crate::error::{CaissonError, Result};
use std::io::{self, Write};
use std::os::fd::AsRawFd;

/// Whether ANSI styling should be emitted (respects `NO_COLOR`, `TERM=dumb`, and TTY).
pub fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("TERM").as_deref(), Ok("dumb") | Ok("")) {
        return false;
    }
    is_interactive_tty()
}

/// Returns true when both stdin and stdout are attached to an interactive TTY.
pub fn is_interactive_tty() -> bool {
    unsafe { libc::isatty(libc::STDIN_FILENO) != 0 && libc::isatty(libc::STDOUT_FILENO) != 0 }
}

pub struct Palette {
    pub color: bool,
}

impl Palette {
    pub fn auto() -> Self {
        Self { color: use_color() }
    }

    pub fn dim(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn bold(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn accent(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1;36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn ok(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1;32m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn warn(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1;33m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    pub fn inv(&self, s: &str) -> String {
        if self.color {
            format!("\x1b[1;7m {s} \x1b[0m")
        } else {
            format!("[{s}]")
        }
    }
}

/// Render an old-school block gauge bar (`[██████░░░░]`).
pub fn bar_meter(fraction: f64, width: usize) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = ((clamped * width as f64).round() as usize).min(width);
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Render a crisp double-line VT220 header banner.
pub fn format_banner(badge: &str, title: &str, meta: &str, pal: &Palette) -> String {
    let inner_width = 64usize;
    let badge_plain = format!(" {badge} ");
    let left_plain = format!("  {badge_plain}  {title}");
    let right_plain = format!("{meta}  ");
    let used = left_plain.chars().count() + right_plain.chars().count();
    let pad = inner_width.saturating_sub(used).max(2);
    let box_width = left_plain.chars().count() + pad + right_plain.chars().count();
    let top = format!("  ╔{}╗", "═".repeat(box_width));
    let bot = format!("  ╚{}╝", "═".repeat(box_width));
    let mid = format!(
        "  {}  {}  {}{}{}  {}",
        pal.accent("║"),
        pal.inv(badge),
        pal.bold(title),
        " ".repeat(pad),
        pal.dim(meta),
        pal.accent("║")
    );
    format!("{}\n{mid}\n{}", pal.accent(&top), pal.accent(&bot))
}

pub fn print_step_header(step_num: usize, total: usize, title: &str, pal: &Palette) {
    println!(
        "  {}  {}",
        pal.accent("◆"),
        pal.bold(&format!("Step {step_num}/{total} · {title}"))
    );
}

pub fn print_step_done(summary: &str, pal: &Palette) {
    println!("  {}  {}", pal.ok("✓"), pal.dim(summary));
}

pub fn print_rail_line(text: &str, pal: &Palette) {
    println!("  {}  {text}", pal.dim("│"));
}

#[derive(Debug, Clone)]
pub struct SelectOption {
    pub label: String,
    pub detail: Option<String>,
    pub sub_detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToggleOption {
    pub label: String,
    pub detail: Option<String>,
    pub checked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Up,
    Down,
    Enter,
    Space,
    Escape,
    CtrlC,
    Char(char),
    Unknown,
}

struct RawModeGuard {
    fd: i32,
    original: libc::termios,
    hide_cursor: bool,
}

impl RawModeGuard {
    fn enter(hide_cursor: bool) -> Result<Self> {
        let fd = io::stdin().as_raw_fd();
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return Err(CaissonError::Io(io::Error::last_os_error()));
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(CaissonError::Io(io::Error::last_os_error()));
        }
        if hide_cursor {
            let _ = write!(io::stdout(), "\x1b[?25l");
            let _ = io::stdout().flush();
        }
        Ok(Self {
            fd,
            original: orig,
            hide_cursor,
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
        if self.hide_cursor {
            let _ = write!(io::stdout(), "\x1b[?25h");
            let _ = io::stdout().flush();
        }
    }
}

fn fd_has_byte_within(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Unbuffered single-byte read directly from `fd` (retries on `EINTR`) so `poll(2)` and `read(2)`
/// always observe the exact same kernel buffer (unlike `std::io::Stdin`, which buffers 8 KiB).
fn read_raw_byte(fd: i32) -> Result<Option<u8>> {
    let mut b = 0u8;
    loop {
        let rc = unsafe { libc::read(fd, (&mut b as *mut u8).cast::<libc::c_void>(), 1) };
        if rc == 1 {
            return Ok(Some(b));
        }
        if rc == 0 {
            return Ok(None);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(CaissonError::Io(err));
    }
}

fn read_key_from_fd(fd: i32) -> Result<Key> {
    let Some(first) = read_raw_byte(fd)? else {
        return Ok(Key::CtrlC);
    };
    match first {
        3 => Ok(Key::CtrlC),
        b'\r' | b'\n' => Ok(Key::Enter),
        b' ' => Ok(Key::Space),
        0x1b => {
            // Bounded poll so a lone Escape key returns immediately instead of blocking forever.
            if !fd_has_byte_within(fd, 25) {
                return Ok(Key::Escape);
            }
            let Some(second) = read_raw_byte(fd)? else {
                return Ok(Key::Escape);
            };
            if second == b'[' || second == b'O' {
                if !fd_has_byte_within(fd, 25) {
                    return Ok(Key::Unknown);
                }
                let Some(third) = read_raw_byte(fd)? else {
                    return Ok(Key::Unknown);
                };
                return match third {
                    b'A' => Ok(Key::Up),
                    b'B' => Ok(Key::Down),
                    // Ignore Left/Right/Home/End/PageUp/etc. rather than cancelling setup.
                    _ => Ok(Key::Unknown),
                };
            }
            Ok(Key::Unknown)
        }
        b'k' => Ok(Key::Up),
        b'j' => Ok(Key::Down),
        c if c.is_ascii() => Ok(Key::Char(c as char)),
        _ => Ok(Key::Unknown),
    }
}

fn read_key() -> Result<Key> {
    read_key_from_fd(io::stdin().as_raw_fd())
}

fn clear_rendered_lines(count: usize) {
    if count == 0 {
        return;
    }
    let mut out = io::stdout();
    let _ = write!(out, "\x1b[{count}A");
    for _ in 0..count {
        let _ = writeln!(out, "\x1b[2K");
    }
    let _ = write!(out, "\x1b[{count}A");
    let _ = out.flush();
}

/// Interactive single-choice radio picker with arrow keys (falls back to numbered input on dumb TTY).
pub fn select_one(
    prompt: &str,
    options: &[SelectOption],
    default_idx: usize,
    pal: &Palette,
) -> Result<usize> {
    if options.is_empty() {
        return Err(CaissonError::Message("no options to select".to_string()));
    }
    let mut idx = default_idx.min(options.len() - 1);

    if !pal.color || RawModeGuard::enter(false).is_err() {
        print_rail_line(prompt, pal);
        for (i, opt) in options.iter().enumerate() {
            let mark = if i == idx { "*" } else { " " };
            print_rail_line(&format!("  [{mark}] {}. {}", i + 1, opt.label), pal);
            if let Some(ref d) = opt.detail {
                print_rail_line(&format!("         {d}"), pal);
            }
        }
        print!("  │  Choose [1-{}] (default {}): ", options.len(), idx + 1);
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(idx);
        }
        let parsed: usize = trimmed
            .parse()
            .map_err(|_| CaissonError::Message(format!("invalid selection '{trimmed}'")))?;
        if parsed == 0 || parsed > options.len() {
            return Err(CaissonError::Message(format!(
                "selection {parsed} out of range 1..={}",
                options.len()
            )));
        }
        return Ok(parsed - 1);
    }

    let _guard = RawModeGuard::enter(true)?;
    let mut prev_lines = 0usize;
    loop {
        if prev_lines > 0 {
            clear_rendered_lines(prev_lines);
        }
        let mut lines = Vec::new();
        lines.push(format!("  {}  {}", pal.dim("│"), pal.dim(prompt)));
        for (i, opt) in options.iter().enumerate() {
            let sel = i == idx;
            let radio = if sel { pal.ok("(●)") } else { pal.dim("( )") };
            let label = if sel {
                pal.bold(&opt.label)
            } else {
                pal.dim(&opt.label)
            };
            lines.push(format!("  {}   {radio} {label}", pal.dim("│")));
            if let Some(ref d) = opt.detail {
                lines.push(format!("  {}       {}", pal.dim("│"), pal.dim(d)));
            }
            if let Some(ref sd) = opt.sub_detail {
                lines.push(format!("  {}       {}", pal.dim("│"), pal.dim(sd)));
            }
        }
        lines.push(format!(
            "  {}",
            pal.dim("└─ [↑↓] Move   [Enter] Select   [q] Cancel")
        ));
        for l in &lines {
            println!("{l}");
        }
        let _ = io::stdout().flush();
        prev_lines = lines.len();

        match read_key()? {
            Key::Up => {
                idx = if idx == 0 { options.len() - 1 } else { idx - 1 };
            }
            Key::Down => {
                idx = (idx + 1) % options.len();
            }
            Key::Enter => {
                clear_rendered_lines(prev_lines);
                return Ok(idx);
            }
            Key::CtrlC | Key::Escape | Key::Char('q') => {
                clear_rendered_lines(prev_lines);
                return Err(CaissonError::Message("setup cancelled by user".to_string()));
            }
            _ => {}
        }
    }
}

/// Interactive multi-select checkbox picker with arrow keys + Space + Enter.
pub fn multi_select(prompt: &str, items: &mut [ToggleOption], pal: &Palette) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    if !pal.color || RawModeGuard::enter(false).is_err() {
        print_rail_line(prompt, pal);
        for (i, item) in items.iter().enumerate() {
            let mark = if item.checked { "x" } else { " " };
            print_rail_line(&format!("  [{mark}] {}. {}", i + 1, item.label), pal);
        }
        print!("  │  Press Enter to accept defaults, or enter comma-separated numbers to toggle: ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        for tok in line.split(|c: char| c == ',' || c.is_whitespace()) {
            if let Ok(n) = tok.parse::<usize>() {
                if n >= 1 && n <= items.len() {
                    items[n - 1].checked = !items[n - 1].checked;
                }
            }
        }
        return Ok(());
    }

    let _guard = RawModeGuard::enter(true)?;
    let mut cursor = 0usize;
    let mut prev_lines = 0usize;
    loop {
        if prev_lines > 0 {
            clear_rendered_lines(prev_lines);
        }
        let mut lines = Vec::new();
        lines.push(format!("  {}  {}", pal.dim("│"), pal.dim(prompt)));
        for (i, item) in items.iter().enumerate() {
            let ptr = if i == cursor {
                pal.accent("▸")
            } else {
                " ".to_string()
            };
            let chk = if item.checked {
                pal.ok("[x]")
            } else {
                pal.dim("[ ]")
            };
            let label = if i == cursor {
                pal.bold(&item.label)
            } else {
                item.label.clone()
            };
            let det = item
                .detail
                .as_deref()
                .map(|d| format!("  {}", pal.dim(d)))
                .unwrap_or_default();
            lines.push(format!("  {}  {ptr} {chk} {label}{det}", pal.dim("│")));
        }
        lines.push(format!(
            "  {}",
            pal.dim("└─ [↑↓] Move   [Space] Toggle   [Enter] Confirm   [q] Cancel")
        ));
        for l in &lines {
            println!("{l}");
        }
        let _ = io::stdout().flush();
        prev_lines = lines.len();

        match read_key()? {
            Key::Up => {
                cursor = if cursor == 0 {
                    items.len() - 1
                } else {
                    cursor - 1
                };
            }
            Key::Down => {
                cursor = (cursor + 1) % items.len();
            }
            Key::Space => {
                items[cursor].checked = !items[cursor].checked;
            }
            Key::Enter => {
                clear_rendered_lines(prev_lines);
                return Ok(());
            }
            Key::CtrlC | Key::Escape | Key::Char('q') => {
                clear_rendered_lines(prev_lines);
                return Err(CaissonError::Message("setup cancelled by user".to_string()));
            }
            _ => {}
        }
    }
}

/// Simple `[Y/n]` or `[y/N]` confirmation prompt on the inline rail.
pub fn confirm(prompt: &str, default_yes: bool, pal: &Palette) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    print!("  {}  {prompt} {}: ", pal.dim("│"), pal.dim(hint));
    let _ = io::stdout().flush();
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line)?;
    if n == 0 {
        return Err(CaissonError::Message("setup cancelled (EOF)".to_string()));
    }
    let trimmed = line.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

/// Prompt for a single line of text with an Enter-to-accept default.
pub fn input_line(prompt: &str, default: &str, pal: &Palette) -> Result<String> {
    print!("  {}  {prompt} [{}]: ", pal.dim("│"), pal.accent(default));
    let _ = io::stdout().flush();
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line)?;
    if n == 0 {
        return Err(CaissonError::Message("setup cancelled (EOF)".to_string()));
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_meter_clamps_and_renders_exact_width() {
        assert_eq!(bar_meter(0.0, 10), "░░░░░░░░░░");
        assert_eq!(bar_meter(1.0, 10), "██████████");
        assert_eq!(bar_meter(0.5, 10), "█████░░░░░");
        assert_eq!(bar_meter(1.5, 8).chars().count(), 8);
        assert_eq!(bar_meter(-0.2, 8).chars().count(), 8);
    }

    #[test]
    fn format_banner_produces_aligned_plain_box() {
        let pal = Palette { color: false };
        let b = format_banner("CAISSON", "APFS Workspace Setup", "v0.1.7", &pal);
        let lines: Vec<&str> = b.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].chars().count(), lines[2].chars().count());
        assert!(lines[1].contains("[CAISSON]"));
        assert!(lines[1].contains("APFS Workspace Setup"));
    }

    #[test]
    fn decodes_csi_and_ss3_arrows_without_buffering_conflict_and_handles_lone_escape() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (rfd, wfd) = (fds[0], fds[1]);

        let write_bytes = |bytes: &[u8]| {
            let n = unsafe { libc::write(wfd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
            assert_eq!(n as usize, bytes.len());
        };

        write_bytes(b"\x1b[A\x1b[B\x1bOA\x1bOB\x1b[C\x1b");
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Up);
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Down);
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Up);
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Down);
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Unknown);
        // Lone Escape must return Key::Escape via bounded poll (not block forever).
        assert_eq!(read_key_from_fd(rfd).unwrap(), Key::Escape);

        unsafe {
            libc::close(rfd);
            libc::close(wfd);
        }
    }
}
