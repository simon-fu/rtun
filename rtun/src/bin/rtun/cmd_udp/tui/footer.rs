/*
TODO:
    - 当 app 行数变少时，会导致 UI 混乱
*/

use anyhow::Result;
use bytes::Bytes;
use console::{Term, Key};
use dialoguer::theme::ColorfulTheme;
use tokio::sync::mpsc;

use super::{history::InputHistory, line_input::{Input, InputState}, term_theme_renderer::TermThemeRenderer};

pub struct FooterInput {
    event_rx: mpsc::Receiver<Event>,
}

impl FooterInput {
    pub fn new(event_rx: mpsc::Receiver<Event>) -> Self {
        Self { event_rx }
    }

    pub fn run_app<A: FooterApp>(self, app: A) -> Result<()> {
        let mut event_rx = self.event_rx;

        let mut history = InputHistory::new().max_entries(8).skip_last_duplicate(true);    
        let theme = ColorfulTheme::default();
        let mut input = Input::<String>::with_theme(&theme);
        input
        .with_prompt("udp")
        .history_with(&mut history);
    
        let mut state = DrawState {
            app_lines: 0,
            last_line_len: 0,
            last_line_extra_y: 0,
            input: InputState::default(),
            app,
            exited: false,
        };

        let term = Term::stdout();
        let mut render = TermThemeRenderer::new(&term, &theme);


        state.app_lines = state.app.on_paint(&term)?;
        input.init_state(&mut state.input, &term, &mut render)?;
    
        loop {
            let r = event_rx.blocking_recv();
            match r {
                Some(Event::Key(key)) => {
                    let init_input = state.process_key(key, &term, &mut input, &mut render)?;
                    if init_input {
                        render = TermThemeRenderer::new(&term, &theme);
                    }
                },
                Some(Event::Log(bytes)) => {
                    state.process_log(bytes, &term, &mut input, &mut render)?;
                },
                Some(Event::PaintApp) => {
                    state.repaint_app(&term, &mut input, &mut render)?;
                }
                Some(Event::Exit) => {
                    break;
                }
                None => break,
            }
        }
    
        Ok(())
    }
}

struct DrawState<A> {
    app_lines: usize,
    last_line_len: usize,
    last_line_extra_y: usize,
    input: InputState,
    app: A,
    exited: bool,
}

impl<A: FooterApp> DrawState<A> {
    fn process_key(&mut self, key: Key, term: &Term, input: &mut Input<String>, render: &mut TermThemeRenderer<'_> ) -> Result<bool> {
        let mut init_input = false;
        if key == Key::Enter {
            if self.input.num_chars() > 0 {
                term.move_cursor_up(self.app_lines)?;
                
                // input.restore_state(&mut self.input, term, render)?;
                let r = input.complete_state(&mut self.input, term, render)?;
                if let Some(s) = r {
                    let action = self.app.on_input(s)?;
                    match action {
                        Action::None => {},
                        Action::Exit => {
                            self.exited = true;
                            return Ok(false)
                        },
                    }

                    self.app_lines = self.app.on_paint(term)?;
                    input.init_state(&mut self.input, term, render)?;
                    init_input = true;
                    // render = TermThemeRenderer::new(&term, &theme); // aaa
                    // last_line_len = 0;
                }

                if self.last_line_len > 0 {
                    self.last_line_extra_y += 1;
                }
            }
        } else {
            input.interact_text_on(&mut self.input, term, key)?;
        }

        Ok(init_input)
    }

    fn process_log(&mut self, mut bytes: Bytes, term: &Term, input: &mut Input<String>, render: &mut TermThemeRenderer<'_>) -> Result<()> {
        if bytes.len() == 0 {
            return Ok(())
        }
        
        if self.last_line_len > 0 {
            self.paint_last(&mut bytes, term)?;
        } 

        term.move_cursor_up(self.app_lines)?;
        term.clear_line()?;

        if bytes.len() > 0 {
            while let Some(line) = bytes.next_line() {
                let s = std::str::from_utf8(&line)?;
                term.write_line(s)?;
                term.clear_line()?;
            }

            if bytes.len() > 0 {
                let s = std::str::from_utf8(&bytes)?;
                term.write_str(s)?;
                self.last_line_len += s.len();
                term.move_cursor_down(1)?;
            }
        }

        self.paint_app(term, input, render)?;

        Ok(())
    }

    fn paint_last(&mut self, bytes: &mut Bytes, term: &Term) -> Result<()> {
        let (h, w) = term.size();
        let up_lines = self.app_lines + self.last_line_extra_y + 1;

        if up_lines > h as usize {
            // last line already out of view, simply assume done
            self.last_line_extra_y = 0;
            self.last_line_len = 0;
            return Ok(())
        }

        term.move_cursor_up(up_lines)?;
        term.move_cursor_left(w as usize)?;
        term.move_cursor_right(self.last_line_len)?;

        if let Some(line) = bytes.next_line() {
            let s = std::str::from_utf8(&line)?;
            term.write_line(s)?;
            term.move_cursor_down(up_lines-1)?;
            self.last_line_extra_y = 0;
            self.last_line_len = 0;
        } else {
            let s = std::str::from_utf8(&bytes)?;
            term.write_line(s)?;
            bytes.clear();
            term.move_cursor_down(up_lines)?;
        }

        Ok(())
    }

    fn repaint_app(&mut self, term: &Term, input: &mut Input<String>, render: &mut TermThemeRenderer<'_>) -> Result<()> {
        term.move_cursor_up(self.app_lines)?;
        term.clear_line()?;
        self.paint_app(term, input, render)
    }

    fn paint_app(&mut self, term: &Term, input: &mut Input<String>, render: &mut TermThemeRenderer<'_>) -> Result<()> {
        self.app_lines = self.app.on_paint(term)?;
        term.clear_line()?;
        input.restore_state(&mut self.input, term, render)?;
        Ok(())
    }
}

trait NextLine {
    fn next_line(&mut self) -> Option<Bytes>;
}

impl NextLine for Bytes {
    fn next_line(&mut self) -> Option<Bytes> {
        let data = self.as_ref();
        let r = data.iter().position(|x|*x == b'\r' || *x == b'\n');
        match r {
            Some(n) => {
                let line = self.split_to(n);
                let r = self.as_ref().iter().position(|x|*x != b'\r' && *x != b'\n');
                match r {
                    Some(n) => { let _r = self.split_to(n); },
                    None => { self.clear(); },
                }
                Some(line)
            },
            None => None,
        }
    }
}

pub enum Event {
    Log(Bytes),
    Key(Key),
    PaintApp,
    Exit,
}

pub trait FooterApp {
    fn on_paint(&mut self, term: &Term) -> Result<usize>;
    fn on_input(&mut self, input: String) -> Result<Action>;
}

pub enum Action {
    None,
    Exit,
}
