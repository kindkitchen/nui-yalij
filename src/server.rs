//! One local coordinator process hosts independent, single-owner session servers.
//! A session switch only changes routing/presence; it never restarts a PTY.

use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use crate::{
    app::App,
    decoration::Library,
    ipc::{self, Peer},
    store::Store,
};

pub const TICK: Duration = Duration::from_millis(250);
/// Repaint promptly even when scrolling/key repeats keep the queue full.
const INPUT_BUDGET: Duration = Duration::from_millis(16);
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

pub fn signals() {
    STOP.store(false, Ordering::Relaxed);
    // SAFETY: handlers only set a lock-free atomic; they do not allocate or do I/O.
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
}
pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

struct SocketFile(PathBuf);
impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn serve(state: PathBuf, config: PathBuf, legacy: Option<PathBuf>) -> Result<(), String> {
    let mut store = Store::new(state.clone());
    if !store.lock()? {
        return Err(format!("a coordinator already owns {}", state.display()));
    }
    let library = Library::load(config)?;
    let socket = ipc::socket_path(&state);
    ipc::prepare_socket(&socket)?;
    let listener =
        UnixListener::bind(&socket).map_err(|error| format!("{}: {error}", socket.display()))?;
    let _socket = SocketFile(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let (events, received) = mpsc::channel();
    let mut app = App::open(
        store,
        library,
        events.clone(),
        crate::shell(),
        legacy.as_deref(),
    )?;
    signals();
    let result = (|| {
        let mut next_client = 1;
        let mut tick = Instant::now() + TICK;
        while !app.quit && !stopped() {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let id = next_client;
                        next_client += 1;
                        app.accept(id, Peer::spawn(id, stream, events.clone())?);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error.to_string()),
                }
            }
            app.poll_gestures();
            app.draw();
            let delay = tick
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(40));
            match received.recv_timeout(delay) {
                Ok(event) => {
                    let until = Instant::now() + INPUT_BUDGET;
                    app.handle(event);
                    // A count limit alone allows slow handlers to postpone every
                    // redraw for seconds. Bound time as well as queue consumption.
                    if !app.quit {
                        drain(&received, until, |event| {
                            app.handle(event);
                            !app.quit
                        });
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= tick {
                app.tick();
                tick = Instant::now() + TICK;
            }
        }
        Ok(())
    })();
    app.shutdown();
    result
}

fn drain<T>(received: &mpsc::Receiver<T>, until: Instant, mut handle: impl FnMut(T) -> bool) {
    for _ in 0..255 {
        if Instant::now() >= until {
            break;
        }
        let Ok(event) = received.try_recv() else {
            break;
        };
        if !handle(event) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_batches_yield_to_drawing_even_with_a_full_queue() {
        let (sent, received) = mpsc::channel();
        for event in 0..300 {
            sent.send(event).unwrap();
        }
        let mut handled = 0;
        drain(&received, Instant::now() + INPUT_BUDGET, |_| {
            handled += 1;
            std::thread::sleep(INPUT_BUDGET + Duration::from_millis(1));
            true
        });
        assert!(
            handled <= 1,
            "slow input must not postpone a repaint for an entire batch"
        );
        assert!(
            received.try_recv().is_ok(),
            "the backlog is left for the next frame"
        );
    }

    #[test]
    fn batches_keep_the_count_limit_and_stop_on_shutdown() {
        let (sent, received) = mpsc::channel();
        for event in 0..300 {
            sent.send(event).unwrap();
        }
        let mut handled = 0;
        drain(&received, Instant::now() + Duration::from_secs(60), |_| {
            handled += 1;
            true
        });
        assert_eq!(handled, 255);
        drain(&received, Instant::now() + Duration::from_secs(60), |_| {
            handled += 1;
            false
        });
        assert_eq!(handled, 256);
        drain(&received, Instant::now() - INPUT_BUDGET, |_| {
            panic!("expired budget")
        });
    }
}
