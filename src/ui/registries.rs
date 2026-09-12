//! The Registries tab's panes. Step 08 fills this in.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use super::theme::theme;
use crate::app::registries::RegistriesScreen;
use crate::app::shell::Shell;
use crate::store::Store;

pub fn render(
    frame: &mut Frame,
    _shell: &mut Shell,
    _screen: &mut RegistriesScreen,
    store: &Store,
    area: Rect,
) {
    let palette = theme();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(palette.border_type)
        .border_style(Style::default().fg(palette.border))
        .title(Line::from(" Registries ").style(Style::default().fg(palette.accent)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(format!(
            "{} registries · {} repositories",
            store.inventory.registries.len(),
            store.repositories.len()
        ))
        .style(Style::default().fg(palette.muted)),
        inner,
    );
}
