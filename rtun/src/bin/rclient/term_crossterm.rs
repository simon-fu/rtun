use std::io::Write;

use anyhow::{Result, Context, anyhow};
use futures::StreamExt;
use rtun::channel::{ChSender, ChReceiver};


use crossterm::{
    cursor::position,
    event::{Event, EventStream, KeyCode},
    terminal::{disable_raw_mode, enable_raw_mode},
};

pub async fn run(tx: ChSender, rx: ChReceiver) -> Result<()> {
    enable_raw_mode()?;
    let result = do_run(tx, rx).await;
    disable_raw_mode()?;
    result
}

async fn do_run(tx: ChSender, mut rx: ChReceiver)-> Result<()> {
    let mut reader = EventStream::new();
    loop {
        tokio::select! {
            r = reader.next() => {
                let ev = match r {
                    Some(r) => r?,
                    None => break,
                };

                tracing::debug!("got event {ev:?}");

                if ev == Event::Key(KeyCode::Esc.into()) {
                    break;
                }

                if let Event::Key(key) = ev {
                    if let Some(s) = encode_key_event(&key) {
                        tx.send_data(s.into_bytes().into()).await.map_err(|_e|anyhow!("send_data fail"))?; 
                    }
                }
            },
            r = rx.recv_data() => {
                let d = r.with_context(||"recv stream failed")?;
                // use rtun::hex::BinStrLine;
                // tracing::debug!("{}", d.bin_str());
                let stdout = std::io::stdout();
                let mut stdout = stdout.lock();
                stdout.write_all(&d[..]).unwrap();
                stdout.flush().unwrap();
            }
        }
    }
    Ok(())
}

fn encode_key_event(src: &crossterm::event::KeyEvent) -> Option<String> {
    match map_key_event(src) {
        Some(dst) => {
            dst.key.encode(
                dst.modifiers, 
                termwiz::input::KeyCodeEncodeModes {
                    encoding: termwiz::input::KeyboardEncoding::Xterm,
                    application_cursor_keys: false,
                    newline_mode: false,
                    modify_other_keys: None,
                }, 
                src.kind == crossterm::event::KeyEventKind::Press,
            ).ok()
        },
        None => None,
    }
}

fn map_key_event(src: &crossterm::event::KeyEvent) -> Option<termwiz::input::KeyEvent> {
    use crossterm::event::KeyModifiers as SrcModifiers;
    use termwiz::input::KeyEvent as DstKeyEvent;
    use termwiz::input::Modifiers as DstModifiers;

    let dst_code = map_key_code(src.code)?;

    let mut dst_modifiers = DstModifiers::empty();

    if src.modifiers.contains(SrcModifiers::SHIFT) {
        dst_modifiers.set(DstModifiers::SHIFT, true);
    }

    if src.modifiers.contains(SrcModifiers::CONTROL) {
        dst_modifiers.set(DstModifiers::CTRL, true);
    }

    if src.modifiers.contains(SrcModifiers::ALT) {
        dst_modifiers.set(DstModifiers::ALT, true);
    }

    if src.modifiers.contains(SrcModifiers::SUPER) {
        dst_modifiers.set(DstModifiers::SUPER, true);
    }

    // if src.modifiers.contains(SrcModifiers::HYPER) {
    //     dst_modifiers.set(DstModifiers::HYPER, true);
    // }

    // if src.modifiers.contains(SrcModifiers::META) {
    //     dst_modifiers.set(DstModifiers::META, true);
    // }


    let dst = DstKeyEvent {
        key: dst_code,
        modifiers: dst_modifiers,
    };

    Some(dst)

}

fn map_key_code(src: crossterm::event::KeyCode) -> Option<termwiz::input::KeyCode> {
    use crossterm::event::KeyCode as SrcKeyCode;
    use crossterm::event::MediaKeyCode as SrcMedia;
    use termwiz::input::KeyCode as DstKeyCode;

    let dst_code = match src {
        SrcKeyCode::Backspace => DstKeyCode::Backspace,
        SrcKeyCode::Enter => DstKeyCode::Enter,
        SrcKeyCode::Left => DstKeyCode::LeftArrow,
        SrcKeyCode::Right => DstKeyCode::RightArrow,
        SrcKeyCode::Up => DstKeyCode::UpArrow,
        SrcKeyCode::Down => DstKeyCode::DownArrow,
        SrcKeyCode::Home => DstKeyCode::Home,
        SrcKeyCode::End => DstKeyCode::End,
        SrcKeyCode::PageUp => DstKeyCode::PageUp,
        SrcKeyCode::PageDown => DstKeyCode::PageDown,
        SrcKeyCode::Tab => DstKeyCode::Tab,
        SrcKeyCode::BackTab => return None,
        SrcKeyCode::Delete => DstKeyCode::Delete,
        SrcKeyCode::Insert => DstKeyCode::Insert,
        SrcKeyCode::F(v) => DstKeyCode::Function(v),
        SrcKeyCode::Char(c) => DstKeyCode::Char(c),
        SrcKeyCode::Null => return None,
        SrcKeyCode::Esc => DstKeyCode::Escape,
        SrcKeyCode::CapsLock => DstKeyCode::CapsLock,
        SrcKeyCode::ScrollLock => DstKeyCode::ScrollLock,
        SrcKeyCode::NumLock => DstKeyCode::NumLock,
        SrcKeyCode::PrintScreen => DstKeyCode::PrintScreen,
        SrcKeyCode::Pause => DstKeyCode::Pause,
        SrcKeyCode::Menu => DstKeyCode::Menu,
        SrcKeyCode::KeypadBegin => return None,
        SrcKeyCode::Media(v) => {
            match v {
                SrcMedia::Play => return None,
                SrcMedia::Pause => return None,
                SrcMedia::PlayPause => DstKeyCode::MediaPlayPause,
                SrcMedia::Reverse => return None,
                SrcMedia::Stop => DstKeyCode::MediaStop,
                SrcMedia::FastForward => return None,
                SrcMedia::Rewind => return None,
                SrcMedia::TrackNext => DstKeyCode::MediaNextTrack,
                SrcMedia::TrackPrevious => DstKeyCode::MediaPrevTrack,
                SrcMedia::Record => return None,
                SrcMedia::LowerVolume => DstKeyCode::VolumeDown,
                SrcMedia::RaiseVolume => DstKeyCode::VolumeUp,
                SrcMedia::MuteVolume => DstKeyCode::VolumeMute,
            }
        },
        SrcKeyCode::Modifier(_v) => return None,
    };
    Some(dst_code)

}





pub async fn run1() -> Result<()> {
    enable_raw_mode()?;
    print_events().await;
    disable_raw_mode()?;
    Ok(())
}

async fn print_events() {
    let mut reader = EventStream::new();
    loop {
        let maybe_event = reader.next().await;

        match maybe_event {
            Some(Ok(event)) => {
                println!("Event::{:?}\r", event);

                if let Event::Key(kc) = &event {
                    let dst = map_key_event(kc);
                    if let Some(dst) = dst {
                        let encoded = dst.key.encode(
                            dst.modifiers, 
                            termwiz::input::KeyCodeEncodeModes {
                                encoding: termwiz::input::KeyboardEncoding::Xterm,
                                application_cursor_keys: false,
                                newline_mode: false,
                                modify_other_keys: None,
                            }, 
                            kc.kind == crossterm::event::KeyEventKind::Press,
                        );
                        println!("  map keycode: [{:?}]-[{:?}]\r", dst, encoded);
                    }
                }

                if event == Event::Key(KeyCode::Char('c').into()) {
                    println!("Cursor position: {:?}\r", position());
                }

                if event == Event::Key(KeyCode::Esc.into()) {
                    break;
                }
            }
            Some(Err(e)) => println!("Error: {:?}\r", e),
            None => break,
        }
    }
}
