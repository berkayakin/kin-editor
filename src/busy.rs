//! When to show a progress indicator ("Fetching…"): not for an operation that
//! finishes within `DELAY`, and once shown, for at least `MIN_SHOWN`, so it
//! doesn't flash up and vanish before it can be read.

use std::time::{Duration, Instant};

use gpui::{Context, SharedString, Task};

/// Operations quicker than this show no progress at all.
pub const DELAY: Duration = Duration::from_millis(400);
/// A progress indicator that appeared stays at least this long.
pub const MIN_SHOWN: Duration = Duration::from_millis(1000);

/// A running operation's label and what its indicator shows. The view that
/// owns it calls `set` when an operation starts or ends and draws `label`.
#[derive(Default)]
pub struct Busy {
    running: Option<SharedString>,
    shown: Option<(SharedString, Instant)>,
    /// Bumped by every `set`, so a timer from an earlier one does nothing.
    generation: usize,
    _timer: Option<Task<()>>,
}

impl Busy {
    /// Whether an operation is running (whether or not it's shown yet).
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// What the indicator shows, if anything.
    pub fn label(&self) -> Option<&SharedString> {
        self.shown.as_ref().map(|(label, _)| label)
    }

    /// Starts (`Some`) or ends (`None`) an operation. `field` finds this in the
    /// view, for the timers that show and hide the indicator.
    pub fn set<V: 'static>(&mut self, running: Option<SharedString>, field: fn(&mut V) -> &mut Busy, cx: &mut Context<V>) {
        let now = Instant::now();
        let wait = match running {
            Some(label) => self.start(label),
            None => self.finish(now),
        };
        let generation = self.generation;
        self._timer = wait.map(|wait| {
            cx.spawn(async move |view, cx| {
                cx.background_executor().timer(wait).await;
                view.update(cx, |view, cx| {
                    if field(view).elapsed(generation, Instant::now()) {
                        cx.notify();
                    }
                })
                .ok();
            })
        });
        cx.notify();
    }

    /// Returns how long to wait before calling `elapsed`.
    fn start(&mut self, label: SharedString) -> Option<Duration> {
        self.generation += 1;
        self.running = Some(label.clone());
        match &mut self.shown {
            // Already showing (an operation right after another): just relabel.
            Some((shown, _)) => {
                *shown = label;
                None
            }
            None => Some(DELAY),
        }
    }

    fn finish(&mut self, now: Instant) -> Option<Duration> {
        self.generation += 1;
        self.running = None;
        let since = self.shown.as_ref()?.1;
        let rest = MIN_SHOWN.saturating_sub(now - since);
        if rest.is_zero() {
            self.shown = None;
            None
        } else {
            Some(rest)
        }
    }

    /// The wait from `start` or `finish` is over: shows the running operation,
    /// or hides the indicator if nothing runs. Returns whether that changed it.
    fn elapsed(&mut self, generation: usize, now: Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.shown = self.running.clone().map(|label| (label, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_operations_show_nothing() {
        let t0 = Instant::now();
        let mut busy = Busy::default();
        assert_eq!(busy.start("Fetching…".into()), Some(DELAY));
        let generation = busy.generation;
        assert_eq!(busy.finish(t0 + Duration::from_millis(100)), None);
        assert!(!busy.elapsed(generation, t0 + DELAY), "the delay timer is stale");
        assert_eq!(busy.label(), None);
    }

    #[test]
    fn a_shown_indicator_stays_at_least_the_minimum() {
        let t0 = Instant::now();
        let mut busy = Busy::default();
        busy.start("Fetching…".into());
        let shown_at = t0 + DELAY;
        assert!(busy.elapsed(busy.generation, shown_at));
        assert_eq!(busy.label().map(|l| l.as_ref()), Some("Fetching…"));

        // Done 100 ms after it appeared: stays for the rest of the minimum.
        let rest = busy.finish(shown_at + Duration::from_millis(100));
        assert_eq!(rest, Some(MIN_SHOWN - Duration::from_millis(100)));
        assert!(!busy.is_running());
        assert!(busy.label().is_some());
        assert!(busy.elapsed(busy.generation, shown_at + MIN_SHOWN));
        assert_eq!(busy.label(), None);

        // Done after the minimum: hides at once.
        busy.start("Pulling…".into());
        busy.elapsed(busy.generation, shown_at);
        assert_eq!(busy.finish(shown_at + MIN_SHOWN * 2), None);
        assert_eq!(busy.label(), None);
    }

    #[test]
    fn an_operation_starting_while_shown_takes_the_indicator_over() {
        let t0 = Instant::now();
        let mut busy = Busy::default();
        busy.start("Committing…".into());
        busy.elapsed(busy.generation, t0 + DELAY);
        let hold = busy.generation;
        busy.finish(t0 + DELAY);
        assert_eq!(busy.start("Pushing…".into()), None);
        assert!(!busy.elapsed(hold, t0 + DELAY + MIN_SHOWN), "the hide timer is stale");
        assert_eq!(busy.label().map(|l| l.as_ref()), Some("Pushing…"));
    }
}
