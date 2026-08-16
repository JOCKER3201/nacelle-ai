//! A daemon without a command does NOTHING — the owner's rule, held
//! open and measured. This is the one test that uses a real socket,
//! because what it verifies is the real front door: the listener is up,
//! a client is connected, and until a command arrives not one byte
//! comes out, not one model is asked for, not one process is run.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nacelle_ai_daemon::backends::{Session, World};
use nacelle_ai_daemon::media::Ffmpeg;
use nacelle_ai_daemon::proto::Wanted;
use nacelle_ai_daemon::{serve, socket};

/// A world that remembers whether the daemon ever reached for it.
/// `backends()` is exempt: it is only called to answer a `hello`,
/// which is a command.
struct Tripwire {
    touched: Arc<AtomicBool>,
}

impl World for Tripwire {
    fn backends(&mut self) -> Vec<String> {
        Vec::new()
    }

    fn session(&mut self, _asked: Wanted) -> Result<Session, String> {
        self.touched.store(true, Ordering::SeqCst);
        Err("the tripwire has no sessions".to_string())
    }

    fn ffmpeg(&mut self) -> Result<Ffmpeg, String> {
        self.touched.store(true, Ordering::SeqCst);
        Err("the tripwire has no ffmpeg".to_string())
    }
}

/// A private place for this test's socket, cleaned up at the end.
fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nacelle-ai-test-{name}-{}", std::process::id()))
}

#[test]
fn a_daemon_without_a_command_does_nothing() {
    let dir = test_dir("idle");
    let (listener, path) = socket::listen(&dir).expect("cannot bind the test socket");

    // The front door is private: 0700 on the directory, 0600 on the
    // socket, exactly as the spec writes them.
    let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700, "the socket directory is the owner's alone");
    let sock_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(sock_mode, 0o600, "the socket is the owner's alone");

    // The daemon's own accept loop, as main runs it: wait, serve, wait.
    let touched = Arc::new(AtomicBool::new(false));
    let tripped = touched.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            let mut world = Tripwire {
                touched: tripped.clone(),
            };
            serve::run(reader, stream, &mut world);
        }
    });

    // A client connects and says NOTHING.
    let mut client = UnixStream::connect(&path).expect("cannot connect to the test socket");
    client
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();

    // Nothing comes out. Not a greeting, not a banner, not a byte:
    // the read runs its whole timeout without data.
    let mut buf = [0u8; 64];
    match client.read(&mut buf) {
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
        Ok(0) => panic!("the daemon closed a quiet connection"),
        Ok(n) => panic!(
            "the daemon spoke unasked: {:?}",
            String::from_utf8_lossy(&buf[..n])
        ),
        Err(e) => panic!("read failed oddly: {e}"),
    }

    // And nothing happened behind the socket either: no session was
    // built, no model was looked for, no tool ran.
    assert!(
        !touched.load(Ordering::SeqCst),
        "the daemon reached for the world without a command"
    );

    // The daemon was not dead — it was waiting. The first command is
    // answered.
    client.write_all(b"{\"cmd\":\"hello\",\"client\":\"idle-test\",\"proto\":0}\n").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reply = String::new();
    BufReader::new(&client).read_line(&mut reply).unwrap();
    let hello: serde_json::Value = serde_json::from_str(reply.trim()).unwrap();
    assert_eq!(hello["ev"], "hello");
    assert_eq!(hello["proto"], 0);

    // Answering a hello needs no session and no ffmpeg.
    assert!(!touched.load(Ordering::SeqCst));

    drop(client);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn a_second_daemon_refuses_to_take_the_same_socket() {
    let dir = test_dir("busy");
    let (listener, path) = socket::listen(&dir).expect("cannot bind the test socket");

    let err = socket::listen(&dir).expect_err("two daemons on one socket");
    assert!(err.contains("already listening"), "said: {err}");

    drop(listener);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

#[test]
fn a_stale_socket_is_swept_and_the_daemon_starts() {
    let dir = test_dir("stale");
    // The remains of a daemon that was killed: a socket file nobody
    // answers on.
    {
        let (listener, _path) = socket::listen(&dir).expect("cannot bind the test socket");
        drop(listener);
    }
    let sock = dir.join(socket::SOCKET_NAME);
    assert!(sock.exists(), "the stale socket file is still there");

    let (listener, path) = socket::listen(&dir).expect("a stale socket must not block startup");
    drop(listener);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}
