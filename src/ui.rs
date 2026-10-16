//! Small shared UI building blocks.

use gpui::{
    AnyView, App, Context, Div, Entity, Focusable, Hsla, IntoElement, ParentElement, Pixels,
    Img, Render, SharedString, Stateful, Styled, Window, div, img, prelude::*, px, svg,
};

use std::time::{Duration, Instant};

use crate::{
    assets::Icon,
    editor::Editor,
    file_icons,
    theme::{Appearance, Theme},
    transition::{self, Transition, TransitionExt, mix},
};

// ---- Scrollbars ---------------------------------------------------------------------

/// Width of a list's scrollbar strip; the thumb is narrower, off its right edge.
pub const SCROLLBAR_WIDTH: f32 = 10.0;
/// How long a scrollbar stays after scrolling or hovering, and how long it
/// takes to fade in and out.
const SCROLLBAR_HOLD: Duration = Duration::from_millis(1000);
const SCROLLBAR_FADE_IN: Duration = Duration::from_millis(150);
const SCROLLBAR_FADE_OUT: Duration = Duration::from_millis(300);

/// When a scrollbar's thumb shows: while its content scrolls, it's hovered or
/// dragged, then for `SCROLLBAR_HOLD`; it fades in and out, a reversal
/// continuing from where it is. Kept by whatever draws the scrollbar and stepped
/// once per paint.
#[derive(Default)]
pub struct ScrollbarFade {
    /// The scroll position last painted; a change counts as activity.
    offset: Option<Pixels>,
    active_at: Option<Instant>,
    /// When a redraw is already scheduled for, so frames don't pile up timers.
    wake_at: Option<Instant>,
    /// 0 hidden → 1 shown, moving at the fade speeds (eased when painted).
    level: f32,
    painted_at: Option<Instant>,
}

/// When a scrollbar wants to be painted again.
pub enum FadeWake {
    None,
    /// Next frame: it's fading.
    Frame,
    /// When the hold ends.
    After(Duration),
}

impl ScrollbarFade {
    /// The thumb's opacity (0–1) this paint, and when to paint again.
    pub fn step(&mut self, offset: Pixels, active: bool, minimal: bool) -> (f32, FadeWake) {
        let now = Instant::now();
        if active || self.offset.is_some_and(|o| o != offset) {
            self.active_at = Some(now);
        }
        self.offset = Some(offset);
        let held = self.active_at.map(|t| now.saturating_duration_since(t)).filter(|d| *d < SCROLLBAR_HOLD);
        let target = if active || held.is_some() { 1. } else { 0. };
        // At most a frame's worth (60 Hz), so a first paint after a while
        // starts the fade instead of jumping into it.
        let dt = self.painted_at.map_or(0., |t| now.saturating_duration_since(t).as_secs_f32().min(1. / 60.));
        self.painted_at = Some(now);
        self.level = if minimal {
            target
        } else if target > self.level {
            (self.level + dt / SCROLLBAR_FADE_IN.as_secs_f32()).min(1.)
        } else {
            (self.level - dt / SCROLLBAR_FADE_OUT.as_secs_f32()).max(0.)
        };
        let wake = if self.level != target {
            FadeWake::Frame
        } else if let Some(d) = held.filter(|_| !active)
            && self.wake_at.is_none_or(|w| w < now)
        {
            self.wake_at = Some(now + (SCROLLBAR_HOLD - d));
            FadeWake::After(SCROLLBAR_HOLD - d)
        } else {
            FadeWake::None
        };
        (self.level * self.level * (3. - 2. * self.level), wake)
    }
}

#[derive(Default)]
pub struct ScrollbarState {
    /// Where in the thumb it was grabbed, while it's dragged.
    grab: Option<Pixels>,
    hovered: bool,
    fade: ScrollbarFade,
}

/// A scrollbar for what `handle` tracks (a `uniform_list`'s `base_handle`, or a
/// div's `track_scroll`), laid over the right edge of its parent, which must be
/// `relative` and as tall as the scrolled area. Like an overlay scrollbar: it
/// shows while the content scrolls or the pointer is over it, then fades out
/// (`ScrollbarFade`; no fades with Minimal visual effects); only when the
/// content overflows.
/// Drag the thumb, or click the track to jump there.
pub fn scrollbar(id: impl Into<gpui::ElementId>, handle: &gpui::ScrollHandle, cx: &App) -> gpui::Canvas<(gpui::Hitbox, Entity<ScrollbarState>)> {
    let id = id.into();
    let handle = handle.clone();
    let theme = Theme::get(cx);
    let (color, minimal): (Hsla, bool) = (theme.text.into(), theme.reduce_effects);
    gpui::canvas(
        move |bounds, window, cx| {
            let state = window.use_keyed_state(id, cx, |_, _| ScrollbarState::default());
            (window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal), state)
        },
        move |bounds, (hitbox, state), window, cx| {
            let max = handle.max_offset().height;
            let track = bounds.size.height;
            if max <= px(0.) || track <= px(0.) {
                return;
            }
            let offset = handle.offset().y;
            let (fade, wake) = state.update(cx, |s, _| s.fade.step(offset, s.grab.is_some() || s.hovered, minimal));
            match wake {
                FadeWake::Frame => window.request_animation_frame(),
                FadeWake::After(delay) => {
                    let view = window.current_view();
                    cx.spawn(async move |cx| {
                        cx.background_executor().timer(delay).await;
                        cx.update(|cx| cx.notify(view)).ok();
                    })
                    .detach();
                }
                FadeWake::None => {}
            }
            // Over the rows' own cursor (a pointing hand on clickable ones).
            window.set_cursor_style(gpui::CursorStyle::Arrow, &hitbox);

            let thumb_h = (track * (track / (track + max))).max(px(24.)).min(track);
            let travel = (track - thumb_h).max(px(1.));
            let t = (-offset / max).clamp(0., 1.);
            let thumb = gpui::Bounds::new(
                gpui::point(bounds.right() - px(8.), bounds.top() + travel * t),
                gpui::size(px(6.), thumb_h),
            );
            if fade > 0. {
                let lit = state.read(cx).grab.is_some() || state.read(cx).hovered;
                let alpha = if lit { 0.28 } else { 0.16 } * fade;
                window.paint_quad(gpui::fill(thumb, color.alpha(alpha)).corner_radii(px(3.)));
            }

            let top = bounds.top();
            let scroll_to = {
                let handle = handle.clone();
                move |y: Pixels, grab: Pixels| {
                    let t = ((y - grab - top) / travel).clamp(0., 1.);
                    handle.set_offset(gpui::point(handle.offset().x, -(max * t)));
                }
            };
            window.on_mouse_event({
                let (state, hitbox, scroll_to) = (state.clone(), hitbox.clone(), scroll_to.clone());
                move |e: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble || e.button != gpui::MouseButton::Left || !hitbox.is_hovered(window) {
                        return;
                    }
                    let on_thumb = thumb.contains(&e.position);
                    let grab = if on_thumb { e.position.y - thumb.top() } else { thumb_h / 2. };
                    if !on_thumb {
                        scroll_to(e.position.y, grab);
                    }
                    state.update(cx, |s, cx| {
                        s.grab = Some(grab);
                        cx.notify();
                    });
                    // The rows under the bar don't take the click.
                    cx.stop_propagation();
                }
            });
            window.on_mouse_event({
                let state = state.clone();
                move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    let hovered = hitbox.is_hovered(window);
                    let (grab, was_hovered) = (state.read(cx).grab, state.read(cx).hovered);
                    match grab {
                        Some(grab) if e.pressed_button == Some(gpui::MouseButton::Left) => {
                            scroll_to(e.position.y, grab);
                            state.update(cx, |_, cx| cx.notify());
                        }
                        _ if hovered != was_hovered => state.update(cx, |s, cx| {
                            s.hovered = hovered;
                            cx.notify();
                        }),
                        _ => {}
                    }
                }
            });
            window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                if phase == gpui::DispatchPhase::Bubble && state.read(cx).grab.is_some() {
                    state.update(cx, |s, cx| {
                        s.grab = None;
                        cx.notify();
                    });
                }
            });
        },
    )
    .absolute()
    .top_0()
    .right_0()
    .bottom_0()
    .w(px(SCROLLBAR_WIDTH))
}

/// `scrollbar` for a `uniform_list` tracking `handle`.
pub fn list_scrollbar(
    id: impl Into<gpui::ElementId>,
    handle: &gpui::UniformListScrollHandle,
    cx: &App,
) -> gpui::Canvas<(gpui::Hitbox, Entity<ScrollbarState>)> {
    let base = handle.0.borrow().base_handle.clone();
    scrollbar(id, &base, cx)
}

// ---- Islands ------------------------------------------------------------------------
//
// Kuana's look: content sits on rounded "islands" (the editor's background) that
// float on the chrome (the theme's panel color, faintly lit with the accent from
// above). The main window, Preferences and the pickers all follow it; menus,
// popovers and tooltips are `popover`s floating above everything.

/// An island's resting outline, the same at every visual effects level: the
/// chrome is kept close to the islands' color (`theme::MAX_CHROME_STEP`) and
/// their shadows are light, so the outline is what shows where an island ends.
pub fn island_edge(theme: &Theme) -> Hsla {
    Hsla::from(theme.border).alpha(0.6)
}

/// The outline of the split (pane or terminal group) that has the keyboard:
/// a hint of the accent, not a frame.
pub fn focused_edge(theme: &Theme) -> Hsla {
    Hsla::from(theme.accent).alpha(0.22)
}

/// Space between islands, and around them.
pub const GAP: f32 = 6.0;
pub const ISLAND_RADIUS: f32 = 12.0;
/// Content masks are square, so content that paints its own background
/// (editor, terminal) is kept this far inside an island's rounded corners.
pub const ISLAND_INSET: f32 = 4.0;

/// The chrome's background: the panel color, with a faint accent glow at the
/// top (more would tint the whole title bar) that has faded out by `fade`
/// (0–1) of the height; just the panel color below Full visual effects.
pub fn chrome_bg(theme: &Theme, fade: f32) -> gpui::Background {
    if !theme.gradients {
        return theme.panel.into();
    }
    let top = Hsla::from(theme.panel).blend(Hsla::from(theme.accent).alpha(0.02));
    gpui::linear_gradient(180., gpui::linear_color_stop(top, 0.), gpui::linear_color_stop(theme.panel, fade))
}

/// A soft drop shadow; `depth` 1 for islands, 2 for popovers, 3 for modals.
/// None with Minimal visual effects (borders separate the surfaces then).
pub fn soft_shadow(theme: &Theme, depth: u8) -> Vec<gpui::BoxShadow> {
    if theme.reduce_effects {
        return Vec::new();
    }
    // On dark chrome an island's shadow only darkens the gap around it, so it
    // stays light; things that float above the window need the depth.
    let (alpha, y, blur): (f32, f32, f32) = match (theme.appearance, depth) {
        (Appearance::Dark, 0 | 1) => (0.12, 2., 12.),
        (Appearance::Dark, 2) => (0.35, 8., 24.),
        (Appearance::Dark, _) => (0.45, 16., 48.),
        (Appearance::Light, 0 | 1) => (0.07, 2., 12.),
        (Appearance::Light, 2) => (0.1, 8., 24.),
        (Appearance::Light, _) => (0.13, 16., 48.),
    };
    vec![gpui::BoxShadow {
        color: gpui::hsla(0., 0., 0., alpha),
        offset: gpui::point(px(0.), px(y)),
        blur_radius: px(blur),
        spread_radius: px(0.),
    }]
}

/// A rounded card on the chrome: the sidebar, each pane, each bottom panel,
/// Preferences' pages, a picker's list.
pub fn island(theme: &Theme) -> Div {
    div()
        .relative()
        .flex()
        .flex_col()
        .p(px(ISLAND_INSET))
        .rounded(px(ISLAND_RADIUS))
        .bg(theme.background)
        .border_1()
        .border_color(island_edge(theme))
        .shadow(soft_shadow(theme, 1))
}

/// A card floating above the window: menus, completion, hover, toasts.
pub fn popover(theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(10.))
        .bg(theme.elevated)
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.85))
        .shadow(soft_shadow(theme, 2))
}

/// A container for related toggles, drawn like a segmented control; its
/// buttons use `segment_on` for the one that's active.
pub fn dock(theme: &Theme) -> Div {
    div().flex().flex_shrink_0().items_center().gap(px(2.)).p(px(2.)).rounded(px(9.)).bg(theme.hover)
}

pub fn segment_on<E: Styled>(el: E, theme: &Theme) -> E {
    el.bg(theme.background).shadow(soft_shadow(theme, 1))
}

/// A soft accent halo for focused fields and active controls; none with
/// Minimal visual effects (their accent border still marks them).
pub fn focus_glow(theme: &Theme) -> Vec<gpui::BoxShadow> {
    glow(theme, Hsla::from(theme.accent).alpha(0.22), 6., 1.)
}

/// A halo of `color` around an element, e.g. a status light; none with
/// Minimal visual effects.
pub fn glow(theme: &Theme, color: Hsla, blur: f32, spread: f32) -> Vec<gpui::BoxShadow> {
    if theme.reduce_effects {
        return Vec::new();
    }
    vec![gpui::BoxShadow {
        color,
        offset: gpui::point(px(0.), px(0.)),
        blur_radius: px(blur),
        spread_radius: px(spread),
    }]
}

/// The file-type icon for `path`, in the pack's own colors (see `file_icons`).
pub fn file_icon(path: &std::path::Path, size: Pixels, theme: &Theme) -> Img {
    img(file_icons::file_source(path, theme.appearance)).size(size).flex_shrink_0()
}

pub fn folder_icon(open: bool, size: Pixels, theme: &Theme) -> Img {
    img(file_icons::folder_source(open, theme.appearance)).size(size).flex_shrink_0()
}

pub fn icon_el(icon: Icon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg().path(icon.path()).size(size).flex_shrink_0().text_color(color)
}

/// A rounded text field wrapping a single-line editor.
pub fn input_box(editor: &Entity<Editor>, icon: Option<Icon>, window: &Window, cx: &App) -> Transition<Div> {
    let theme = Theme::get(cx);
    let focused = editor.read(cx).focus_handle(cx).is_focused(window);
    let field = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .h(px(32.))
        .px_2p5()
        .rounded(px(10.))
        .bg(theme.background)
        .when_some(icon, |el, icon| el.child(icon_el(icon, px(14.), theme.text_dim.into())))
        .child(div().flex_1().min_w_0().overflow_hidden().child(editor.clone()));
    focus_ring(field, ("input-focus", editor.entity_id().as_u64()), focused, theme)
}

/// A text field's border, accent with `focus_glow` while `focused`, fading in
/// and out. Every field draws its focus this way; `id` must be stable across frames.
pub fn focus_ring<E: Styled + IntoElement + 'static>(
    field: E,
    id: impl Into<gpui::ElementId>,
    focused: bool,
    theme: &Theme,
) -> Transition<E> {
    let (border, accent) = (Hsla::from(theme.border).alpha(0.75), Hsla::from(theme.accent).alpha(0.7));
    let glow = focus_glow(theme);
    field.border_1().with_transition(id, focused, transition::FOCUS, move |el, t| {
        el.border_color(mix(border, accent, t)).when(t > 0., |el| el.shadow(faded(glow, t)))
    })
}

/// `shadows` with their colors' alpha scaled by `t`, to fade them in or out.
pub fn faded(mut shadows: Vec<gpui::BoxShadow>, t: f32) -> Vec<gpui::BoxShadow> {
    for shadow in &mut shadows {
        shadow.color.a *= t;
    }
    shadows
}

/// A button: a clickable element whose hover fill fades in and out.
pub type Button = Transition<Stateful<Div>>;

/// Hover fills that fade, for elements with an id (buttons, chips, pills, list
/// rows), in place of `.hover(|s| s.bg(…))`; the transition is keyed by the
/// element's id. A fill set on the element itself (a selected row, `segment_on`)
/// wins and doesn't change on hover: GPUI doesn't expose a fill's color, so
/// there's nothing to fade from.
pub trait HoverFill: StatefulInteractiveElement + Styled + IntoElement + 'static {
    /// No fill at rest, `hover` under the mouse.
    fn hover_bg(self, hover: impl Into<Hsla>) -> Transition<Self> {
        let hover = hover.into();
        fade_fill(self, hover.alpha(0.), hover, transition::FOCUS)
    }

    /// `rest` easing to `hover` under the mouse, for elements with a resting fill.
    fn hover_fill(self, rest: impl Into<Hsla>, hover: impl Into<Hsla>) -> Transition<Self> {
        fade_fill(self, rest.into(), hover.into(), transition::FOCUS)
    }

    /// `hover_bg` for list rows, quicker.
    fn row_hover(self, hover: impl Into<Hsla>) -> Transition<Self> {
        let hover = hover.into();
        fade_fill(self, hover.alpha(0.), hover, transition::ROW)
    }
}

impl<E: StatefulInteractiveElement + Styled + IntoElement + 'static> HoverFill for E {}

fn fade_fill<E: HoverFill>(mut el: E, rest: Hsla, hover: Hsla, duration: std::time::Duration) -> Transition<E> {
    let id = el.interactivity().element_id.clone().expect("hover fills need an element id");
    el.with_hover_transition(gpui::ElementId::NamedChild(Box::new(id), "hover".into()), duration, move |mut el, t| {
        if el.style().background.is_some() { el } else { el.bg(mix(rest, hover, t)) }
    })
}

/// A small toggle like JetBrains' "Cc", "W" and ".*" search options.
pub fn toggle(id: &'static str, label: &'static str, active: bool, tooltip: &'static str, cx: &App) -> Button {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .h(px(24.))
        .min_w(px(26.))
        .px_1p5()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.))
        .font_family(theme.mono_font.clone())
        .text_size(px(12.))
        .cursor_pointer()
        .when(active, |el| el.bg(theme.accent_soft).text_color(theme.accent))
        .when(!active, |el| el.text_color(theme.text_muted))
        .tooltip(move |_, cx| tooltip_view(tooltip, cx))
        .child(label)
        .hover_bg(theme.hover)
}

/// An on/off switch (draw only; the caller handles clicks).
pub fn switch(on: bool, theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w(px(28.))
        .h(px(16.))
        .p(px(2.))
        .rounded_full()
        .when(on, |el| el.bg(theme.accent).justify_end().shadow(focus_glow(theme)))
        .when(!on, |el| el.bg(theme.active))
        .child(div().size(px(12.)).rounded_full().bg(if on { theme.background } else { theme.text_dim }))
}

/// A bottom panel's header row (Problems, Git log): its title at the left, a
/// hairline under it, room above and below its controls, and the close button
/// (`panel_close_button`) at the same place in every panel.
pub fn panel_header(theme: &Theme) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .h(px(38.))
        .flex_shrink_0()
        .pl(px(10.))
        .pr(px(4.))
        .border_b_1()
        .border_color(theme.border_subtle)
}

/// The close button at the right of a `panel_header`.
pub fn panel_close_button(id: impl Into<gpui::ElementId>, cx: &App) -> Button {
    let theme = Theme::get(cx);
    icon_button(id, Icon::X, theme.text_dim.into(), cx)
        .tooltip(|_, cx| tooltip_view("Hide", cx))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

pub fn icon_button(id: impl Into<gpui::ElementId>, icon: Icon, color: Hsla, cx: &App) -> Button {
    let theme = Theme::get(cx);
    div()
        .id(id)
        .size(px(26.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .active(|s| s.bg(theme.active))
        .child(icon_el(icon, px(15.), color))
        .hover_bg(theme.hover)
}

pub fn text_button(id: impl Into<gpui::ElementId>, label: &'static str, primary: bool, cx: &App) -> Button {
    let theme = Theme::get(cx);
    let (rest, hover) = if primary {
        (Hsla::from(theme.accent_soft), Hsla::from(theme.accent).alpha(0.24))
    } else {
        (Hsla::from(theme.hover), Hsla::from(theme.active))
    };
    div()
        .id(id)
        .h(px(28.))
        .px_3()
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(14.))
        .cursor_pointer()
        .font_weight(gpui::FontWeight::MEDIUM)
        .when(primary, |el| el.text_color(theme.accent).border_1().border_color(Hsla::from(theme.accent).alpha(0.3)))
        .when(!primary, |el| el.text_color(theme.text))
        .child(label)
        .hover_fill(rest, hover)
}

/// A small outlined pill for a list row's actions (Merge, Edit, Pop…),
/// usually shown on the row's hover; `danger` colors it like a destructive action.
pub fn row_pill(id: impl Into<gpui::ElementId>, label: &'static str, danger: bool, cx: &App) -> Button {
    let theme = Theme::get(cx);
    let hover_text = if danger { theme.error } else { theme.text };
    div()
        .id(id)
        .px_1p5()
        .h(px(20.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .rounded(px(10.))
        .cursor_pointer()
        .text_size(px(11.5))
        .text_color(theme.text_muted)
        .border_1()
        .border_color(theme.border)
        .hover(move |s| s.text_color(hover_text))
        .child(label)
        .hover_fill(theme.elevated, theme.hover)
}

/// A key hint chip, e.g. "⌘⇧F".
pub fn kbd(text: impl Into<SharedString>, cx: &App) -> impl IntoElement {
    let theme = Theme::get(cx);
    div()
        .px_1p5()
        .rounded(px(5.))
        .bg(theme.hover)
        .border_1()
        .border_color(theme.border_subtle)
        .text_size(px(11.5))
        .text_color(theme.text_muted)
        .child(text.into())
}

/// The shortcut currently bound to `action`, formatted for this platform.
pub fn binding_label(action: &dyn gpui::Action, window: &Window) -> Option<String> {
    // Look the binding up as if an editor were focused, so editor-only
    // shortcuts show too. The first built-in binding is the primary one.
    let context = gpui::KeyContext::parse("Workspace Editor").ok()?;
    let bindings = window.bindings_for_action_in_context(action, context);
    // The user's own binding (keymap.json) is the one to show.
    let binding = bindings
        .iter()
        .rev()
        .find(|b| b.meta() == Some(crate::keymap::USER_BINDING))
        .or(bindings.first())?;
    let parts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|k| shortcut_label(&k.to_string()))
        .collect();
    Some(parts.join(" "))
}

/// Renders a keybinding like `secondary-shift-o` the platform's way.
pub fn shortcut_label(keys: &str) -> String {
    let mac = cfg!(target_os = "macos");
    // GPUI already renders macOS keystrokes with symbols (e.g. "⇧enter");
    // only the key name needs tidying.
    if mac && !keys.contains('-') {
        let split = keys.char_indices().find(|(_, c)| !"⌃⌥⇧⌘".contains(*c)).map_or(keys.len(), |(i, _)| i);
        let (mods, key) = keys.split_at(split);
        let key = match key {
            "enter" => "↩".to_string(),
            "escape" => "Esc".to_string(),
            "tab" => "⇥".to_string(),
            "space" => "Space".to_string(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        };
        return format!("{mods}{key}");
    }
    let tokens: Vec<&str> = if let Some(rest) = keys.strip_suffix("--") {
        // A binding on the '-' key itself.
        let mut t: Vec<&str> = rest.split('-').collect();
        t.push("-");
        t
    } else {
        keys.split('-').collect()
    };
    let parts: Vec<String> = tokens
        .iter()
        .map(|k| match (*k, mac) {
            ("secondary" | "cmd" | "platform", true) => "⌘".into(),
            ("secondary", false) | ("ctrl", false) => "Ctrl".into(),
            ("shift", true) => "⇧".into(),
            ("shift", false) => "Shift".into(),
            ("alt", true) => "⌥".into(),
            ("alt", false) => "Alt".into(),
            ("ctrl", true) => "⌃".into(),
            ("enter", true) => "↩".into(),
            ("backspace", true) => "⌫".into(),
            ("up", _) => "↑".into(),
            ("down", _) => "↓".into(),
            ("left", _) => "←".into(),
            ("right", _) => "→".into(),
            ("escape", _) => "Esc".into(),
            (k, _) if k.len() == 1 => k.to_uppercase(),
            (k, _) => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        })
        .collect();
    if mac {
        // macOS orders modifiers ⌃⌥⇧⌘ and joins without separators.
        let (mods, keys): (Vec<&String>, Vec<&String>) =
            parts.iter().partition(|p| ["⌃", "⌥", "⇧", "⌘"].contains(&p.as_str()));
        let order = ["⌃", "⌥", "⇧", "⌘"];
        let mut mods = mods;
        mods.sort_by_key(|m| order.iter().position(|o| o == m));
        mods.into_iter().chain(keys).map(String::as_str).collect()
    } else {
        parts.join("+")
    }
}

/// What a drag (a tab, a tree row) shows under the pointer.
pub struct DragLabel(pub SharedString);

impl Render for DragLabel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        popover(theme)
            .px_3()
            .py_1()
            .rounded(px(14.))
            .text_size(px(theme.ui_font_size))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

pub struct Tooltip(pub SharedString);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tooltip_box(Theme::get(cx)).child(self.0.clone())
    }
}

fn tooltip_box(theme: &Theme) -> Div {
    popover(theme)
        // Block, not the popover's flex column: in a column GPUI sizes the
        // box for one unwrapped line, and wrapped text spills out of it.
        .block()
        .max_w(px(420.))
        .px_2()
        .py_1()
        .rounded(px(8.))
        .text_size(px(12.))
        .text_color(theme.text)
        .font_family(theme.ui_font.clone())
}

pub fn tooltip_view(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    let text = text.into();
    cx.new(|_| Tooltip(text)).into()
}

/// A tooltip whose text changes while it's shown (GPUI builds a tooltip's
/// view once per hover): `text` is read again every quarter second.
pub struct LiveTooltip {
    text: fn() -> String,
    tick: Option<gpui::Task<()>>,
}

impl Render for LiveTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(250)).await;
            this.update(cx, |_, cx| cx.notify()).ok();
        }));
        tooltip_box(Theme::get(cx)).child((self.text)())
    }
}

pub fn live_tooltip_view(text: fn() -> String, cx: &mut App) -> AnyView {
    cx.new(|_| LiveTooltip { text, tick: None }).into()
}

/// The floating panel used by all modal popups: chrome, like the window
/// around the islands. Long content goes into an `island` inside it.
pub fn modal_panel(cx: &App) -> Div {
    let theme = Theme::get(cx);
    div()
        .flex()
        .flex_col()
        .bg(chrome_bg(theme, 0.5))
        .border_1()
        .border_color(Hsla::from(theme.border).alpha(0.85))
        .rounded(px(16.))
        .shadow(soft_shadow(theme, 3))
        .overflow_hidden()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_effect_levels_drop_gradients_then_shadows_and_glows() {
        let mut theme = Theme::blank_for_tests();
        theme.panel = gpui::rgba(0x202020ff);
        theme.accent = gpui::rgba(0xff8000ff);
        // Full.
        assert_ne!(chrome_bg(&theme, 0.5), gpui::Background::from(theme.panel));
        assert!(!soft_shadow(&theme, 1).is_empty() && !focus_glow(&theme).is_empty());
        // Reduced: solid, but shadows and glows stay.
        theme.gradients = false;
        assert_eq!(chrome_bg(&theme, 0.5), gpui::Background::from(theme.panel));
        assert!(!soft_shadow(&theme, 1).is_empty() && !focus_glow(&theme).is_empty());
        // Minimal.
        theme.reduce_effects = true;
        assert!(soft_shadow(&theme, 3).is_empty());
        assert!(focus_glow(&theme).is_empty());
        assert!(glow(&theme, theme.accent.into(), 4., 0.).is_empty());
    }
}
