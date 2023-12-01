use std::{sync::Arc, time::Duration};

use cpal::Device;
use eyre::{Context, Result};
use rustfft::{num_complex::Complex32, Fft, FftPlanner};
use voicemeter::{
    connection::{
        buffer::{self, SampleBuffer},
        managed::{LoadError, ManagedConnection, Manager},
    },
    ui::{DynamicLce, HorizontalBarWidget, Lce, LoadableExt},
};

struct ChannelAnalysisSet {
    devices: Vec<ChannelAnalysis>,
    detail_view: Option<(usize, usize)>,

    forier: Arc<dyn Fft<f32>>,
    forier_buffer: Vec<Complex32>,
    forier_scratch: Vec<Complex32>,
}

struct ChannelAnalysis {
    pub connection: ManagedConnection<i32>,
    pub buffer_duration: Duration,
    pub smooth_duration: Duration,
    pub decay_rate: f32,
    max_decay: Vec<i32>,
}

#[derive(Debug, Clone, Copy)]
struct BarInfo<'o> {
    #[allow(unused)]
    pub from: &'o SampleBuffer<i32>,
    pub jagged: f32,
    pub smooth: f32,
    pub decaying: f32,
}

impl ChannelAnalysisSet {
    pub fn new(from: Vec<ManagedConnection<i32>>, planner: &mut FftPlanner<f32>) -> Self {
        let buffer_len = 2 << 11;
        let forier = planner.plan_fft_forward(buffer_len);

        let forier_buffer = vec![Complex32::new(0.0, 0.0); buffer_len];
        let forier_scratch = vec![Complex32::new(0.0, 0.0); forier.get_inplace_scratch_len()];

        Self {
            devices: from.into_iter().map(ChannelAnalysis::new).collect(),

            detail_view: None,

            forier,
            forier_buffer,
            forier_scratch,
        }
    }

    pub fn rectify_selected(&mut self) {
        self.detail_view = self.detail_view.and_then(|(device, channel)| {
            let Some(dev) = self.devices.get(device) else {
                return None;
            };

            (channel < dev.connection.channels()).then_some((device, channel))
        });
    }

    pub fn detail_channel_mut(&mut self) -> Option<&mut SampleBuffer<i32>> {
        self.rectify_selected();

        let (device, channel) = self.detail_view?;

        Some(&mut self.devices[device].connection.buffers()[channel])
    }

    pub fn process_fft(
        &mut self,
    ) -> Option<impl ExactSizeIterator<Item = (usize, Complex32)> + '_> {
        self.rectify_selected();
        let (device, channel) = self.detail_view?;
        let data = &mut self.devices[device].connection.buffers()[channel];

        let buffer_len = self.forier_buffer.len();
        let backbuffer_len = data.backbuffer_len();
        if backbuffer_len < buffer_len {
            return None;
        }

        // let skip_amount = data.backbuffer_len().saturating_sub(buffer_len);

        // let backbuffer = data.backbuffer().skip(skip_amount).take(buffer_len);

        // let initial_idx = (backbuffer.len() - buffer_len) / 2;

        // if backbuffer.len() < buffer_len {
        //     tracing::warn!(len = %backbuffer.len(), expected = %buffer_len, "Backbuffer does not contain enough data!");
        // }

        // let iter = std::iter::repeat(0)
        //     .take(initial_idx)
        //     .chain(backbuffer.copied())
        //     .chain(std::iter::repeat(0));

        let iter = data
            .backbuffer()
            .skip(backbuffer_len.saturating_sub(buffer_len))
            .copied();

        for (idx, (val, buf)) in iter.zip(&mut self.forier_buffer).enumerate() {
            fn convert(val: i32) -> f32 {
                (val as f32) / (i32::MAX as f32)
            }

            fn real(val: f32) -> Complex32 {
                Complex32::new(val, 0.0)
            }

            fn filter(idx: usize, len: usize, val: f32) -> f32 {
                let half_len = len as f32 / 2.0;
                let percent = 1.0 - ((idx as f32 - half_len).abs() / half_len);
                assert!(0.0 <= percent && percent <= 1.0);
                // val * percent
                val
            }

            *buf = real(filter(idx, buffer_len, convert(val)));
        }

        self.forier
            .process_with_scratch(&mut self.forier_buffer, &mut self.forier_scratch);

        let scale = (buffer_len as f32).sqrt();

        struct Alternating<I> {
            next_back: bool,
            inner: I,
        }

        impl<I> Iterator for Alternating<I>
        where
            I: DoubleEndedIterator<Item = Complex32>,
        {
            type Item = I::Item;

            fn next(&mut self) -> Option<Self::Item> {
                let a = self.inner.next()?;
                let b = self.inner.next_back()?;
                Some((a + b) / 2.0)
            }
        }

        impl<I> ExactSizeIterator for Alternating<I>
        where
            Self: Iterator,
            I: ExactSizeIterator,
        {
            fn len(&self) -> usize {
                self.inner.len() / 2
            }
        }

        impl<I> Alternating<I> {
            fn new(inner: I) -> Self {
                Self {
                    inner,
                    next_back: false,
                }
            }
        }

        Some(
            Alternating::new(
                self.forier_buffer
                    .iter()
                    .map(move |it| (it).scale(1.0 / scale)),
            )
            .enumerate(),
        )
    }

    pub fn showing_detail(&self) -> bool {
        self.detail_view.is_some()
    }

    pub fn list_panel(&mut self, ui: &mut egui::Ui) {
        let frame_time = Duration::from_secs_f32(ui.ctx().input(|it| it.unstable_dt));

        egui::ScrollArea::vertical().show(ui, |ui| {
            for (dev_idx, device) in self.devices.iter_mut().enumerate() {
                device.process(frame_time);

                ui.separator();
                ui.heading(device.connection.name());

                device.with_bar_info(frame_time, |channel_idx, channel_info| {
                    let sense = ui
                        .push_id(dev_idx * 31 + channel_idx, |ui| {
                            ui.add(
                                HorizontalBarWidget::new(
                                    channel_info.jagged,
                                    channel_info.smooth,
                                    channel_info.decaying,
                                )
                                .with_label(channel_idx.to_string()),
                            );
                        })
                        .response
                        .interact(egui::Sense::click());

                    if sense.clicked() {
                        self.detail_view = Some((dev_idx, channel_idx));
                    }

                    if sense.hovered()
                        || self
                            .detail_view
                            .is_some_and(|it| it.0 == dev_idx && it.1 == channel_idx)
                    {
                        ui.label("Selected TODO fix this");
                    }
                });
            }
        });
    }

    pub fn detail_view(&mut self, ui: &mut egui::Ui) {
        egui_plot::Plot::new("fft_plot").show(ui, |plot| {
            plot.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                [0.0, 0.0],
                [f64::INFINITY, 1.0],
            ));
            plot.set_auto_bounds([true, false].into());

            let Some(points) = self.process_fft() else {
                // ui.label("No FFT");
                return;
            };

            let points_len = points.len();
            let mut mag = Vec::with_capacity(points.len());

            for (idx, point) in points {
                use egui_plot::PlotPoint;

                // let x = (points_len as f64).log10() - ((points_len - idx) as f64).log10();
                let x = ((idx + 1) as f64).log10();
                // let x = idx as f32;

                mag.push(PlotPoint::new(x, point.norm()));
            }

            use egui::Color32;
            use egui_plot::Line;
            use egui_plot::PlotPoints;

            plot.line(Line::new(PlotPoints::Owned(mag)).color(Color32::RED));
        });
    }
}

impl egui::Widget for &mut ChannelAnalysisSet {
    fn ui(self, ui: &mut egui::Ui) -> egui::Response {
        if self.showing_detail() {
            voicemeter::ui::HorizontalSplit::new("detail_view")
                .show_with(
                    ui,
                    self,
                    |ui, this| {
                        this.list_panel(ui);
                    },
                    |ui, this| {
                        this.detail_view(ui);
                    },
                )
                .response
        } else {
            ui.allocate_ui(ui.available_size_before_wrap(), |ui| self.list_panel(ui))
                .response
        }
    }
}

impl ChannelAnalysis {
    pub fn new(connection: ManagedConnection<i32>) -> Self {
        let max_decay = std::iter::repeat(0).take(connection.channels()).collect();

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

        for (idx, channel) in self.connection.buffers().iter_mut().enumerate() {
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
                from: channel,
                jagged: convert(jagged_raw),
                smooth: convert(smooth_raw),
                decaying: convert(decaying_raw),
            };

            thunk(idx, info);
        }
    }
}

struct App {
    manager: Manager<i32>,
    connections: DynamicLce<ChannelAnalysisSet, LoadError>,
    fft_planner: FftPlanner<f32>,
}

impl App {
    fn reload_connections(&mut self) {
        let loader = self.manager.load();
        let mapped_loader = loader.map_res(|res| {
            let mut planner = FftPlanner::new();
            res.map(|connections| ChannelAnalysisSet::new(connections, &mut planner))
        });

        self.connections = Lce::Loading(mapped_loader).to_dyn_lce();
    }
}

impl App {
    fn new(manager: Manager<i32>) -> Self {
        App {
            manager,
            connections: Default::default(),
            fft_planner: FftPlanner::new(),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frametime = Duration::from_secs_f32(ctx.input(|it| it.unstable_dt));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("VoiceMeter");

            if ui.button("reload").clicked() {
                self.reload_connections();
            }

            if matches!(self.connections, Lce::Waiting) {
                self.reload_connections();
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                self.connections.ui(
                    ui,
                    |ui| {
                        ui.heading("Waiting");
                    },
                    |ui, _l| {
                        ui.horizontal(|ui| {
                            ui.heading("Loading");
                            ui.spinner();
                        });
                    },
                    |ui, c| {
                        ui.add(c);
                        // for connection in c {
                        //     connection.process(frametime);

                        //     ui.separator();
                        //     ui.label(connection.connection.name());

                        //     connection.with_bar_info(frametime, |idx, info| {
                        //         ui.add(
                        //             voicemeter::ui::HorizontalBarWidget::new(
                        //                 info.jagged,
                        //                 info.smooth,
                        //                 info.decaying,
                        //             )
                        //             .with_label(format!("{idx}")),
                        //         );
                        //     });
                        // }
                    },
                    |ui, e| {
                        ui.heading("Error");
                        ui.code(e.to_string());
                    },
                );
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
        Box::new(|ctx| {
            let host = cpal::host_from_id(cpal::HostId::Wasapi)
                .wrap_err("Getting host")
                .unwrap();

            let notifier = {
                let ctx = ctx.egui_ctx.clone();
                let callback = move || {
                    ctx.request_repaint();
                };
                Arc::new(callback)
            };

            let manager =
                voicemeter::connection::managed::Manager::new(host, Some("Wasapi"), notifier);

            let app = App::new(manager);
            Box::new(app)
        }),
    )
    .unwrap();

    Ok(())
}
