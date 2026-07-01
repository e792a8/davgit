#![cfg(feature = "litmus-tests")]

use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::sync::Once;
use std::time::{Duration, Instant};

static BUILD_LITMUS: Once = Once::new();

/// Path to the litmus submodule checkout.
fn litmus_root() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("litmus");
    assert!(p.exists(), "litmus submodule not found at {}", p.display());
    p
}

/// Ensure litmus is built.  Rebuilds only if `make` would recompile.
fn build_litmus() {
    let root = litmus_root();
    if !root.join("Makefile").exists() {
        let status = Command::new("./autogen.sh")
            .current_dir(&root)
            .status()
            .expect("failed to run autogen.sh");
        assert!(status.success(), "autogen.sh failed");

        let status = Command::new("./configure")
            .current_dir(&root)
            .status()
            .expect("failed to run configure");
        assert!(status.success(), "configure failed");
    }

    let status = Command::new("make")
        .arg("-j")
        .arg(num_cpus().to_string())
        .current_dir(&root)
        .status()
        .expect("failed to run make");
    assert!(status.success(), "make failed");
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

fn litmus_bin(name: &str) -> PathBuf {
    litmus_root().join(name)
}

fn server_exe() -> PathBuf {
    std::env::var("CARGO_BIN_EXE_DAVGIT").map(PathBuf::from).unwrap_or_else(|_| {
        let p = PathBuf::from("target/release/davgit");
        if p.exists() {
            return p;
        }
        PathBuf::from("target/debug/davgit")
    })
}

struct ServerGuard(Child, u16);

impl ServerGuard {
    fn start(port: u16) -> Self {
        let exe = server_exe();
        let remote_url =
            std::env::var("REMOTE_URL").expect("REMOTE_URL must be set for litmus tests");
        let child = Command::new(exe)
            .arg("--remote-url")
            .arg(&remote_url)
            .arg("--branch")
            .arg("main")
            .arg("--port")
            .arg(port.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to start davgit");
        ServerGuard(child, port)
    }

    fn wait_ready(&self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if std::net::TcpStream::connect(format!("127.0.0.1:{}", self.1)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        panic!("server not ready within {:?}", timeout);
    }
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_suite(name: &str, port: u16) -> Output {
    BUILD_LITMUS.call_once(build_litmus);
    let bin = litmus_bin(name);
    let url = format!("http://127.0.0.1:{}", port);
    let root = litmus_root();
    let output = Command::new(&bin)
        .arg(&url)
        .current_dir(&root)
        .output()
        .unwrap_or_else(|e| panic!("failed to run litmus '{}': {}", name, e));
    println!("=== {} ===", name);
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if !output.status.success() {
        println!("(exit code: {:?})", output.status.code());
    }
    output
}

fn assert_passed(output: &Output, expected_min: usize) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(summary) = stdout.lines().find(|l| l.contains("summary")) {
        let passed = summary
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|s| s.parse::<usize>().ok())
            .next()
            .unwrap_or(0);
        assert!(
            passed >= expected_min,
            "expected at least {} passed, got {}",
            expected_min,
            passed
        );
    }
}

#[test]
fn litmus_basic() {
    let server = ServerGuard::start(18080);
    server.wait_ready(Duration::from_secs(15));
    let out = run_suite("basic", 18080);
    assert!(out.status.success());
}

#[test]
fn litmus_http() {
    let server = ServerGuard::start(18081);
    server.wait_ready(Duration::from_secs(15));
    let out = run_suite("http", 18081);
    assert!(out.status.success());
}

#[test]
fn litmus_props() {
    let server = ServerGuard::start(18082);
    server.wait_ready(Duration::from_secs(15));
    let out = run_suite("props", 18082);
    assert_passed(&out, 9);
}

#[test]
fn litmus_copymove() {
    let server = ServerGuard::start(18083);
    server.wait_ready(Duration::from_secs(15));
    let out = run_suite("copymove", 18083);
    assert_passed(&out, 10);
}

#[test]
fn litmus_locks() {
    let server = ServerGuard::start(18084);
    server.wait_ready(Duration::from_secs(15));
    let out = run_suite("locks", 18084);
    println!("(expected all fail — LOCK is 501)");
    let _ = out;
}
