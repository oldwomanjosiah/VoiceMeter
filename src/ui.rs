mod horizontal_bar;
mod lce;

pub use horizontal_bar::*;
pub use horizontal_split::*;
pub use lce::*;

mod horizontal_split {
    use std::hash::Hash;

    use egui::{Id, InnerResponse, Ui};

    pub struct HorizontalSplit {
        id: Id,
    }

    impl HorizontalSplit {
        pub fn new<IdSource: Hash>(id_source: IdSource) -> Self {
            let id = Id::new(id_source);

            Self { id }
        }

        pub fn show<Rl, Rr>(
            self,
            ui: &mut Ui,
            left_content: impl FnOnce(&mut Ui) -> Rl,
            right_content: impl FnOnce(&mut Ui) -> Rr,
        ) -> InnerResponse<(Rl, Rr)> {
            self.show_with(ui, (), |ui, _| left_content(ui), |ui, _| right_content(ui))
        }

        pub fn show_with<C, Rl, Rr>(
            self,
            ui: &mut Ui,
            mut ctx: C,
            left_content: impl FnOnce(&mut Ui, &mut C) -> Rl,
            right_content: impl FnOnce(&mut Ui, &mut C) -> Rr,
        ) -> InnerResponse<(Rl, Rr)> {
            let total_rect = ui.available_rect_before_wrap();
            let (left_rect, right_rect) = total_rect.split_left_right_at_fraction(0.5);
            let left_resp = ui.allocate_ui_at_rect(left_rect, |ui| left_content(ui, &mut ctx));
            let right_resp = ui.allocate_ui_at_rect(right_rect, |ui| right_content(ui, &mut ctx));

            InnerResponse {
                inner: (left_resp.inner, right_resp.inner),
                response: left_resp.response.union(right_resp.response),
            }
        }
    }
}
