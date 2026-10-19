//! Dump a rendered frame as plain text: `cargo run -p kiln-tui --example snapshot -- dash 160 45`.
//! Screens: login setup dash run plans chat logs creds palette help confirm feedback pair.

use kiln_tui::app::{Msg, Overlay, Screen};
use kiln_tui::model::{DataEvent, PairView};
use kiln_tui::testing::{demo_app, lines};
use kiln_tui::view::render_buffer;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let what = args.get(1).map(String::as_str).unwrap_or("dash");
    let w: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(160);
    let h: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(45);
    let screen = match what {
        "login" => Screen::Login,
        "setup" => Screen::Setup,
        "run" => Screen::Run,
        "plans" | "confirm" => Screen::Plans,
        "chat" => Screen::Chat,
        "logs" => Screen::Logs,
        "creds" => Screen::Creds,
        _ => Screen::Dash,
    };
    let mut app = demo_app(screen, w, h);
    match what {
        "palette" => app.overlay = Some(Overlay::Palette { q: String::new(), i: 0 }),
        "help" => app.overlay = Some(Overlay::Help),
        "confirm" => app.overlay = Some(Overlay::Confirm),
        "feedback" => {
            app.screen = Screen::Run;
            app.overlay = Some(Overlay::Feedback { buf: String::new() })
        }
        "pair" => {
            app.update(Msg::Data(DataEvent::Pair(PairView {
                uri: "kiln://pair?url=https%3A%2F%2Fmac-mini.tailnet.ts.net&code=K7Q29XMA".into(),
                code: "K7Q2-9XMA".into(),
                expires: "12:14".into(),
            })));
        }
        _ => {}
    }
    let buf = render_buffer(&app, w, h);
    for l in lines(&buf) {
        println!("{}", l.trim_end());
    }
}
