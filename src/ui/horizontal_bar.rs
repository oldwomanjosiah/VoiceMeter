use egui::{Response, Sense, TextStyle, Ui, Widget};

pub struct HorizontalBarWidget {
    jagged: f32,
    smooth: f32,
    peak: f32,
    label: Option<egui::WidgetText>,

    warn: (f32, egui::Color32),
    error: (f32, egui::Color32),

    height: f32,
    desired_width: Option<f32>,
}

impl HorizontalBarWidget {
    pub fn new(jagged: f32, smooth: f32, peak: f32) -> Self {
        Self {
            jagged,
            smooth,
            peak,
            label: None,

            warn: (0.75, egui::Color32::YELLOW),
            error: (0.95, egui::Color32::RED),

            height: 32.0,
            desired_width: None,
        }
    }

    pub fn with_label(self, text: impl Into<egui::WidgetText>) -> Self {
        Self {
            label: Some(text.into()),
            ..self
        }
    }

    pub fn with_height(self, height: f32) -> Self {
        Self { height, ..self }
    }

    pub fn with_width(self, width: f32) -> Self {
        Self {
            desired_width: Some(width),
            ..self
        }
    }

    fn select_color(&self, progress: f32, default: egui::Color32) -> egui::Color32 {
        if progress > self.error.0 {
            self.error.1
        } else if progress > self.warn.0 {
            self.warn.1
        } else {
            default
        }
    }
}

impl Widget for HorizontalBarWidget {
    fn ui(self, ui: &mut Ui) -> Response {
        let fill = self.select_color(self.smooth, ui.style().visuals.selection.bg_fill);
        let decay_color = self.select_color(self.peak, fill);

        let max_text_with = self
            .desired_width
            .unwrap_or(ui.available_size_before_wrap().x)
            - (ui.spacing().item_spacing.x * 2.0);
        let text_layout = self
            .label
            .map(|it| it.into_galley(ui, Some(false), max_text_with, TextStyle::Button));

        let size = egui::vec2(
            self.desired_width
                .unwrap_or_else(|| ui.available_width().max(256.0)),
            self.height,
        );

        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());

        {
            let painter = ui.painter().with_clip_rect(rect);
            let bg = ui.style().visuals.faint_bg_color;

            // Background
            painter.rect_filled(rect, 0.0, bg);

            // Main Bar
            let (main_bar_rect, _) = rect.split_left_right_at_fraction(self.smooth);
            painter.rect_filled(main_bar_rect, 0.0, fill);

            // Jagged Display
            let jagged_fill = fill.gamma_multiply(0.4).to_opaque();
            let jagged_rect = {
                let top_pt = rect.split_left_right_at_fraction(self.jagged).0.right_top();
                let bottom_pt = main_bar_rect.right_bottom();
                egui::Rect::from_points(&[top_pt, bottom_pt])
            };

            painter.rect_filled(jagged_rect, 0.0, jagged_fill); //egui::Color32::RED);

            let (decay_rect, _) = rect.split_left_right_at_fraction(self.peak);
            painter.line_segment(
                [decay_rect.right_top(), decay_rect.right_bottom()],
                (2.0, decay_color),
            );
        }

        if let Some(galley) = text_layout {
            let size = galley.size();
            galley.paint_with_visuals(
                ui.painter(),
                egui::pos2(
                    rect.min.x + ui.spacing().item_spacing.x,
                    rect.min.y + (rect.size().y - size.y) / 2.0,
                ),
                ui.visuals().noninteractive(),
            );
        }

        response
    }
}
