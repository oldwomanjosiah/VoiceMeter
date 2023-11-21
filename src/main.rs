use std::{sync::Arc, time::Duration};

use connection::*;
use cpal::traits::StreamTrait;
use eyre::{Context, Report, Result};

mod connection;

fn log_timing<R>(name: &str, block: impl FnOnce() -> R) -> R {
    let start = std::time::Instant::now();

    let res = block();
    tracing::info!("{name} took {}ms", start.elapsed().as_millis());
    res
}

struct ChannelAnalysis<S: StreamTrait> {
    pub connection: ChannelConnection<S, i32>,
    pub buffer_duration: Duration,
    pub smooth_duration: Duration,
    pub decay_rate: f32,
    max_decay: Vec<i32>,
}

struct BarInfo {
    jagged: f32,
    smooth: f32,
    decaying: f32,
}

impl<S: StreamTrait> ChannelAnalysis<S> {
    pub fn new(mut connection: ChannelConnection<S, i32>) -> Self {
        let max_decay = std::iter::repeat(0)
            .take(connection.channels().len())
            .collect();
        Self {
            connection,
            buffer_duration: Duration::from_secs(3),
            smooth_duration: Duration::from_millis(150),
            decay_rate: 0.4,
            max_decay,
        }
    }

    pub fn process(&mut self, dt: Duration) {
        self.connection.process();

        let decay_factor = self.decay_rate.powf(dt.as_secs_f32() / 2.0);

        for decaying in &mut self.max_decay {
            *decaying = ((*decaying as f32) * decay_factor).floor() as _;
        }
    }

    pub fn with_bar_info(&mut self, frame_time: Duration, mut thunk: impl FnMut(usize, BarInfo)) {
        let combine = |acc: i32, it: &i32| acc.max(it.saturating_abs());

        for (idx, channel) in self.connection.channels().iter_mut().enumerate() {
            let jagged_raw = channel.take_duration(frame_time).fold(0, combine);

            channel.trim_backbuffer_duration(self.buffer_duration);

            let smooth_raw = {
                let iter = channel.backbuffer();
                let len = iter.len();
                iter.skip(len.saturating_sub(channel.samples_for_duration(self.smooth_duration)))
                    .fold(0, combine)
            };

            let decaying_raw = jagged_raw.max(self.max_decay[idx]);
            self.max_decay[idx] = decaying_raw;

            fn convert(raw: i32) -> f32 {
                raw as f32 / i32::MAX as f32
            }

            let info = BarInfo {
                jagged: convert(jagged_raw),
                smooth: convert(smooth_raw),
                decaying: convert(decaying_raw),
            };

            thunk(idx, info);
        }
    }
}

struct App<H, S: StreamTrait> {
    host: H,
    repaint: Option<Arc<dyn Fn() + Send + Sync + 'static>>,
    devices: Vec<ChannelAnalysis<S>>,
}

type HostInputStream<H> =
    <<H as cpal::traits::HostTrait>::Device as cpal::traits::DeviceTrait>::Stream;

impl<H: cpal::traits::HostTrait> App<H, HostInputStream<H>> {
    fn reload_connections(&mut self) {
        log_timing("reloading connection", || {
            let Some(repaint) = self.repaint.as_ref() else {
                tracing::warn!("could not get repainter");
                return;
            };

            let devices = match log_timing("getting devices", || self.host.devices()) {
                Ok(devices) => devices,
                Err(e) => {
                    tracing::error!("Could not get device list! {e}");
                    return;
                }
            };

            log_timing("clearing devices", || {
                self.devices.clear();
            });

            log_timing("building devices", || {
                for device in devices {
                    let repaint = {
                        let parent = repaint.clone();
                        move || parent()
                    };

                    match connection::ChannelConnection::build_connection(&device, repaint, true) {
                        Ok(mut conn) => {
                            if let Err(e) = conn.play() {
                                tracing::warn!("Could not start playing stream: {e}");
                            }
                            self.devices.push(ChannelAnalysis::new(conn));
                        }
                        Err(e) => {
                            tracing::error!("Could not build device! Got {e}");
                        }
                    }
                }
            });
        });
    }
}

struct HorizontalBarWidget {
    info: BarInfo,
    label: Option<egui::WidgetText>,

    warn: (f32, egui::Color32),
    error: (f32, egui::Color32),

    height: f32,
    desired_width: Option<f32>,
}

impl HorizontalBarWidget {
    pub fn new(info: BarInfo) -> Self {
        Self {
            info,
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

impl egui::Widget for HorizontalBarWidget {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        let fill = self.select_color(self.info.smooth, ui.style().visuals.selection.bg_fill);
        let decay_color = self.select_color(self.info.decaying, fill);

        let max_text_with = self
            .desired_width
            .unwrap_or(ui.available_size_before_wrap().x)
            - (ui.spacing().item_spacing.x * 2.0);
        let text_layout = self
            .label
            .map(|it| it.into_galley(ui, Some(false), max_text_with, egui::TextStyle::Button));

        let size = egui::vec2(
            self.desired_width
                .unwrap_or_else(|| ui.available_width().max(256.0)),
            self.height,
        );

        let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());

        {
            let painter = ui.painter().with_clip_rect(rect);
            let bg = ui.style().visuals.faint_bg_color;

            // Background
            painter.rect_filled(rect, 0.0, bg);

            // Main Bar
            let (main_bar_rect, _) = rect.split_left_right_at_fraction(self.info.smooth);
            painter.rect_filled(main_bar_rect, 0.0, fill);

            // Jagged Display
            let jagged_fill = fill.gamma_multiply(0.4).to_opaque();
            let jagged_rect = {
                let top_pt = rect
                    .split_left_right_at_fraction(self.info.jagged)
                    .0
                    .right_top();
                let bottom_pt = main_bar_rect.right_bottom();
                egui::Rect::from_points(&[top_pt, bottom_pt])
            };

            painter.rect_filled(jagged_rect, 0.0, jagged_fill); //egui::Color32::RED);

            let (decay_rect, _) = rect.split_left_right_at_fraction(self.info.decaying);
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

impl<H, S: StreamTrait> App<H, S> {
    fn new(host: H) -> Self {
        App {
            host,
            repaint: None,
            devices: Default::default(),
        }
    }

    fn set_repaint_from(&mut self, ctx: &egui::Context) -> bool {
        match self.repaint.as_ref() {
            Some(_) => false,
            None => {
                let ctx = ctx.clone();
                let new_repaint = Arc::new(move || ctx.request_repaint());
                self.repaint = Some(new_repaint);
                true
            }
        }
    }
}

impl<H> eframe::App for App<H, HostInputStream<H>>
where
    H: cpal::traits::HostTrait,
{
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frametime = Duration::from_secs_f32(ctx.input(|it| it.unstable_dt));

        if self.set_repaint_from(ctx) {
            self.reload_connections();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("VoiceMeter");

            if ui.button("reload").clicked() {
                self.reload_connections();
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for connection in &mut self.devices {
                    connection.process(frametime);

                    ui.separator();
                    ui.label(connection.connection.name());

                    connection.with_bar_info(frametime, |idx, info| {
                        ui.add(HorizontalBarWidget::new(info).with_label(format!("{idx}")));
                    });
                }
            });
        });
    }
}

fn main() -> Result<()> {
    {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_timer(tracing_subscriber::fmt::time::uptime())
                    .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
            )
            .init();

        color_eyre::install()?;
    }

    let options = eframe::NativeOptions {
        vsync: false,
        ..Default::default()
    };

    eframe::run_native(
        "VoiceMeter",
        options,
        Box::new(|_| {
            let app = App::new(log_timing("getting host", || {
                cpal::host_from_id(cpal::HostId::Wasapi)
                    .wrap_err("getting host to initalize")
                    .unwrap()
            }));
            Box::new(app)
        }),
    )
    .unwrap();

    Ok(())
}
