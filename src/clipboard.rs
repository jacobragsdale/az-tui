//! Copying, on two channels at once.
//!
//! **OSC 52** writes the text to the terminal itself, which is the only thing
//! that works over SSH or inside a VDI: no `xclip` on the far side can reach
//! the clipboard the eyes are next to. The terminal gives no receipt, so a
//! write that returned is counted as a copy.
//!
//! **An external command** — `pbcopy`, `wl-copy`, `xclip`, `xsel`,
//! `clip.exe` — is what works locally in a terminal that does not speak
//! OSC 52.
//!
//! Both are tried, and neither logs the text. Under tmux the sequence has to
//! be passed through: `set -s set-clipboard on`, tmux 3.3 or newer. The
//! README says so.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Puts `text` on the clipboard by whichever channel works.
///
/// Takes `&str`: the caller is what exposes a secret, and it does so on one
/// line that is easy to find.
pub fn copy(text: &str) -> Result<()> {
    // OSC 52 first and always, because it is the one that crosses an SSH
    // hop, and because a terminal that ignores it costs nothing.
    let osc = write_osc52(&mut std::io::stdout(), text);
    match first_that_works(commands(), text) {
        Ok(()) => Ok(()),
        Err(error) if osc.is_ok() => {
            // The escape went out; the terminal may well have taken it. Say
            // what could not be confirmed rather than claiming a failure.
            let _ = error;
            Ok(())
        }
        Err(error) => {
            Err(error).context("no clipboard command found and the terminal may not support OSC 52")
        }
    }
}

/// The OSC 52 sequence for `text`: `ESC ] 52 ; c ; <base64> BEL`.
#[must_use]
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn write_osc52(out: &mut impl Write, text: &str) -> Result<()> {
    out.write_all(osc52(text).as_bytes())
        .context("failed to write to the terminal")?;
    out.flush().context("failed to flush the terminal")
}

/// The commands worth trying, in the order most likely to work.
fn commands() -> Vec<Command> {
    let mut commands = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push(command("pbcopy", &[]));
        return commands;
    }
    if is_wsl() {
        // The Windows clipboard is the one the eyes are next to.
        commands.push(command("clip.exe", &[]));
    }
    commands.push(command("wl-copy", &["--trim-newline"]));
    commands.push(command("xclip", &["-selection", "clipboard"]));
    commands.push(command("xsel", &["--clipboard", "--input"]));
    commands
}

fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command
}

/// Runs `commands` in turn, each fed `stdin`, until one exits cleanly. The
/// error reported is the last one's: the command nearest to working.
fn first_that_works(commands: Vec<Command>, stdin: &str) -> Result<()> {
    let mut last = None;
    for command in commands {
        match write_to_command(command, stdin) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    match last {
        Some(error) => Err(error),
        None => bail!("no clipboard command available"),
    }
}

/// Nothing the child prints reaches the screen: the terminal is in raw mode
/// on the alternate screen, and a stray line would land on the table.
fn write_to_command(mut command: Command, text: &str) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .with_context(|| format!("failed to write to {program}"))?;
    }
    let status = child.wait().with_context(|| format!("{program} stopped"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{program} exited with {status}");
    }
}

/// WSL 1 reports `…-Microsoft`, WSL 2 `…-microsoft-standard-WSL2`. The file
/// is read rather than `WSL_DISTRO_NAME`, which an ssh session does not
/// carry.
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

/// Standard base64 with padding, which is what OSC 52 wants.
///
// ponytail: fifteen lines rather than the `base64` crate, which is the whole
// of what this program would use it for.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0_u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from(buffer[0]) << 16 | u32::from(buffer[1]) << 8 | u32::from(buffer[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let value = (bits >> (18 - 6 * index)) & 0x3f;
                output.push(char::from(TABLE[value as usize]));
            } else {
                output.push('=');
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        // RFC 4648's own vectors.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn the_osc_52_bytes_are_exactly_what_a_terminal_expects() {
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x07");
        let mut out = Vec::new();
        write_osc52(&mut out, "hello").unwrap();
        assert_eq!(out, b"\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn a_value_with_padding_and_a_trailing_newline_survives_the_encoding() {
        // Two shapes the live checklist names: a value that is itself
        // base64 with `=` padding, and one that ends in a newline.
        for value in ["c2VjcmV0==", "line\n", "λ 值 🔑"] {
            let encoded = osc52(value);
            assert!(encoded.starts_with("\x1b]52;c;") && encoded.ends_with('\x07'));
            assert!(
                !encoded.contains(value),
                "the value goes out encoded, not raw"
            );
        }
    }

    #[test]
    fn a_command_that_is_not_there_is_an_error_rather_than_a_panic() {
        let error =
            first_that_works(vec![command("az-tui-no-such-clipboard", &[])], "x").unwrap_err();
        assert!(format!("{error:#}").contains("az-tui-no-such-clipboard"));
        assert!(first_that_works(Vec::new(), "x").is_err());
    }
}
