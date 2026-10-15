//! The Problems panel's place in the bottom area (see `crate::problems_panel`).

use super::*;

impl Workspace {
    /// The panel on an island, as tall as the bottom area, with its grab edge.
    pub(super) fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        island(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(cached(&self.problems))
            .child(self.resize_handle(Edge::Problems, cx))
    }
}
