//! The terminal loop: take the terminal, drain the worker, draw, read a key,
//! give the terminal back — on every exit path, including a panic.

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser as _;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;

use crate::azure::auth::AzCli;
use crate::azure::transport::{Client, Https};
use crate::cli::{Cli, Command};
use crate::store::Store;
use crate::worker::{Request, Worker};
use crate::{cache, config, doctor, paths, ui};

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
    let mut store = match (cli.no_cache, cache::load(&cache_path)) {
        (false, Some(snapshot)) => Store::from_cache(snapshot),
        _ => Store::default(),
    };

    let client = Client::new(Box::new(AzCli), Box::new(Https::new()));
    let worker = Worker::start(config.azure.clone(), client);
    worker.send(Request::Refresh);

    let every = Duration::from_secs(config.azure.refresh.unwrap_or(DEFAULT_REFRESH));
    let mut next_refresh = (!every.is_zero()).then(|| Instant::now() + every);

    let mut terminal = ratatui::init();
    // From here on the terminal is ours, so every way out of this function —
    // an error, a panic, `q` — goes through the guard's `Drop`.
    let _restore = TerminalRestore;
    enable_terminal_input()?;

    loop {
        terminal
            .draw(|frame| ui::render(frame, &store))
            .context("failed to draw")?;

        if event::poll(if store.refreshing { SPINNING } else { RESTING })? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    let quit = matches!(key.code, KeyCode::Char('q'))
                        || (key.modifiers.contains(KeyModifiers::CONTROL)
                            && matches!(key.code, KeyCode::Char('c')));
                    if quit {
                        return Ok(());
                    }
                    if matches!(key.code, KeyCode::Char('r')) {
                        worker.send(Request::Refresh);
                    }
                }
                _ => {}
            }
        }

        // Everything the worker has said since the last frame.
        while let Some(event) = worker.try_recv() {
            let idle = matches!(event, crate::worker::Event::Idle);
            store.apply(event);
            if idle
                && !cli.no_cache
                && let Err(error) = cache::save(&cache_path, &store.snapshot())
            {
                // A cache that will not save is a slower next start, not a
                // reason to stop.
                store.problems.push((
                    String::new(),
                    format!("could not save the cache: {error:#}"),
                ));
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
