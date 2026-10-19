//! Terminal event loop: crossterm events + 120 ms tick + data-source events
//! → `App::update` → `view::render`.

use crate::app::{App, Msg};
use crate::model::{Cmd, DataEvent};
use crate::source::DataSource;
use crate::theme::{self, ColorMode};
use crate::view;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event, EventStream};
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;

pub const TICK: Duration = Duration::from_millis(120);

fn dispatch(app: &mut App, source: &mut dyn DataSource, msg: Msg) {
    for cmd in app.update(msg) {
        let quit = cmd == Cmd::Quit;
        source.handle(cmd);
        if quit {
            app.quit = true;
        }
    }
}

/// Run the TUI until the user quits.
pub async fn run(
    mut app: App,
    mut source: Box<dyn DataSource>,
    mut rx: UnboundedReceiver<DataEvent>,
) -> anyhow::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut app, source.as_mut(), &mut rx, &mut terminal).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(
    app: &mut App,
    source: &mut dyn DataSource,
    rx: &mut UnboundedReceiver<DataEvent>,
    terminal: &mut ratatui::DefaultTerminal,
) -> anyhow::Result<()> {
    let color = ColorMode::detect();
    let size = terminal.size()?;
    app.update(Msg::Resize(size.width, size.height));
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        terminal.draw(|f| {
            view::render(app, f);
            if color == ColorMode::Ansi256 {
                theme::downgrade(f.buffer_mut());
            }
        })?;
        let msg = tokio::select! {
            _ = tick.tick() => Msg::Tick,
            Some(ev) = rx.recv() => Msg::Data(ev),
            ev = events.next() => match ev {
                Some(Ok(Event::Key(k))) => Msg::Key(k),
                Some(Ok(Event::Resize(w, h))) => Msg::Resize(w, h),
                Some(Ok(Event::Paste(s))) => Msg::Paste(s),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
        };
        dispatch(app, source, msg);
        // Apply any queued data events before the next frame.
        while let Ok(ev) = rx.try_recv() {
            dispatch(app, source, Msg::Data(ev));
        }
        if app.quit {
            return Ok(());
        }
    }
}
