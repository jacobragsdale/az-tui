//! Opening a URL in whatever browser the desktop has.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// Opens `url`, if it is one worth opening.
///
/// Only HTTPS goes out. Every URL this program builds is one it built itself
/// from a resource id, but a `file:` or `javascript:` one is not a portal
/// blade and handing it to the system launcher would be handing over more
/// than a link.
pub fn open_in_browser(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("only HTTPS links are opened");
    }
    let mut last = None;
    for command in browser_commands(url, is_wsl()) {
        match launch(command, LAUNCH_WAIT) {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no browser launcher available")))
        .context("could not open a browser")
}

/// How long a launcher gets to say it failed. `xdg-open` can run the
/// browser in the foreground and only return when it closes, and this is the
/// UI thread.
const LAUNCH_WAIT: Duration = Duration::from_secs(2);

/// Starts one launcher and waits up to `wait` for it. An early failure is an
/// error, so the next launcher is tried; one still running at the deadline
/// has opened something, and is reaped on a thread of its own.
fn launch(mut command: Command, wait: Duration) -> Result<()> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {program}"))?;
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match child
            .try_wait()
            .with_context(|| format!("{program} could not be waited for"))?
        {
            Some(status) if status.success() => return Ok(()),
            Some(status) => bail!("{program} exited with {status}"),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// The launchers to try, in order.
///
/// Under WSL the URL goes to Windows, whose default browser is the one the
/// user means; `xdg-open` there is usually missing and, with no desktop
/// behind it, can exec a terminal browser onto our screen, so it comes last.
/// `cmd.exe` gets the URL bare with its operators escaped — `start "…"` reads
/// a quoted first argument as a window title — and PowerShell gets it
/// single-quoted, where only `'` is special.
#[must_use]
pub fn browser_commands(url: &str, wsl: bool) -> Vec<Command> {
    let mut commands = Vec::new();
    if wsl {
        commands.push(command("cmd.exe", &["/c", "start", &cmd_escape(url)]));
        commands.push(command(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Process -FilePath '{}'", url.replace('\'', "''")),
            ],
        ));
    }
    let launcher = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    commands.push(command(launcher, &[url]));
    commands
}

pub(crate) fn command(program: &str, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command.args(args);
    command
}

/// What `cmd.exe` would read as an operator gets its caret: `&` ends a
/// command, `|` pipes it, `<` and `>` redirect, and `^` is the escape itself.
/// A portal URL carries `#` and can carry `&`, so this is not a corner.
#[must_use]
pub fn cmd_escape(url: &str) -> String {
    url.chars()
        .fold(String::with_capacity(url.len()), |mut out, character| {
            if "^&|<>".contains(character) {
                out.push('^');
            }
            out.push(character);
            out
        })
}

/// WSL 1 reports `…-Microsoft`, WSL 2 `…-microsoft-standard-WSL2`. The file
/// is read rather than `WSL_DISTRO_NAME`, which an ssh session does not
/// carry.
pub(crate) fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_is_handed_to_the_desktop() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://portal.azure.com",
            "",
        ] {
            let error = open_in_browser(url).unwrap_err();
            assert!(format!("{error:#}").contains("only HTTPS"), "{url}");
        }
    }

    #[test]
    fn wsl_tries_windows_first_and_the_linux_launcher_last() {
        let url = "https://portal.azure.com/#@/resource/subscriptions/s/x";
        let programs: Vec<String> = browser_commands(url, true)
            .iter()
            .map(|command| command.get_program().to_string_lossy().into_owned())
            .collect();
        assert_eq!(programs.first().unwrap(), "cmd.exe");
        assert_eq!(
            programs.last().unwrap(),
            if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            }
        );

        let programs: Vec<String> = browser_commands(url, false)
            .iter()
            .map(|command| command.get_program().to_string_lossy().into_owned())
            .collect();
        assert_eq!(programs.len(), 1, "nothing Windows-shaped off WSL");
    }

    #[cfg(unix)]
    #[test]
    fn a_launcher_that_fails_early_is_an_error_and_one_still_running_has_opened() {
        let error = launch(command("sh", &["-c", "exit 3"]), Duration::from_secs(5)).unwrap_err();
        assert!(format!("{error:#}").contains("exited with"), "{error:#}");

        let started = Instant::now();
        launch(command("sleep", &["5"]), Duration::from_millis(200)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "not waited for: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_cmd_operator_gets_its_caret() {
        assert_eq!(cmd_escape("https://x/?a=1&b=2"), "https://x/?a=1^&b=2");
        assert_eq!(cmd_escape("a|b<c>d^e"), "a^|b^<c^>d^^e");
        assert_eq!(cmd_escape("https://plain"), "https://plain");
    }
}
