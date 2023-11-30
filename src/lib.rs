use std::sync::Arc;

use connection::managed::{LoadError, ManagedConnection};
use tracing::Span;
use ui::lce::Loadable;

pub mod connection;

#[derive(Debug, Clone)]
pub struct SharedSpan(Option<Arc<Span>>);

#[must_use = "A Span Guard must be held until you wish to exit the span"]
pub struct SharedSpanGuard<'s>(Option<tracing::span::Entered<'s>>);

impl From<Span> for SharedSpan {
    fn from(value: Span) -> Self {
        SharedSpan(Some(Arc::new(value)))
    }
}

impl From<Option<Span>> for SharedSpan {
    fn from(value: Option<Span>) -> Self {
        SharedSpan(value.map(Arc::new))
    }
}

impl SharedSpan {
    pub fn enter(&self) -> SharedSpanGuard<'_> {
        SharedSpanGuard(self.0.as_ref().map(|it| it.enter()))
    }
}

pub mod ui {
    pub mod horizontal_bar {
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
    }

    pub mod lce {
        use std::{marker::PhantomData, ops::DerefMut};

        pub type DynamicLce<C, E> = Lce<Box<dyn Loadable<C, E>>, C, E>;

        impl<C, E> Loadable<C, E> for Box<dyn Loadable<C, E>> {
            fn ready(&mut self) -> bool {
                self.as_mut().ready()
            }

            fn take(&mut self) -> Result<C, E> {
                self.as_mut().take()
            }
        }

        pub enum Lce<L, C, E> {
            Waiting,
            Loading(L),
            Content(C),
            Error(E),
        }

        impl<L, C, E> Default for Lce<L, C, E> {
            fn default() -> Self {
                Self::Waiting
            }
        }

        pub struct MappedLoadable<F, L, C, E, Co, Eo>(L, F, PhantomData<(C, E, Co, Eo)>);

        impl<F, L, C, E, Co, Eo> Loadable<Co, Eo> for MappedLoadable<F, L, C, E, Co, Eo>
        where
            L: Loadable<C, E>,
            F: FnMut(Result<C, E>) -> Result<Co, Eo>,
        {
            fn take(&mut self) -> Result<Co, Eo> {
                (&mut self.1)(self.0.take())
            }

            fn ready(&mut self) -> bool {
                self.0.ready()
            }
        }

        pub trait Loadable<C, E> {
            fn ready(&mut self) -> bool;
            fn take(&mut self) -> Result<C, E>;
        }

        pub trait LoadableExt<C, E>: Sized {
            fn map_res<F, Co, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, Co, Eo>
            where
                F: FnMut(Result<C, E>) -> Result<Co, Eo>;

            // These should be doable, but are not currently due to instability of RPITIT
            // fn map<F, Co>(
            //     self,
            //     convert: F,
            // ) -> MappedLoadable<impl FnOnce(Result<C, E>) -> Result<Co, E>, Self, C, E, Co, E>
            // where
            //     F: FnOnce(C) -> Co,
            // {
            //     self.map_res(|it: Result<C, E>| it.map(convert))
            // }

            // fn map_err<F, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, C, Eo>
            // where
            //     F: FnOnce(E) -> Eo,
            // {
            //     self.map_res(|it: E| it.map_err(convert))
            // }
        }

        impl<L, C, E> LoadableExt<C, E> for L
        where
            L: Loadable<C, E>,
        {
            fn map_res<F, Co, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, Co, Eo>
            where
                F: FnMut(Result<C, E>) -> Result<Co, Eo>,
            {
                MappedLoadable(self, convert, PhantomData::default())
            }
        }

        impl<L, C, E> Lce<L, C, E>
        where
            L: Loadable<C, E>,
        {
            fn process(&mut self) {
                match std::mem::replace(self, Lce::Waiting) {
                    Lce::Loading(mut loading) => {
                        if loading.ready() {
                            match loading.take() {
                                Ok(content) => {
                                    *self = Lce::Content(content);
                                }

                                Err(e) => {
                                    *self = Lce::Error(e);
                                }
                            }
                        } else {
                            *self = Lce::Loading(loading);
                        }
                    }

                    it => {
                        *self = it;
                    }
                }
            }

            #[inline]
            pub fn to_dyn_lce(self) -> DynamicLce<C, E>
            where
                L: 'static,
            {
                match self {
                    Lce::Waiting => Lce::Waiting,
                    Lce::Loading(l) => {
                        Lce::Loading(Box::new(l) as Box<dyn Loadable<C, E> + 'static>)
                    }
                    Lce::Content(c) => Lce::Content(c),
                    Lce::Error(e) => Lce::Error(e),
                }
            }

            pub fn ui<R>(
                &mut self,
                ui: &mut egui::Ui,
                on_waiting: impl FnOnce(&mut egui::Ui) -> R,
                on_loading: impl FnOnce(&mut egui::Ui, &mut L) -> R,
                on_content: impl FnOnce(&mut egui::Ui, &mut C) -> R,
                on_error: impl FnOnce(&mut egui::Ui, &mut E) -> R,
            ) -> egui::InnerResponse<R> {
                self.process();

                ui.allocate_ui(ui.available_size(), |ui| match self {
                    Lce::Waiting => on_waiting(ui),
                    Lce::Loading(l) => on_loading(ui, l),
                    Lce::Content(c) => on_content(ui, c),
                    Lce::Error(e) => on_error(ui, e),
                })
            }

            pub fn show(&mut self, ui: &mut egui::Ui, on_waiting: impl FnOnce(&mut egui::Ui))
            where
                for<'a> &'a mut L: egui::Widget,
                for<'a> &'a mut C: egui::Widget,
                for<'a> &'a mut E: egui::Widget,
            {
                self.ui(
                    ui,
                    on_waiting,
                    |ui, l| {
                        ui.add(l);
                    },
                    |ui, c| {
                        ui.add(c);
                    },
                    |ui, e| {
                        ui.add(e);
                    },
                );
            }
        }
    }
}

impl<S> Loadable<Vec<ManagedConnection<S>>, LoadError>
    for crate::connection::managed::LoadingDevices<S>
{
    fn ready(&mut self) -> bool {
        crate::connection::managed::LoadingDevices::ready(self)
    }

    fn take(&mut self) -> Result<Vec<ManagedConnection<S>>, LoadError> {
        crate::connection::managed::LoadingDevices::take(self)
    }
}
