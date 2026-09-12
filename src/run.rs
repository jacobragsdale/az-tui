//! The terminal loop: take the terminal, drain the worker, draw, read a key,
//! give the terminal back — on every exit path, including a panic.

use std::io::{self, Write as _};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser as _;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;

use crate::app::App;
use crate::app::screen::AppAction;
use crate::azure::auth::AzCli;
use crate::azure::transport::{Client, Https};
use crate::cli::{Cli, Command, SecretCommand};
use crate::store::Store;
use crate::worker::{Request, Worker};
use crate::{cache, clipboard, commands, config, desktop, doctor, paths, session, ui};

/// How long a settled screen waits for a key before looking at the clock.
const RESTING: Duration = Duration::from_millis(250);
/// Seconds between background refreshes when nothing says otherwise.
const DEFAULT_REFRESH: u64 = 300;
/// How long a layout has to stop changing before it is written. Holding `s`
/// through six columns is one save, not six.
const SETTLE: Duration = Duration::from_millis(500);

pub fn run() -> Result<()> {
    let mut cli = Cli::parse();
    let config_path = paths::config_file(cli.config.as_deref());
    let config = cli.merge(config::load(&config_path)?);
    let (theme, _label) = cli
        .resolve_theme(&config)
        .and_then(|choice| choice.theme(&config))
        .with_context(|| format!("resolving the theme (config: {})", config_path.display()))?;
    ui::theme::set_theme(theme);

    match cli.command.take() {
        Some(Command::Doctor) => {
            if doctor::run(&config.azure)? {
                Ok(())
            } else {
                std::process::exit(commands::FAILED)
            }
        }
        Some(command) => shell(&cli, &config, command),
        None => tui(&cli, config),
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
        Command::Doctor => unreachable!("handled above"),
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
    out.flush()?;
    if let Err(failure) = done {
        eprintln!("error: {}", failure.message);
        std::process::exit(failure.code);
    }
    Ok(())
}

fn tui(cli: &Cli, config: config::Config) -> Result<()> {
    // The cache is read before the terminal is taken, so the first frame is
    // painted from it rather than after it.
    let cache_path = paths::cache_file(cli.cache.as_deref());
    let store = match (cli.no_cache, cache::load(&cache_path)) {
        (false, Some(snapshot)) => Store::from_cache(snapshot),
        _ => Store::default(),
    };

    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    // The worker starts knowing whatever the cache knew, so a key pressed on
    // the first frame reaches the right host without waiting for the refresh
    // behind it.
    let worker = Worker::start(config.azure.clone(), client, store.inventory.clone());
    worker.send(Request::Refresh);

    let every = Duration::from_secs(config.azure.refresh.unwrap_or(DEFAULT_REFRESH));
    let mut next_refresh = (!every.is_zero()).then(|| Instant::now() + every);

    let mut app = App::new(store);
    // Before the first frame, so nothing is drawn in a layout that is about
    // to change.
    let session_path = paths::session_file();
    app.restore(&session::Session::load(&session_path));
    let mut saved = serde_json::to_string(&app.session()).unwrap_or_default();
    let mut settling: Option<Instant> = None;
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
                _ => AppAction::None,
            };
            if act(&mut app, &worker, action) {
                // The layout goes with the run, settle timer or not.
                let _ = app.session().save(&session_path);
                return Ok(());
            }
        }

        // Everything the worker has said since the last frame.
        while let Some(event) = worker.try_recv() {
            let idle = matches!(event, crate::worker::Event::Idle);
            let action = app.apply(event, Instant::now());
            if act(&mut app, &worker, action) {
                return Ok(());
            }
            if idle
                && !cli.no_cache
                && let Err(error) = cache::save(&cache_path, &app.store.snapshot())
            {
                // A cache that will not save is a slower next start, not a
                // reason to stop.
                app.shell
                    .set_error(format!("could not save the cache: {error:#}"));
            }
        }

        // What has run out, and what the cursor has settled on long enough
        // to be worth asking about.
        if let Some(request) = app.tick(Instant::now()) {
            worker.send(request);
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
            worker.send(Request::Refresh);
            next_refresh = Some(Instant::now() + every);
        }
    }
}

/// Does what a screen asked for. Returns true when the run is over.
///
/// The clipboard is the one place a value leaves the program, and the status
/// it sets names only what was copied — never what it was.
fn act(app: &mut App, worker: &Worker, action: AppAction) -> bool {
    match action {
        AppAction::Quit => return true,
        AppAction::Send(request) => worker.send(request),
        AppAction::Copy { text, label } => match clipboard::copy(&text) {
            Ok(()) => app.shell.set_status(label),
            Err(error) => app.shell.set_error(format!("{error:#}")),
        },
        AppAction::OpenUrl(url) => {
            if let Err(error) = desktop::open_in_browser(&url) {
                app.shell.set_error(format!("{error:#}"));
            }
        }
        AppAction::None => {}
    }
    false
}

struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        // Best effort: the run is over either way, and a terminal that
        // refuses one of these is not something the exit can fix.
        let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
        ratatui::restore();
    }
}

/// The input the TUI reads beyond the keyboard. Turned on here so no later
/// step has to remember to.
fn enable_terminal_input() -> Result<()> {
    execute!(io::stdout(), EnableMouseCapture, EnableBracketedPaste)
        .context("failed to enable terminal input features")
}
