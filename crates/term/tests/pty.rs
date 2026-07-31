//! Proof that there is a real shell on the other end.
//!
//! Everything in the unit tests is a pure function. This is the part that can
//! only be checked by actually forking a process: that `spawn` produces a live
//! PTY, that keystrokes reach it, that its output comes back through the grid,
//! and that a resize does not take the whole thing down.

use std::time::{Duration, Instant};

use zemacs_term::{Input, Terminal};

const FG: [u8; 3] = [200, 200, 200];
const BG: [u8; 3] = [0, 0, 0];

/// Drive the terminal until its screen satisfies `pred`, or give up.
///
/// Polling is not incidental — `poll` is what answers the child's queries, so a
/// wait loop that did not call it could hang the shell it is waiting for.
fn wait_for(term: &mut Terminal, what: &str, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        term.poll();
        let text = term.screen(FG, BG).to_text();
        if pred(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; screen was:\n{text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_shell_runs_and_its_output_reaches_the_grid() {
    let mut term = Terminal::spawn(80, 24, std::env::temp_dir().into()).expect("spawn a pty");

    // Wait for the shell to be up before typing at it, or the keystrokes race
    // the prompt and get eaten.
    wait_for(&mut term, "a prompt", |text| !text.trim().is_empty());

    // Deliberately arithmetic: the shell echoes what is typed, so a command
    // whose output is a substring of its input would pass without ever running.
    // "42" appears only if something executed.
    term.input(Input::Char('e'));
    for c in "xpr 21 + 21".chars() {
        term.input(Input::Char(c));
    }
    term.input(Input::Enter);

    let screen = wait_for(&mut term, "the answer", |text| {
        text.lines().any(|l| l.trim() == "42")
    });
    assert!(
        screen.contains("expr 21 + 21"),
        "the shell should have echoed the command; got:\n{screen}"
    );

    // The grid keeps its shape, and a resize does not bring anything down.
    assert_eq!(term.size(), (80, 24));
    term.resize(100, 30);
    assert_eq!(term.size(), (100, 30));
    let after = wait_for(&mut term, "the grid to follow the resize", |_| true);
    let _ = after;
    assert_eq!(term.screen(FG, BG).cols, 100);
    assert_eq!(term.screen(FG, BG).rows, 30);

    // A resize to the size it already has must not be reported as a change.
    term.resize(100, 30);
    assert_eq!(term.size(), (100, 30));

    // Default colours come from the editor, so a terminal pane sits inside the
    // theme rather than being a black rectangle in the middle of it.
    let screen = term.screen(FG, BG);
    let blank = screen.cell(screen.rows - 1, screen.cols - 1).expect("a cell");
    assert_eq!(blank.bg, BG);
    assert_eq!(blank.fg, FG);

    // The cursor is somewhere on screen, since nothing has hidden it.
    let (row, col) = screen.cursor.expect("a visible cursor");
    assert!(row < screen.rows && col < screen.cols);

    // The scrollback is what the editor navigates once the shell gives the
    // keyboard back. Without it, stepping out of a terminal leaves the motions
    // with one screenful and no history to search or yank from — which is
    // exactly what made every vim key look broken in here.
    let history = term.history_text();
    assert!(
        history.contains("expr 21 + 21") && history.lines().any(|l| l.trim() == "42"),
        "the scrollback should hold what scrolled past; got:\n{history}"
    );
    assert!(
        !history.ends_with('\n'),
        "trailing blank rows are not history, and `G` landing on them looks \
         like the buffer lost its contents"
    );
}

#[test]
fn exiting_the_shell_is_noticed() {
    let mut term = Terminal::spawn(40, 10, None).expect("spawn a pty");
    wait_for(&mut term, "a prompt", |text| !text.trim().is_empty());

    assert!(!term.exited(), "the shell should still be running");
    for c in "exit".chars() {
        term.input(Input::Char(c));
    }
    term.input(Input::Enter);

    let deadline = Instant::now() + Duration::from_secs(20);
    while !term.exited() {
        term.poll();
        assert!(Instant::now() < deadline, "the shell never reported exiting");
        std::thread::sleep(Duration::from_millis(20));
    }
}
