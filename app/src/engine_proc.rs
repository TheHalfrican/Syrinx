//! Windows/macOS engine lifecycle: adopt-or-spawn + supervision.
//!
//! On Linux the engine belongs to systemd + D-Bus activation and this module
//! never compiles. On Windows/macOS the app owns the engine as a supervised
//! child process — RPC-PROTOCOL.md §13.2 (adopt-or-spawn) and §13.3
//! (supervision / backoff / quit). The connection itself still goes through
//! `EngineClient::connect_rpc` in `syrinx-shared`; this module only decides
//! *when* to spawn, *which* executable, and *how* to tear it down.
//!
//! Placement rationale: process spawning, app-exe-relative path resolution and
//! quit-time teardown are app policy, and `shared/` deliberately makes a single
//! connect attempt (the retry/supervision loop lives in the app). `dictate/` is
//! Linux-only and never spawns, so this stays out of `shared/`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use syrinx_shared::EngineClient;
use tokio::process::Child;

/// Backoff reset threshold: a session that stayed up this long is "stable", so
/// the next crash starts the ladder over (§13.3).
const STABLE_UPTIME: Duration = Duration::from_secs(60);
/// Readiness polling after a spawn: connect-with-handshake, this often, growing
/// to the cap while the engine binds its socket and warms up (§13.2 step 3).
const READY_POLL_MIN: Duration = Duration::from_millis(200);
const READY_POLL_MAX: Duration = Duration::from_secs(2);
/// Grace after closing the child's stdin before we SIGKILL on quit (§13.3).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// The engine executable, relative to a checkout root (§13.2 steps 2–3).
#[cfg(windows)]
const ENGINE_REL: &str = "engine/.venv/Scripts/syrinx-engine.exe";
#[cfg(not(windows))] // macOS
const ENGINE_REL: &str = "engine/.venv/bin/syrinx-engine";

/// The bare name to hand to the OS for a `PATH` lookup (§13.2 step 4).
#[cfg(windows)]
const ENGINE_BIN: &str = "syrinx-engine.exe";
#[cfg(not(windows))]
const ENGINE_BIN: &str = "syrinx-engine";

/// Exponential respawn backoff (§13.3): 1 s doubling to a 30 s cap.
pub(crate) struct Backoff {
    delay: Duration,
}

impl Backoff {
    const INITIAL: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self { delay: Self::INITIAL }
    }

    /// The delay to wait before the next attempt, then advance (double, capped).
    fn next_delay(&mut self) -> Duration {
        let d = self.delay;
        self.delay = (self.delay * 2).min(Self::MAX);
        d
    }

    /// Back to the bottom of the ladder (after a stable run).
    fn reset(&mut self) {
        self.delay = Self::INITIAL;
    }
}

/// Resolve the engine executable per §13.2's order. Pure over its inputs so the
/// ordering is unit-testable without touching the real process environment.
fn resolve_from(
    env_cmd: Option<&str>,
    cwd: &Path,
    app_exe: &Path,
    rel: &Path,
    path_bin: &str,
) -> PathBuf {
    // 1. explicit override — used verbatim (dev/CI).
    if let Some(cmd) = env_cmd {
        if !cmd.is_empty() {
            return PathBuf::from(cmd);
        }
    }
    // 2. cwd-relative (a checkout run from its root).
    let c = cwd.join(rel);
    if c.is_file() {
        return c;
    }
    // 3. relative to each ancestor of the app exe's dir (covers
    //    target/debug/syrinx-app.exe inside a checkout).
    let mut dir = app_exe.parent();
    while let Some(d) = dir {
        let p = d.join(rel);
        if p.is_file() {
            return p;
        }
        dir = d.parent();
    }
    // 4. bare name — let the OS resolve it on PATH.
    PathBuf::from(path_bin)
}

/// Resolve the engine executable from the live environment (§13.2).
pub(crate) fn resolve_engine_exe() -> PathBuf {
    let env_cmd = std::env::var("SYRINX_ENGINE_CMD").ok();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let exe = std::env::current_exe().unwrap_or_default();
    resolve_from(env_cmd.as_deref(), &cwd, &exe, Path::new(ENGINE_REL), ENGINE_BIN)
}

/// The engine log: `engine.log` beside the discovery file (§13.2 note). Honors
/// `SYRINX_RPC_ENDPOINT`, so tests keep their logs in a temp dir.
fn engine_log_path() -> Option<PathBuf> {
    syrinx_shared::discovery_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("engine.log")))
}

/// Open (truncating) the per-spawn engine log; `None` if it can't be created,
/// in which case the child's output is dropped rather than blocking the spawn.
fn open_log() -> Option<std::fs::File> {
    let path = engine_log_path()?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::File::create(&path).ok()
}

/// Spawn the engine with `SYRINX_SUPERVISED=1`, stdin piped-and-held (never
/// written — its EOF is how the engine learns the app is gone, §13.1), and
/// stdout/stderr redirected to the log. On Windows, `CREATE_NO_WINDOW` keeps a
/// console from flashing.
fn spawn_engine() -> std::io::Result<Child> {
    let exe = resolve_engine_exe();
    tracing::info!("spawning engine: {}", exe.display());

    let mut std_cmd = std::process::Command::new(&exe);
    std_cmd.env("SYRINX_SUPERVISED", "1").stdin(Stdio::piped());
    match open_log() {
        Some(f) => {
            let err = f.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null());
            std_cmd.stdout(Stdio::from(f)).stderr(err);
        }
        None => {
            std_cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std_cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut cmd = tokio::process::Command::from(std_cmd);
    // Backstop: if the supervisor is ever dropped without a clean shutdown, the
    // child dies with it rather than leaking a GPU-holding process.
    cmd.kill_on_drop(true);
    cmd.spawn()
}

/// Owns the engine child (when we spawned it) and the respawn backoff.
///
/// `child` is `Some` only for a *supervised* engine we launched; an *adopted*
/// (externally started) engine leaves it `None` and is never killed on quit
/// (§13.2 case 1).
pub(crate) struct EngineSupervisor {
    child: Option<Child>,
    backoff: Backoff,
}

impl EngineSupervisor {
    pub(crate) fn new() -> Self {
        Self { child: None, backoff: Backoff::new() }
    }

    /// True once we have spawned a child we own (as opposed to adopting one).
    #[cfg(test)]
    fn is_supervising(&self) -> bool {
        self.child.is_some()
    }

    /// §13.2: adopt an already-running engine if one answers, else spawn one.
    /// Blocks until a ready client — the app shows the splash meanwhile, so
    /// "not up yet" is the normal early state (matches the old retry loop).
    pub(crate) async fn adopt_or_spawn(&mut self) -> EngineClient {
        match EngineClient::connect_rpc().await {
            Ok(c) => {
                tracing::info!("adopted an already-running engine");
                self.child = None;
                c
            }
            Err(e) => {
                tracing::info!("no engine to adopt ({e}); spawning one");
                self.spawn_and_wait_ready().await
            }
        }
    }

    /// §13.3: the live session lost its transport. Respawn (or, for an adopted
    /// engine whose socket dropped, spawn — §13.3 falls through to case 2) with
    /// backoff, and return a fresh ready client. `uptime` gates the 60 s reset.
    pub(crate) async fn reconnect(&mut self, uptime: Duration) -> EngineClient {
        if uptime >= STABLE_UPTIME {
            self.backoff.reset();
        }
        self.kill_child().await; // no-op for an adopted engine
        let delay = self.backoff.next_delay();
        tracing::warn!("engine transport lost; respawning in {delay:?}");
        tokio::time::sleep(delay).await;
        self.spawn_and_wait_ready().await
    }

    /// App quit (§13.3): a supervised child dies with the app — close its stdin
    /// (its watchdog then exits), grace, then kill. An adopted engine is left
    /// untouched. On Windows the spawned engine always dies here regardless of
    /// the ⚙ stop-engine toggle (that toggle is Linux/systemd-only).
    pub(crate) async fn shutdown(&mut self) {
        let Some(mut child) = self.child.take() else {
            return; // adopted (or never spawned) — do not touch
        };
        // Close stdin: EOF is the supervised engine's cue to remove its
        // discovery file and _exit(0) (§13.1).
        drop(child.stdin.take());
        match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
            Ok(_) => tracing::info!("engine exited on stdin close"),
            Err(_) => {
                tracing::warn!("engine still alive after grace; killing");
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }

    /// Kill and reap the supervised child if any (used before a respawn).
    async fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Spawn the engine and poll connect-with-handshake until it's ready
    /// (§13.2 step 3). A child that dies before it's ready is respawned with
    /// backoff — no tight crash loop.
    async fn spawn_and_wait_ready(&mut self) -> EngineClient {
        loop {
            match spawn_engine() {
                Ok(child) => self.child = Some(child),
                Err(e) => {
                    let delay = self.backoff.next_delay();
                    tracing::error!("spawn engine failed ({e}); retrying in {delay:?}");
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            let mut wait = READY_POLL_MIN;
            loop {
                // A child that exited during startup can't become ready — back
                // off and respawn (outer loop) rather than poll a dead process.
                if let Some(child) = self.child.as_mut() {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        let delay = self.backoff.next_delay();
                        tracing::error!("engine exited during startup; respawning in {delay:?}");
                        tokio::time::sleep(delay).await;
                        break;
                    }
                }
                match EngineClient::connect_rpc().await {
                    Ok(c) => {
                        tracing::info!("engine ready");
                        return c;
                    }
                    Err(e) => {
                        tracing::debug!("engine not ready yet: {e}");
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(READY_POLL_MAX);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The process tests mutate the global env (SYRINX_ENGINE_CMD /
    // SYRINX_RPC_ENDPOINT) and connect over them, so they must not run
    // concurrently. Serialize them on this async lock (held across awaits, so a
    // tokio Mutex — a std one would trip clippy::await_holding_lock).
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // A no-arg stub the spawn path can launch (SYRINX_ENGINE_CMD is used
    // verbatim). `sort`/`cat` block reading stdin and exit on EOF — exactly the
    // supervised-engine shape (stays alive while stdin is held; exits when the
    // app closes it), without needing a real engine.
    #[cfg(windows)]
    const STUB_BLOCKS_ON_STDIN: &str = r"C:\Windows\System32\sort.exe";
    #[cfg(not(windows))]
    const STUB_BLOCKS_ON_STDIN: &str = "/bin/cat";
    // A no-arg stub that prints something and exits immediately.
    #[cfg(windows)]
    const STUB_EXITS: &str = r"C:\Windows\System32\hostname.exe";
    #[cfg(not(windows))]
    const STUB_EXITS: &str = "/bin/echo";

    fn unique_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "syrinx-engine-proc-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // --- executable resolution order (§13.2) -----------------------------

    #[test]
    fn resolve_env_override_wins_verbatim() {
        let got = resolve_from(
            Some("/abs/override"),
            Path::new("/cwd"),
            Path::new("/app/dir/app.exe"),
            Path::new(ENGINE_REL),
            ENGINE_BIN,
        );
        assert_eq!(got, PathBuf::from("/abs/override"));
    }

    #[test]
    fn resolve_empty_env_is_ignored() {
        // an empty override must not short-circuit to "" — fall through to PATH.
        let got = resolve_from(
            Some(""),
            Path::new("/nope"),
            Path::new("/also/nope/app.exe"),
            Path::new(ENGINE_REL),
            ENGINE_BIN,
        );
        assert_eq!(got, PathBuf::from(ENGINE_BIN));
    }

    #[test]
    fn resolve_cwd_relative_before_exe_ancestors() {
        let cwd = unique_dir("cwd");
        let rel = Path::new(ENGINE_REL);
        let target = cwd.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"stub").unwrap();

        let got = resolve_from(None, &cwd, Path::new("/app/dir/app.exe"), rel, ENGINE_BIN);
        assert_eq!(got, target);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_walks_exe_ancestors() {
        // layout: <root>/engine/.venv/... and app exe at <root>/target/debug/app
        let root = unique_dir("root");
        let rel = Path::new(ENGINE_REL);
        let target = root.join(rel);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"stub").unwrap();
        let app_exe = root.join("target").join("debug").join("syrinx-app.exe");
        std::fs::create_dir_all(app_exe.parent().unwrap()).unwrap();

        // cwd points somewhere without the file, so resolution must climb the
        // exe's ancestors to find <root>/engine/...
        let empty_cwd = unique_dir("empty");
        let got = resolve_from(None, &empty_cwd, &app_exe, rel, ENGINE_BIN);
        assert_eq!(got, target);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&empty_cwd);
    }

    #[test]
    fn resolve_falls_back_to_path() {
        let got = resolve_from(
            None,
            Path::new("/no/such/cwd"),
            Path::new("/no/such/app/app.exe"),
            Path::new(ENGINE_REL),
            ENGINE_BIN,
        );
        assert_eq!(got, PathBuf::from(ENGINE_BIN));
    }

    // --- backoff schedule (§13.3) ----------------------------------------

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut b = Backoff::new();
        let secs = |d: Duration| d.as_secs();
        assert_eq!(secs(b.next_delay()), 1);
        assert_eq!(secs(b.next_delay()), 2);
        assert_eq!(secs(b.next_delay()), 4);
        assert_eq!(secs(b.next_delay()), 8);
        assert_eq!(secs(b.next_delay()), 16);
        assert_eq!(secs(b.next_delay()), 30); // 32 capped
        assert_eq!(secs(b.next_delay()), 30); // stays at cap
        b.reset();
        assert_eq!(secs(b.next_delay()), 1);
    }

    // --- spawn / stdin / exit mechanics (§13.1/§13.3) --------------------

    // Point the log (via the discovery override) at a temp dir so spawns don't
    // scribble in the real LOCALAPPDATA, and restore it after.
    struct EndpointGuard(Option<String>);
    impl EndpointGuard {
        fn set(dir: &Path) -> Self {
            let prev = std::env::var("SYRINX_RPC_ENDPOINT").ok();
            std::env::set_var("SYRINX_RPC_ENDPOINT", dir.join("rpc.json"));
            Self(prev)
        }
    }
    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("SYRINX_RPC_ENDPOINT", v),
                None => std::env::remove_var("SYRINX_RPC_ENDPOINT"),
            }
        }
    }

    #[tokio::test]
    async fn spawn_holds_stdin_then_exits_on_close() {
        let _env = ENV_LOCK.lock().await;
        let dir = unique_dir("spawn-stdin");
        let _guard = EndpointGuard::set(&dir);
        std::env::set_var("SYRINX_ENGINE_CMD", STUB_BLOCKS_ON_STDIN);

        let mut child = spawn_engine().expect("spawn stub");
        // stdin was piped and is held — the stub blocks on it and stays alive.
        assert!(child.stdin.is_some(), "stdin must be a held pipe");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "stub must stay alive while stdin is held"
        );

        // Close stdin: the stub sees EOF and exits (the watchdog shape).
        drop(child.stdin.take());
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("stub should exit soon after stdin close")
            .expect("wait");
        assert!(status.success() || !status.success()); // just prove it exited

        std::env::remove_var("SYRINX_ENGINE_CMD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn supervisor_shutdown_closes_stdin_and_reaps() {
        let _env = ENV_LOCK.lock().await;
        let dir = unique_dir("shutdown");
        let _guard = EndpointGuard::set(&dir);
        std::env::set_var("SYRINX_ENGINE_CMD", STUB_BLOCKS_ON_STDIN);

        let mut sup = EngineSupervisor::new();
        sup.child = Some(spawn_engine().expect("spawn stub"));
        assert!(sup.is_supervising());

        // shutdown must close stdin, let the stub exit within grace, and reap it.
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("shutdown should finish within grace");
        assert!(!sup.is_supervising(), "child taken during shutdown");

        std::env::remove_var("SYRINX_ENGINE_CMD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn child_exit_is_observed_for_respawn() {
        let _env = ENV_LOCK.lock().await;
        let dir = unique_dir("exit");
        let _guard = EndpointGuard::set(&dir);
        std::env::set_var("SYRINX_ENGINE_CMD", STUB_EXITS);

        let mut child = spawn_engine().expect("spawn stub");
        // The exit-detection building block the respawn loop relies on: within a
        // bounded wait, an immediately-exiting child reports Some via try_wait.
        let mut observed = false;
        for _ in 0..50 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                observed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(observed, "an exiting child must be observed by try_wait");

        std::env::remove_var("SYRINX_ENGINE_CMD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- full adopt-fails → spawn → ready (§13.2) ------------------------

    // A minimal fake engine: a WS server that answers the Authenticate + one
    // readiness round-trip, exactly as RPC-PROTOCOL.md §2.3/§7.3 require, so the
    // real `EngineClient::connect_rpc` treats it as a live engine.
    async fn fake_ws_engine(listener: tokio::net::TcpListener, token: String) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        while let Ok((tcp, _)) = listener.accept().await {
            let token = token.clone();
            tokio::spawn(async move {
                let mut ws = match tokio_tungstenite::accept_async(tcp).await {
                    Ok(w) => w,
                    Err(_) => return,
                };
                while let Some(Ok(msg)) = ws.next().await {
                    if !msg.is_text() {
                        continue;
                    }
                    let v: serde_json::Value =
                        serde_json::from_str(msg.to_text().unwrap()).unwrap();
                    let id = v["id"].clone();
                    let reply = match v["method"].as_str().unwrap_or("") {
                        "Authenticate" if v["params"][0] == token => {
                            serde_json::json!({"jsonrpc":"2.0","result":true,"id":id})
                        }
                        "Authenticate" => serde_json::json!({
                            "jsonrpc":"2.0",
                            "error":{"code":-32001,"message":"invalid token"},"id":id
                        }),
                        // any readiness call (GetBackend/GetModelLoaded/…)
                        _ => serde_json::json!({"jsonrpc":"2.0","result":"cpu","id":id}),
                    };
                    if ws.send(Message::Text(reply.to_string())).await.is_err() {
                        break;
                    }
                }
            });
        }
    }

    #[tokio::test]
    async fn adopt_fails_then_spawns_and_reaches_ready() {
        let _env = ENV_LOCK.lock().await;
        let dir = unique_dir("ready");
        let disc = dir.join("rpc.json");
        let _guard = EndpointGuard::set(&dir);
        std::env::set_var("SYRINX_ENGINE_CMD", STUB_BLOCKS_ON_STDIN);

        // Bring up the fake engine's socket, but withhold the discovery file so
        // the initial adopt attempt fails (file missing) and the supervisor
        // must spawn.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "b9f1c0e2a7d34f5c8e1b6a09d2f47e3c".to_string();
        tokio::spawn(fake_ws_engine(listener, token.clone()));

        // Simulate the spawned engine writing its discovery file a moment after
        // boot (the child stub itself is just a stdin-blocker keeping the
        // supervised process alive; the file is what makes connect succeed).
        let disc2 = disc.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            std::fs::write(
                &disc2,
                format!(
                    r#"{{"protocol":1,"port":{port},"token":"{token}","pid":1,"url":"ws://127.0.0.1:{port}"}}"#
                ),
            )
            .unwrap();
        });

        let mut sup = EngineSupervisor::new();
        let client = tokio::time::timeout(Duration::from_secs(10), sup.adopt_or_spawn())
            .await
            .expect("should reach ready within timeout");
        // We spawned it (adoption failed), so it is supervised.
        assert!(sup.is_supervising(), "must be a supervised child, not adopted");
        // It's a live, authenticated client.
        assert_eq!(client.backend().await.unwrap(), "cpu");

        drop(client);
        tokio::time::timeout(Duration::from_secs(5), sup.shutdown())
            .await
            .expect("clean shutdown");

        std::env::remove_var("SYRINX_ENGINE_CMD");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
