//! The terminal loop: take the terminal, drain the worker, draw, read a key,
//! give the terminal back — on every exit path, including a panic.

use std::io;
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
use crate::cli::{Cli, Command};
use crate::store::Store;
use crate::worker::{Request, Worker};
use crate::{cache, clipboard, config, desktop, doctor, paths, ui};

/// How often the spinner turns while a refresh is running.
const SPINNING: Duration = Duration::from_millis(100);
/// How long a settled screen waits for a key before looking at the clock.
const RESTING: Duration = Duration::from_millis(250);
/// Seconds between background refreshes when nothing says otherwise.
const DEFAULT_REFRESH: u64 = 300;

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let config_path = paths::config_file(cli.config.as_deref());
    let config = cli.merge(config::load(&config_path)?);
    let (theme, _label) = cli
        .resolve_theme(&config)
        .and_then(|choice| choice.theme(&config))
        .with_context(|| format!("resolving the theme (config: {})", config_path.display()))?;
    ui::theme::set_theme(theme);

    match cli.command {
        Some(Command::Doctor) => {
            if doctor::run(&config.azure)? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        None => tui(&cli, config),
    }
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
    let worker = Worker::start(config.azure.clone(), client);
    worker.send(Request::Refresh);

    let every = Duration::from_secs(config.azure.refresh.unwrap_or(DEFAULT_REFRESH));
    let mut next_refresh = (!every.is_zero()).then(|| Instant::now() + every);

    let mut app = App::new(store);
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

        if event::poll(if app.store.refreshing {
            SPINNING
        } else {
            RESTING
        })? {
            let action = match event::read()? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => AppAction::None,
            };
            match action {
                AppAction::Quit => return Ok(()),
                AppAction::Send(request) => worker.send(request),
                AppAction::Copy { text, label } => match clipboard::copy(&text) {
                    // `text` is never in a message: the label says what was
                    // copied, and nothing says what it was.
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
        }

        // Everything the worker has said since the last frame.
        while let Some(event) = worker.try_recv() {
            let idle = matches!(event, crate::worker::Event::Idle);
            app.apply(event);
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

        if let Some(due) = next_refresh
            && Instant::now() >= due
        {
            worker.send(Request::Refresh);
            next_refresh = Some(Instant::now() + every);
        }
    }
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
