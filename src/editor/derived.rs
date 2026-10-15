//! Data derived from the whole document (outline, fold regions, run targets,
//! conflicts, the Markdown preview): computed in the background from a
//! snapshot (a rope and tree clone are cheap), shown from the last result
//! until the new one is ready. Like linting and blame, a result is applied
//! only if it's still the one asked for.

use std::{sync::Arc, time::Duration};

use gpui::{Context, Task};

/// How long after an edit to start: typing doesn't recompute per keystroke.
const DEBOUNCE: Duration = Duration::from_millis(150);

pub(super) struct Derived<K, T> {
    /// What `value` was computed for (None: nothing yet).
    done: Option<K>,
    value: Arc<T>,
    /// What's being computed.
    pending: Option<(K, Task<()>)>,
}

impl<K: PartialEq + Clone + 'static, T: Default + Send + Sync + 'static> Derived<K, T> {
    pub(super) fn new() -> Self {
        Derived { done: None, value: Arc::default(), pending: None }
    }

    /// The last result, whatever it was computed for: for drawing.
    pub(super) fn last(&self) -> Arc<T> {
        self.value.clone()
    }

    /// The result for `key` if it's ready.
    pub(super) fn current(&self, key: &K) -> Option<Arc<T>> {
        (self.done.as_ref() == Some(key)).then(|| self.value.clone())
    }

    /// The result for `key`, computed here and now if it isn't ready: for
    /// actions on it (folding, resolving a conflict), which mustn't act on
    /// an older version's rows.
    pub(super) fn now(&mut self, key: K, compute: impl FnOnce() -> T) -> Arc<T> {
        if self.done.as_ref() != Some(&key) {
            self.pending = None;
            self.value = Arc::new(compute());
            self.done = Some(key);
        }
        self.value.clone()
    }

    /// Computes the result for `key` in the background, unless it's ready or
    /// underway; the view redraws when it arrives. The first one starts at
    /// once, later ones after a pause in editing. `field` finds this
    /// `Derived` in the view again.
    pub(super) fn schedule<V: 'static>(
        &mut self,
        key: K,
        compute: impl FnOnce() -> T + Send + 'static,
        field: fn(&mut V) -> &mut Self,
        cx: &mut Context<V>,
    ) {
        if self.done.as_ref() == Some(&key) || self.pending.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let delay = if self.done.is_some() { DEBOUNCE } else { Duration::ZERO };
        let wanted = key.clone();
        // Replacing an older pending task cancels it.
        let task = cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let value = cx.background_executor().spawn(async move { compute() }).await;
            this.update(cx, |view, cx| {
                let derived = field(view);
                if derived.pending.as_ref().is_some_and(|(k, _)| *k == wanted) {
                    // This task is running; let it finish instead of dropping it.
                    if let Some((_, task)) = derived.pending.take() {
                        task.detach();
                    }
                    derived.value = Arc::new(value);
                    derived.done = Some(wanted);
                    cx.notify();
                }
            })
            .ok();
        });
        self.pending = Some((key, task));
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    struct View(Derived<u64, String>);

    fn schedule(view: &mut View, key: u64, cx: &mut Context<View>) {
        view.0.schedule(key, move || format!("v{key}"), |v| &mut v.0, cx);
    }

    #[gpui::test]
    fn shows_the_last_result_until_the_newest_arrives(cx: &mut TestAppContext) {
        let view = cx.new(|_| View(Derived::new()));
        // The first result comes without waiting.
        view.update(cx, |v, cx| schedule(v, 1, cx));
        cx.run_until_parked();
        view.read_with(cx, |v, _| assert_eq!(*v.0.last(), "v1"));

        // Edits in a row: the old result shows meanwhile, and only the
        // newest is computed and applied, after the pause.
        view.update(cx, |v, cx| {
            schedule(v, 2, cx);
            schedule(v, 3, cx);
            assert_eq!(*v.0.last(), "v1");
        });
        cx.run_until_parked();
        view.read_with(cx, |v, _| assert!(v.0.current(&2).is_none() && v.0.current(&3).is_none()));
        cx.executor().advance_clock(DEBOUNCE);
        cx.run_until_parked();
        view.read_with(cx, |v, _| assert_eq!(v.0.current(&3).as_deref().map(String::as_str), Some("v3")));

        // An action that needs the value now gets it, and a background
        // result for that key arriving later changes nothing.
        view.update(cx, |v, cx| {
            schedule(v, 4, cx);
            assert_eq!(*v.0.now(4, || "now".into()), "now");
        });
        cx.executor().advance_clock(DEBOUNCE);
        cx.run_until_parked();
        view.read_with(cx, |v, _| assert_eq!(*v.0.last(), "now"));
    }
}
