//! The Problems panel.

use super::*;

impl Workspace {
    pub(super) fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let run_count = self.run_problems_shown(cx).len();
        let editor = self.active_editor().cloned();
        let diag_count = editor.as_ref().map_or(0, |e| e.read(cx).doc.read(cx).diagnostics.len());

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("PROBLEMS"),
            )
            .child(
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(theme.active)
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child((diag_count + run_count).to_string()),
            )
            .child(div().flex_1())
            .child(self.icon_button("close-problems", Icon::X, false, cx, |ws, _, window, cx| {
                ws.toggle_problems(&ToggleProblems, window, cx)
            }));

        // Rows, top to bottom: the file's title when the last Run reported
        // problems too, the file's problems, then the Run's title and its
        // problems. Only the rows on screen are built.
        let file_title = run_count > 0 && diag_count > 0;
        let count = file_title as usize + diag_count + if run_count > 0 { 1 + run_count } else { 0 };
        let list = if count == 0 {
            div()
                .flex()
                .items_center()
                .gap_2()
                .p_3()
                .text_color(theme.text_dim)
                .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                .child("No problems detected")
                .into_any_element()
        } else {
            uniform_list(
                "problems-list",
                count,
                cx.processor(move |ws, range: std::ops::Range<usize>, _window, cx| {
                    let theme = Theme::get(cx).clone();
                    let editor = ws.active_editor().cloned();
                    let run_problems: Vec<run::OutputProblem> = ws.run_problems_shown(cx).into_iter().cloned().collect();
                    let run_name = ws.run_problems.as_ref().map(|(name, _)| name.clone()).unwrap_or_default();
                    let mut rows = Vec::with_capacity(range.len());
                    for ix in range {
                        let mut ix = ix;
                        if file_title {
                            if ix == 0 {
                                let name = editor.as_ref().and_then(|e| e.read(cx).doc.read(cx).path.as_deref().map(folder_name)).unwrap_or_default();
                                rows.push(problem_section("file-problems".into(), Icon::File, name, &theme).into_any_element());
                                continue;
                            }
                            ix -= 1;
                        }
                        let diagnostic = editor.as_ref().and_then(|e| {
                            let doc = e.read(cx).doc.read(cx);
                            let d = doc.diagnostics.get(ix)?;
                            let row = doc.row_of(d.range.start);
                            let col = doc.slice(doc.line_start(row)..d.range.start).chars().count();
                            Some((d.clone(), row + 1, col + 1))
                        });
                        if let Some((d, line, col)) = diagnostic {
                            let (icon, color) = severity_icon(d.severity, &theme);
                            let target = d.range.start;
                            let editor = editor.clone();
                            // One row per problem: a multi-line message (Pyright's
                            // details) reads as one line, like the inline hint.
                            let message = d.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
                            rows.push(
                                problem_row(SharedString::from(format!("problem-{ix}")), icon, color, message, &theme)
                                    .on_click(move |_, window, cx| {
                                        if let Some(editor) = editor.as_ref() {
                                            editor.update(cx, |e, cx| e.go_to(target, window, cx));
                                        }
                                    })
                                    .child(div().text_color(theme.text_dim).flex_shrink_0().child(SharedString::from(d.rule.to_string())))
                                    .child(div().flex_1())
                                    .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(format!("Ln {line}, Col {col}")))
                                    .into_any_element(),
                            );
                            continue;
                        }
                        ix -= diag_count;
                        if ix == 0 {
                            rows.push(problem_section("run-problems".into(), Icon::Play, run_name.clone(), &theme).into_any_element());
                            continue;
                        }
                        let Some(p) = run_problems.get(ix - 1) else {
                            rows.push(div().into_any_element());
                            continue;
                        };
                        let (icon, color) = severity_icon(p.severity, &theme);
                        let place = match p.column {
                            Some(column) => format!("{}:{}:{column}", ws.display_rel(&p.path), p.line),
                            None => format!("{}:{}", ws.display_rel(&p.path), p.line),
                        };
                        let (path, line, column) = (p.path.clone(), p.line, p.column);
                        rows.push(
                            problem_row(SharedString::from(format!("run-problem-{ix}")), icon, color, p.message.clone(), &theme)
                                .on_click(cx.listener(move |ws, _, window, cx| ws.open_at(path.clone(), Some(line), column, window, cx)))
                                .child(div().flex_1())
                                .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(place))
                                .into_any_element(),
                        );
                    }
                    rows
                }),
            )
            .flex_1()
            .py_1()
            .into_any_element()
        };

        island(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(header)
            .child(list)
            .child(self.resize_handle(Edge::Problems, cx))
    }
}

/// A heading in the Problems panel (the file, the last Run).
pub(super) fn problem_section(id: SharedString, icon: Icon, title: String, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(26.))
        .px_3()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_dim)
        .child(icon_el(icon, px(12.), theme.text_dim.into()))
        .child(title)
}

/// A problem in the Problems panel: severity icon and message; callers add
/// where it is.
pub(super) fn problem_row(id: SharedString, icon: Icon, color: Hsla, message: String, theme: &Theme) -> crate::ui::Button {
    div()
        .id(id)
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .h(px(26.))
        .mx_1p5()
        .px_2()
        .rounded(px(6.))
        .cursor_pointer()
        .hover_bg(theme.hover)
        .child(icon_el(icon, px(14.), color))
        .child(div().min_w_0().text_color(theme.text).truncate().child(message))
}
