//! Downloads in progress (wasi-sdk, language servers, Node.js, updates), for
//! the status bar: what, how far (curl's progress and the bytes written so
//! far) and how fast. `updater::download` reports here.

use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

struct Active {
    id: u64,
    label: String,
    path: PathBuf,
    /// curl's progress, in thousandths (None until it reports).
    permille: Option<u32>,
    /// The size, when it's known beforehand (GitHub lists its assets').
    size: Option<u64>,
    started: Instant,
}

static ACTIVE: Mutex<Vec<Active>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
/// Bumped when a download starts or ends, so views know to redraw.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// One download's state, as shown.
pub struct Progress {
    pub label: String,
    pub done: u64,
    /// Known beforehand (else only the fraction, from curl, is shown).
    pub total: Option<u64>,
    pub fraction: Option<f32>,
    pub bytes_per_second: Option<f64>,
}

/// A download being tracked; dropping it ends it.
pub struct Tracker(u64);

impl Tracker {
    pub fn set_fraction(&self, fraction: f32) {
        if let Some(active) = ACTIVE.lock().unwrap().iter_mut().find(|a| a.id == self.0) {
            active.permille = Some((fraction.clamp(0., 1.) * 1000.) as u32);
        }
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        ACTIVE.lock().unwrap().retain(|a| a.id != self.0);
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

pub fn start(label: &str, path: &Path, size: Option<u64>) -> Tracker {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    ACTIVE.lock().unwrap().push(Active { id, label: label.to_string(), path: path.to_path_buf(), permille: None, size, started: Instant::now() });
    GENERATION.fetch_add(1, Ordering::SeqCst);
    Tracker(id)
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub fn any() -> bool {
    !ACTIVE.lock().unwrap().is_empty()
}

pub fn active() -> Vec<Progress> {
    ACTIVE
        .lock()
        .unwrap()
        .iter()
        .map(|a| {
            let done = std::fs::metadata(&a.path).map_or(0, |m| m.len());
            let total = a.size.filter(|size| *size > 0);
            let fraction = match total {
                Some(total) => Some((done as f64 / total as f64).min(1.) as f32),
                None => a.permille.map(|p| p as f32 / 1000.),
            };
            let seconds = a.started.elapsed().as_secs_f64();
            let bytes_per_second = (seconds > 1.).then(|| done as f64 / seconds);
            Progress { label: a.label.clone(), done, total, fraction, bytes_per_second }
        })
        .collect()
}

/// "wasi-sdk: 81 of 180 MB (45%), 3.2 MB/s"
pub fn describe(p: &Progress) -> String {
    let mb = |bytes: f64| bytes / 1_000_000.;
    let mut text = match (p.total, p.fraction) {
        (Some(total), Some(fraction)) => {
            format!("{}: {:.0} of {:.0} MB ({:.0}%)", p.label, mb(p.done as f64), mb(total as f64), fraction * 100.)
        }
        (None, Some(fraction)) => format!("{}: {:.0} MB ({:.0}%)", p.label, mb(p.done as f64), fraction * 100.),
        _ => format!("{}: {:.1} MB", p.label, mb(p.done as f64)),
    };
    if let Some(speed) = p.bytes_per_second {
        text.push_str(&format!(", {:.1} MB/s", mb(speed)));
    }
    text
}

/// Every download, a line each, for the status bar's tooltip.
pub fn summary() -> String {
    let lines: Vec<String> = active().iter().map(describe).collect();
    if lines.is_empty() { "Downloads finished".into() } else { format!("Downloading\n{}", lines.join("\n")) }
}

/// The last percentage in curl's `--progress-bar` output ("####   45.3%").
pub fn parse_curl_progress(text: &str) -> Option<f32> {
    text.rsplit(['\r', '\n'])
        .filter_map(|segment| segment.trim().rsplit(' ').next()?.strip_suffix('%')?.parse::<f32>().ok())
        .next()
        .map(|percent| percent / 100.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_curls_progress_and_describes_it() {
        assert_eq!(parse_curl_progress("\r###                 4.5%\r########          45.0%"), Some(0.45));
        assert_eq!(parse_curl_progress("\r##### 100.0%\n"), Some(1.0));
        assert_eq!(parse_curl_progress("curl: (22) The requested URL returned error: 404\n"), None);

        let dir = std::env::temp_dir().join(format!("kuana-download-{}", std::process::id()));
        std::fs::write(&dir, vec![0u8; 2_000_000]).unwrap();
        // A known size gives the fraction; otherwise curl's percentage does.
        let tracker = start("wasi-sdk", &dir, Some(4_000_000));
        let shown = active().into_iter().find(|p| p.label == "wasi-sdk").unwrap();
        assert_eq!((shown.done, shown.total, shown.fraction), (2_000_000, Some(4_000_000), Some(0.5)));
        assert!(describe(&shown).starts_with("wasi-sdk: 2 of 4 MB (50%)"), "{}", describe(&shown));
        drop(tracker);
        let tracker = start("Node.js", &dir, None);
        tracker.set_fraction(0.25);
        let shown = active().into_iter().find(|p| p.label == "Node.js").unwrap();
        assert!(describe(&shown).starts_with("Node.js: 2 MB (25%)"), "{}", describe(&shown));
        drop(tracker);
        assert!(active().iter().all(|p| p.label != "wasi-sdk"));
        std::fs::remove_file(&dir).ok();
    }
}
