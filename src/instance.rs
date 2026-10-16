//! One Kuana per config folder, as Zed does on every platform: a second
//! launch hands its paths to the running Kuana and exits, so two processes
//! never rewrite the same sessions and state (the Windows jump list and the
//! `kuana` command outside an app bundle start a new process each time; a
//! macOS app bundle gets this from the system already). The running Kuana
//! listens on a loopback port that it writes, with a random token, to
//! `<config>/instance.json`.
//!
//! Only release builds do this (like Zed's stable channel): development
//! builds and their tests often run several instances on purpose.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};

/// What `instance.json` holds.
#[derive(Serialize, Deserialize)]
struct Record {
    port: u16,
    token: String,
}

/// What a second launch sends.
#[derive(Serialize, Deserialize)]
struct Hello {
    token: String,
    paths: Vec<PathBuf>,
}

/// This process is the running Kuana: it takes later launches' paths.
pub struct Primary {
    listener: TcpListener,
    token: String,
}

/// Whether to go on starting: `Ok(Some)` as the running Kuana, `Ok(None)`
/// without the single-instance check (a development build), `Err(())` when
/// the paths went to the Kuana already running (this one exits).
pub fn claim(paths: &[PathBuf]) -> Result<Option<Primary>, ()> {
    let enforced = option_env!("KUANA_RELEASE").is_some() || std::env::var_os("KUANA_SINGLE_INSTANCE").is_some();
    match crate::theme::config_dir() {
        Some(dir) if enforced => claim_in(&dir, paths),
        _ => Ok(None),
    }
}

fn claim_in(dir: &Path, paths: &[PathBuf]) -> Result<Option<Primary>, ()> {
    let file = dir.join("instance.json");
    // Relative paths mean this launch's folder, which the running Kuana
    // doesn't know.
    let cwd = std::env::current_dir().unwrap_or_default();
    let paths: Vec<PathBuf> = paths.iter().map(|p| cwd.join(p)).collect();
    if let Some(record) = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Record>(&t).ok())
        && forward(&record, &paths).is_ok()
    {
        return Err(());
    }
    // Nobody answered (none running, or it crashed): this is the one.
    let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) else { return Ok(None) };
    let token = random_token();
    let record = Record { port: listener.local_addr().map(|a| a.port()).unwrap_or(0), token: token.clone() };
    let json = serde_json::to_string(&record).unwrap_or_default();
    if let Err(err) = crate::state::write_atomic(&file, json.as_bytes()) {
        eprintln!("couldn't note the running Kuana: {err}");
    }
    Ok(Some(Primary { listener, token }))
}

/// Hands `paths` to the Kuana in `record`; Ok once it confirmed.
fn forward(record: &Record, paths: &[PathBuf]) -> std::io::Result<()> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, record.port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let hello = Hello { token: record.token.clone(), paths: paths.to_vec() };
    stream.write_all((serde_json::to_string(&hello)? + "\n").as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim() == "ok" { Ok(()) } else { Err(std::io::Error::other("refused")) }
}

impl Primary {
    /// Takes later launches' paths for as long as Kuana runs; an empty list
    /// means a launch without paths (bring Kuana to the front).
    pub fn serve(self, on_paths: impl Fn(Vec<PathBuf>) + Send + 'static) {
        let Primary { listener, token } = self;
        let spawned = std::thread::Builder::new().name("instance".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Some(paths) = receive(stream, &token) {
                    on_paths(paths);
                }
            }
        });
        if let Err(err) = spawned {
            eprintln!("couldn't listen for other launches: {err}");
        }
    }
}

/// One launch's paths, if it knew the token.
fn receive(stream: TcpStream, token: &str) -> Option<Vec<PathBuf>> {
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line).ok()?;
    let hello: Hello = serde_json::from_str(&line).ok()?;
    if hello.token != token {
        return None;
    }
    reader.get_mut().write_all(b"ok\n").ok()?;
    Some(hello.paths)
}

/// A token another local program can't guess (std's hasher keys are random).
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    (0..2)
        .map(|i| {
            let mut hasher = state.build_hasher();
            hasher.write_u64(i ^ std::process::id() as u64);
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_hands_its_paths_to_the_first() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = claim_in(&dir, &[]).unwrap().expect("first launch runs");
        let (tx, rx) = std::sync::mpsc::channel();
        first.serve(move |paths| tx.send(paths).unwrap());
        // The second one forwards and is told to exit.
        assert!(claim_in(&dir, &[PathBuf::from("/work/a.rs")]).is_err());
        assert_eq!(rx.recv_timeout(Duration::from_secs(3)).unwrap(), [PathBuf::from("/work/a.rs")]);
        // A wrong token is ignored, and the launch starts on its own.
        let record: Record = serde_json::from_str(&std::fs::read_to_string(dir.join("instance.json")).unwrap()).unwrap();
        assert!(forward(&Record { token: "guess".into(), ..record }, &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_record_doesnt_stop_a_launch() {
        let dir = std::env::temp_dir().join(format!("kuana-instance-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A port nothing listens on (bound, then released).
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        std::fs::write(dir.join("instance.json"), format!(r#"{{"port":{port},"token":"t"}}"#)).unwrap();
        assert!(claim_in(&dir, &[]).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
