//! The terminal loop: take the terminal, start both workers, draw, read a
//! key, drain the workers, give the terminal back — on every exit path,
//! including a panic.

use std::io::{self, Write as _};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser as _;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

use crate::app::App;
use crate::app::screen::{AppAction, Tab};
use crate::azure::auth::AzCli;
use crate::azure::transport::{Client, Https};
use crate::cli::{Cli, Command, SecretCommand};
use crate::kube::{self, Handle, Kubectl};
use crate::store::Store;
use crate::worker::{Request, Worker};
use crate::{cache, clipboard, commands, config, desktop, doctor, paths, session, ui};

/// How long a settled screen waits for a key before looking at the clock.
const RESTING: Duration = Duration::from_millis(250);
/// Seconds between background refreshes of the vaults and registries when
/// nothing says otherwise. The AKS cadence is `config.refresh`, in the
/// kube worker.
const DEFAULT_REFRESH: u64 = 300;
/// How long a layout has to stop changing before it is written. Holding `S`
/// through six columns is one save, not six.
const SETTLE: Duration = Duration::from_millis(500);
/// How often the cache is rewritten while reads keep landing. Every pod read
/// would be a file write every few seconds for nothing anyone can see.
const CACHE_EVERY: Duration = Duration::from_secs(30);

pub fn run() -> Result<()> {
    let mut cli = Cli::parse();
    let config_path = paths::config_file(cli.config.as_deref());
    // `setup` is what writes the file, so it is the one command a named file
    // that is not there yet is not a mistake for.
    let named = paths::config_named(cli.config.as_deref())
        && !matches!(cli.command, Some(Command::Setup { .. }));
    let config = cli.merge(config::load(&config_path, named)?);
    let theme = cli
        .resolve_theme(&config)
        .and_then(|choice| choice.theme(&config))
        .with_context(|| format!("resolving the theme (config: {})", config_path.display()))?;
    ui::theme::set_theme(theme);

    match cli.command.take() {
        Some(Command::Doctor) => finish(doctor::run(
            &mut io::stdout().lock(),
            &config,
            &config_path,
        )?),
        Some(Command::Setup { write }) => finish(doctor::setup(
            &mut io::stdout().lock(),
            write,
            &config_path,
            &config.azure.subscriptions,
        )?),
        Some(command) => shell(&cli, &config, command),
        None => tui(&cli, config),
    }
}

/// `Ok` when a check passed, else the shell's failure code — after what was
/// printed has landed.
fn finish(ok: bool) -> Result<()> {
    io::stdout().flush()?;
    if ok {
        Ok(())
    } else {
        std::process::exit(commands::FAILED)
    }
}

/// One subcommand, on the calling thread. Every failure exits with the code
/// the plan gives it rather than with an error up the stack, so a script can
/// tell "nothing matched" from "you asked wrong".
fn shell(cli: &Cli, config: &config::Config, command: Command) -> Result<()> {
    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    let cache_path = paths::cache_file(cli.cache.as_deref());
    let context = commands::Context {
        azure: &config.azure,
        client: &client,
        cache: (!cli.no_cache).then_some(cache_path.as_path()),
    };
    let mut out = io::stdout().lock();
    let done = match command {
        Command::Doctor | Command::Setup { .. } => unreachable!("handled above"),
        Command::Secrets {
            query,
            vaults,
            json,
            refresh,
        } => commands::secrets(&mut out, &context, query.as_deref(), &vaults, json, refresh),
        Command::Secret {
            command:
                SecretCommand::Get {
                    name,
                    vault,
                    version,
                    json,
                },
        } => commands::secret_get(
            &mut out,
            &context,
            &name,
            vault.as_deref(),
            version.as_deref(),
            json,
        ),
        Command::Repos {
            query,
            registries,
            json,
            refresh,
        } => commands::repos(
            &mut out,
            &context,
            query.as_deref(),
            &registries,
            json,
            refresh,
        ),
        Command::Tags {
            repo,
            registry,
            json,
        } => commands::tags(&mut out, &context, &repo, registry.as_deref(), json),
    };
    let flushed = out.flush().map_err(commands::Failure::from);
    // Code 0 is a reader that stopped reading, which had all it wanted.
    if let Err(failure) = done.and(flushed)
        && failure.code != 0
    {
        eprintln!("error: {}", failure.message);
        std::process::exit(failure.code);
    }
    Ok(())
}

fn tui(cli: &Cli, config: config::Config) -> Result<()> {
    let tabs = crate::app::screen::tabs(config.tabs());
    let scopes: Vec<_> = tabs
        .iter()
        .filter_map(|tab| match tab {
            Tab::Scope(tab) => Some(tab.scope.clone()),
            Tab::Secrets | Tab::Registries => None,
        })
        .collect();
    // The cache is read before the terminal is taken, so the first frame is
    // painted from it rather than after it.
    let cache_path = paths::cache_file(cli.cache.as_deref());
    let store = match (cli.no_cache, cache::load(&cache_path)) {
        (false, Some(snapshot)) => Store::from_cache(&snapshot, &tabs),
        _ => Store::new(scopes.len()),
    };

    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    // The Azure worker starts knowing whatever the cache knew, so a key
    // pressed on the first frame reaches the right host without waiting for
    // the refresh behind it.
    let azure = Worker::start(config.azure.clone(), client, store.azure.inventory.clone());
    azure.send(Request::Refresh);
    let fast = config
        .refresh
        .map_or(kube::DEFAULT_REFRESH, Duration::from_secs);
    let kube = Handle::spawn(Box::new(Kubectl), scopes, fast)?;

    let every = Duration::from_secs(config.azure.refresh.unwrap_or(DEFAULT_REFRESH));
    // An interval too far off to represent is "never", which is what a
    // person who typed it meant; `+` would panic before the first frame.
    let mut next_refresh = (!every.is_zero())
        .then(|| Instant::now().checked_add(every))
        .flatten();

    let mut app = App::new(tabs, store);
    // Before the first frame, so nothing is drawn in a layout that is about
    // to change — and so the kube worker reads the tab that will actually
    // show.
    let session_path = paths::session_file();
    app.restore(&session::Session::load(&session_path));
    if let Some(kind) = app.kind() {
        kube.send(kube::Request::Showing(app.tab, kind))?;
    }
    let mut saved = serde_json::to_string(&app.session()).unwrap_or_default();
    let mut settling: Option<Instant> = None;
    let mut cache_written = Instant::now();
    // The periodic cache write, off the loop. One at a time: a save falls
    // due while one is still writing waits for it.
    let mut cache_writing: Option<std::thread::JoinHandle<Result<()>>> = None;
    let started = Instant::now();

    let mut terminal = ratatui::init();
    // From here on the terminal is ours, so every way out of this function —
    // an error, a panic, `q` — goes through the guard's `Drop`.
    let _restore = TerminalRestore;
    enable_terminal_input()?;

    loop {
        terminal
            .draw(|frame| app.render(frame, started.elapsed().as_millis()))
            .context("failed to draw")?;

        if event::poll(app.poll_for(RESTING))? {
            let action = match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                // Bracketed paste is on, so a paste arrives whole rather than
                // as keystrokes — and would be dropped here if nothing took it.
                Event::Paste(text) => app.handle_paste(&text),
                _ => AppAction::None,
            };
            match act(&mut app, &azure, &kube, action) {
                Outcome::Quit => {
                    // The layout and the cache go with the run, timers or
                    // not. A store that has never read anything is not
                    // worth a file, and its snapshot is empty.
                    let _ = app.session().save(&session_path);
                    // A write still in flight lands first, so this one,
                    // newer, is what the file ends up holding.
                    if let Some(writing) = cache_writing.take() {
                        let _ = writing.join();
                    }
                    let snapshot = app.store.snapshot(&app.tabs);
                    if !cli.no_cache && !snapshot.tabs.is_empty() {
                        let _ = cache::save(&cache_path, &snapshot);
                    }
                    return Ok(());
                }
                // Whatever ratatui thought was on screen died with the frame
                // the shell drew over.
                Outcome::Repaint => terminal.clear().context("failed to repaint")?,
                Outcome::Continue => {}
            }
        }

        // Everything the Azure worker has said since the last frame. A
        // finished refresh is worth a cache write once it read anything.
        while let Some(event) = azure.try_recv() {
            let idle = matches!(event, crate::worker::Event::Idle);
            let action = app.apply_azure(event, Instant::now());
            act(&mut app, &azure, &kube, action);
            app.cache_dirty |= idle && app.store.azure.read_at.is_some();
        }
        // And the kube worker.
        while let Some(event) = kube.try_event() {
            if matches!(event, kube::Event::Stopped) {
                app.shell
                    .set_error("the cluster worker stopped; restart az-tui");
            }
            let action = app.apply_kube(event);
            act(&mut app, &azure, &kube, action);
        }

        // What has run out, what the pane should be following now, and what
        // the cursor has settled on long enough to be worth asking about.
        for action in app.tick(Instant::now()) {
            act(&mut app, &azure, &kube, action);
        }

        if let Some(writing) = cache_writing.take_if(|writing| writing.is_finished())
            && let Ok(Err(error)) = writing.join()
        {
            // A cache that will not save is a slower next start, not a
            // reason to stop.
            app.shell
                .set_error(format!("could not save the cache: {error:#}"));
        }
        if app.cache_dirty
            && !cli.no_cache
            && cache_written.elapsed() >= CACHE_EVERY
            && cache_writing.is_none()
        {
            cache_written = Instant::now();
            app.cache_dirty = false;
            // The rows are copied here; serialising and writing them, which
            // is the slow half, happens on its own thread.
            let snapshot = app.store.snapshot(&app.tabs);
            let path = cache_path.clone();
            cache_writing = Some(std::thread::spawn(move || cache::save(&path, &snapshot)));
        }

        // The layout, once it has stopped moving.
        let now = serde_json::to_string(&app.session()).unwrap_or_default();
        if now == saved {
            settling = None;
        } else {
            let due = *settling.get_or_insert_with(|| Instant::now() + SETTLE);
            if Instant::now() >= due {
                settling = None;
                saved = now;
                if let Err(error) = app.session().save(&session_path) {
                    // A layout that will not save is worth saying once.
                    app.shell
                        .set_error(format!("could not save the session: {error:#}"));
                }
            }
        }

        if let Some(due) = next_refresh
            && Instant::now() >= due
        {
            azure.send(Request::Refresh);
            next_refresh = Instant::now().checked_add(every);
        }
    }
}

/// What the loop does after an action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Continue,
    /// Something else drew on the terminal; the next frame is a full one.
    Repaint,
    Quit,
}

/// Does what a screen asked for.
///
/// The clipboard is the one place a value leaves the program, and the status
/// it sets names only what was copied — never what it was.
fn act(app: &mut App, azure: &Worker, kube: &Handle, action: AppAction) -> Outcome {
    match action {
        AppAction::Quit => return Outcome::Quit,
        AppAction::Azure(request) => azure.send(request),
        AppAction::Kube(request) => {
            if let Err(error) = kube.send(request) {
                app.shell.set_error(format!("{error:#}"));
            }
        }
        AppAction::Exec {
            context,
            namespace,
            pod,
            container,
        } => {
            // bash when the image has it, sh when it does not, in this
            // terminal, with the TUI out of the way until the shell exits.
            let mut command = std::process::Command::new("kubectl");
            command.args(["--context", &context, "exec", "-it", "-n", &namespace, &pod]);
            if let Some(container) = &container {
                command.args(["-c", container]);
            }
            command.args([
                "--",
                "sh",
                "-c",
                "command -v bash >/dev/null 2>&1 && exec bash || exec sh",
            ]);
            let status = released_terminal(|| {
                command
                    .stdin(Stdio::inherit())
                    .stdout(Stdio::inherit())
                    .stderr(Stdio::inherit())
                    .status()
            });
            match status {
                Ok(status) if status.success() => {}
                Ok(status) => app
                    .shell
                    .set_error(format!("kubectl exec on {pod} exited with {status}")),
                Err(error) => app
                    .shell
                    .set_error(format!("kubectl could not be run: {error}")),
            }
            return Outcome::Repaint;
        }
        AppAction::Copy { text, label } => match clipboard::copy(&text) {
            Ok(clipboard::Channel::Command) => app.shell.set_status(label),
            // The escape went out and nothing confirmed it; a terminal that
            // does not speak it has dropped the text, so say which it was.
            Ok(clipboard::Channel::Terminal) => app
                .shell
                .set_status(format!("{label} · sent to the terminal (OSC 52)")),
            Err(error) => app.shell.set_error(format!("{error:#}")),
        },
        AppAction::OpenUrl(url) => {
            if let Err(error) = desktop::open_in_browser(&url) {
                app.shell.set_error(format!("{error:#}"));
            }
        }
        AppAction::None => {}
    }
    Outcome::Continue
}

/// Runs `body` with the terminal handed back to the shell, and takes it back
/// however `body` went. The caller repaints.
fn released_terminal<T>(body: impl FnOnce() -> T) -> T {
    release_terminal();
    let outcome = body();
    if let Err(error) = claim_terminal() {
        // Nothing can be reported through a TUI that is not there, so this
        // goes where the shell's own output went.
        eprintln!("az-tui could not take the terminal back: {error:#}");
    }
    outcome
}

/// Puts the terminal back the way the TUI found it: the input features, then
/// raw mode and the alternate screen. The end of a run and the shell hand-off
/// both leave this way.
fn release_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
}

/// Takes the terminal back after [`release_terminal`] gave it away, in the
/// same order `ratatui::init` and the TUI's own startup take it.
fn claim_terminal() -> Result<()> {
    enable_raw_mode().context("failed to take raw mode back")?;
    execute!(io::stdout(), EnterAlternateScreen).context("failed to take the screen back")?;
    enable_terminal_input()
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        // Best effort: the run is over either way, and a terminal that
        // refuses one of these is not something the exit can fix.
        release_terminal();
    }
}

/// The input the TUI reads beyond the keyboard. Turned on here so no later
/// step has to remember to.
fn enable_terminal_input() -> Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)
        .context("failed to enable terminal input features")
}
