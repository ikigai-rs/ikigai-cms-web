//! The page face's accept loop under descriptor exhaustion, and with its listener dead
//! (ledger #753).
//!
//! The loop used to log `http accept: {e}` and go round again, whatever the error. At `EMFILE`
//! tokio does not clear the listener's readiness (only `WouldBlock` does), so on Linux the next
//! accept fails at once and the loop spins a core, printing a line per turn, for as long as the
//! process is out of descriptors. And a listener that is DEAD (its descriptor closed or replaced)
//! fails every accept the same way, forever, on every kernel: the room's page face was gone and
//! the process kept running, spinning and logging.
//!
//! Each scenario runs in a CHILD process (this test binary re-executed with one `#[ignore]`d test
//! selected) because it lowers the descriptor limit and then fills every slot, which no other
//! test in the same process could survive. The child reports on stdout; the parent judges.
//!
//! The setup is deterministic, not a race: client sockets are created first and connected only
//! once the table is full (`connect` needs no new descriptor), so the server's accept finds a
//! connection and no descriptor to put it in.
//!
//! ⚠ The kernels differ there (measured for the same loop in ikigai-web, ikigai-rs/ikigai-web
//! PR 21). **Linux** leaves the connection in the backlog, so a loop that retries at once fails
//! at once, forever: that is the spin, and only CI (ubuntu) can show it. **macOS** aborts the
//! connection inside the failed `accept`, so the next attempt finds an empty backlog and waits:
//! no spin, but every connection arriving while the process is full is thrown away. The dead
//! listener spins on both.
//!
//! The scaffolding (descriptor filling, the raw connect, the CPU sample) is ikigai-web's
//! `tests/accept_exhaustion.rs`; what differs is the server, which here is the room's own loop
//! with a handler that answers one fixed response.

#![cfg(all(unix, feature = "server"))]

use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::Command;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The marker that turns an `#[ignore]`d scenario on. Without it a scenario is a no-op, so a
/// stray `cargo test -- --ignored` cannot lower the limit of a process running other tests.
const CHILD: &str = "IKIGAI_CMS_WEB_ACCEPT_EXHAUSTION_CHILD";

/// The descriptor limit a child runs under: small enough to fill quickly, large enough for the
/// runtime and the harness.
const FD_LIMIT: libc::rlim_t = 128;

/// How long the CPU is sampled.
const SAMPLE: Duration = Duration::from_secs(2);

/// The most of one core the server may burn while it cannot accept. The fixed loop sleeps
/// between retries (at most a few percent); a spinning loop sits near 1.0.
const QUIET: f64 = 0.25;

/// Run one scenario in a child process; return (stdout, stderr). `output()` waits for the child
/// to be REAPED, so its status is final (field guide 9h).
fn run_child(scenario: &str) -> (String, String) {
    let exe = std::env::current_exe().expect("test binary path");
    let out = Command::new(exe)
        .args([
            scenario,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .expect("spawn the child scenario");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "child {scenario} failed ({}):\n--- stdout\n{stdout}\n--- stderr\n{}",
        out.status,
        tail(&stderr)
    );
    (stdout, stderr)
}

/// The last lines of a child's stderr: a spinning loop writes megabytes of it.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().collect();
    let from = lines.len().saturating_sub(20);
    format!(
        "({} lines; the last {})\n{}",
        lines.len(),
        lines.len() - from,
        lines[from..].join("\n")
    )
}

/// One `RESULT key=value` line from a child's stdout.
fn result(stdout: &str, key: &str) -> String {
    let prefix = format!("RESULT {key}=");
    stdout
        .lines()
        // The harness may print the test's name on the same line, before it.
        .find_map(|line| line.find(&prefix).map(|at| &line[at + prefix.len()..]))
        .unwrap_or_else(|| panic!("child reported no {key}:\n{stdout}"))
        .to_string()
}

/// Process CPU time (user + system), all threads.
fn cpu_time() -> Duration {
    // SAFETY: getrusage writes one `rusage` into the zeroed struct we hand it.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, &mut usage), 0);
        usage
    };
    let tv = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// The fraction of one core this process burns over [`SAMPLE`].
fn sample_cpu() -> f64 {
    let (cpu0, wall0) = (cpu_time(), Instant::now());
    std::thread::sleep(SAMPLE);
    (cpu_time() - cpu0).as_secs_f64() / wall0.elapsed().as_secs_f64()
}

fn lower_fd_limit() {
    // SAFETY: get/setrlimit read and write one `rlimit`; lowering the soft limit needs no
    // privilege.
    unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        limit.rlim_cur = FD_LIMIT.min(limit.rlim_max);
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
    }
}

/// Open `/dev/null` until the process is out of descriptors; return what was opened.
fn fill_descriptors() -> Vec<File> {
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(e) if e.raw_os_error() == Some(libc::EMFILE) => return fillers,
            Err(e) => panic!("filling descriptors: {e}"),
        }
    }
}

/// The room's accept loop on a loopback port, on its own thread and runtime, answering every
/// connection with one fixed response. Returns where it listens, the listener's descriptor, and
/// the channel that receives the error the loop ended with, if it ever ends.
fn spawn_server() -> (SocketAddr, i32, Receiver<std::io::Error>) {
    let (bound_tx, bound_rx) = std::sync::mpsc::channel();
    let (ended_tx, ended_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            // Port 0, read back from the listener: never a probed-and-released port (field
            // guide 9h).
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            bound_tx
                .send((listener.local_addr().unwrap(), listener.as_raw_fd()))
                .unwrap();
            let error = ikigai_cms_web::accept::serve_until_error(&listener, |mut sock| {
                tokio::spawn(async move {
                    let mut request = [0u8; 256];
                    let _ = sock.read(&mut request).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                });
            })
            .await;
            let _ = ended_tx.send(error);
        })
    });
    let (addr, fd) = bound_rx.recv().expect("server bound");
    (addr, fd, ended_rx)
}

/// How many connections wait in the backlog while the server is out of descriptors.
const QUEUED: usize = 16;

/// Put the server at EMFILE with [`QUEUED`] connections in its backlog, each with its request
/// already written. Returns the clients and the descriptors holding the process full.
fn exhaust(addr: SocketAddr) -> (Vec<TcpStream>, Vec<File>) {
    let sockets: Vec<OwnedFd> = (0..QUEUED).map(|_| tcp_socket()).collect();
    let fillers = fill_descriptors();
    let clients = sockets
        .into_iter()
        .map(|socket| {
            connect(&socket, addr);
            let mut client = TcpStream::from(socket);
            // macOS may already have aborted this connection (see the module docs), so a failed
            // write is not this test's business.
            let _ = client.write_all(REQUEST);
            client
        })
        .collect();
    (clients, fillers)
}

/// An unconnected IPv4 TCP socket.
fn tcp_socket() -> OwnedFd {
    // SAFETY: socket(2) returns a new descriptor we take ownership of, or -1.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` is a fresh descriptor nothing else owns.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Blocking connect to a loopback IPv4 address: the handshake completes in the kernel and the
/// connection waits in the listener's backlog until the server accepts it.
fn connect(socket: &OwnedFd, addr: SocketAddr) {
    let SocketAddr::V4(v4) = addr else {
        panic!("loopback IPv4 expected, got {addr}")
    };
    // SAFETY: an all-zero sockaddr_in is valid; the fields that matter are set below.
    let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = v4.port().to_be();
    sin.sin_addr = libc::in_addr {
        s_addr: u32::from(*v4.ip()).to_be(),
    };
    // SAFETY: `sin` is a complete sockaddr_in and the length says so.
    let rc = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&sin as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "connect: {}", std::io::Error::last_os_error());
}

const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: test\r\n\r\n";

/// Read one response; true when it is HTTP.
fn answered(client: &mut TcpStream) -> bool {
    if client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    let _ = client.read_to_end(&mut response);
    response.starts_with(b"HTTP/1.1 ")
}

#[test]
fn exhaustion_does_not_spin_and_the_page_serves_again_once_descriptors_free_up() {
    let (stdout, stderr) = run_child("child_exhaust_then_recover");
    let busy: f64 = result(&stdout, "cpu_fraction").parse().unwrap();
    assert!(
        busy < QUIET,
        "the accept loop burned {busy:.2} of a core while out of descriptors (spinning)"
    );
    // The scenario must really have put the server at EMFILE, or a quiet CPU proves nothing.
    assert!(
        stderr.contains("accept failing"),
        "the server never reported exhaustion, so the scenario did not reach EMFILE:\n{}",
        tail(&stderr)
    );
    assert_eq!(
        result(&stdout, "fresh_served"),
        "true",
        "a new connection was not served once descriptors were free"
    );
    // Linux keeps a connection queued through EMFILE, so the one that was waiting must be
    // answered too. (macOS aborts it at the failed accept: there is nothing left to answer.)
    if cfg!(target_os = "linux") {
        assert_eq!(result(&stdout, "queued_served"), "true");
    }
    assert!(
        stderr.contains("accept recovered"),
        "the episode's end was not reported:\n{}",
        tail(&stderr)
    );
}

#[test]
#[ignore = "a child scenario: run by the test above, in its own process"]
fn child_exhaust_then_recover() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    lower_fd_limit();
    let (addr, _fd, _ended) = spawn_server();
    let (mut clients, fillers) = exhaust(addr);

    std::thread::sleep(Duration::from_millis(300));
    println!("RESULT cpu_fraction={:.3}", sample_cpu());

    // Give the descriptors back: a new connection must be served, and on Linux the first one
    // that waited through the episode as well.
    drop(fillers);
    let mut fresh = TcpStream::connect(addr).expect("connect after recovery");
    fresh.write_all(REQUEST).unwrap();
    println!("RESULT fresh_served={}", answered(&mut fresh));
    println!("RESULT queued_served={}", answered(&mut clients[0]));
}

#[test]
fn a_dead_listener_ends_the_loop_with_its_error_instead_of_spinning() {
    let (stdout, stderr) = run_child("child_listener_dies_while_backing_off");
    assert_eq!(
        result(&stdout, "ended_with"),
        libc::ENOTSOCK.to_string(),
        "the loop did not end with the dead listener's error:\n{}",
        tail(&stderr)
    );
    assert!(
        stderr.contains("accept failing"),
        "the server was not backing off when its listener died:\n{}",
        tail(&stderr)
    );
    let busy: f64 = result(&stdout, "cpu_fraction_dead").parse().unwrap();
    assert!(
        busy < QUIET,
        "the accept loop burned {busy:.2} of a core with its listener dead (spinning)"
    );
}

#[test]
#[ignore = "a child scenario: run by the test above, in its own process"]
fn child_listener_dies_while_backing_off() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    lower_fd_limit();
    let (addr, fd, ended) = spawn_server();
    let (_clients, fillers) = exhaust(addr);
    std::thread::sleep(Duration::from_millis(300));
    // Kill the listener under the loop: its descriptor now names /dev/null, so the next accept
    // answers ENOTSOCK. The loop must RETRY to see that, and it does only while the listener's
    // readiness stays set: tokio clears it only on WouldBlock, never on EMFILE.
    // SAFETY: dup2 onto a descriptor this process owns; the listener closes it on drop.
    assert_eq!(unsafe { libc::dup2(fillers[0].as_raw_fd(), fd) }, fd);
    // ⚠ While the table is still full, Linux may not say ENOTSOCK at all: its `accept4`
    // reserves the new descriptor BEFORE it looks at the listener, so a dead listener at EMFILE
    // answers EMFILE and the loop rightly keeps backing off. macOS checks the descriptor first.
    // So report what happened while full, then free the table: from there every kernel must
    // say ENOTSOCK on the next retry.
    let early = ended.recv_timeout(Duration::from_millis(1500)).ok();
    println!("RESULT ended_while_full={}", early.is_some());
    drop(fillers);
    let ended_with = match early.or_else(|| ended.recv_timeout(Duration::from_secs(5)).ok()) {
        Some(error) => error
            .raw_os_error()
            .map_or_else(|| format!("{error:?}"), |code| code.to_string()),
        None => "still-running".to_string(),
    };
    println!("RESULT ended_with={ended_with}");
    // Whatever happened, a dead listener must not cost a core: an ended loop costs nothing, and
    // one still running against a dead socket is exactly the defect.
    println!("RESULT cpu_fraction_dead={:.3}", sample_cpu());
}
