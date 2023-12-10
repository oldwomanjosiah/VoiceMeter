#![cfg_attr(profile = "release", windows_subsystem = "windows")]

use std::{sync::Arc, time::Duration};

use cpal::Device;
use eyre::{Context, Result};
use rustfft::{num_complex::Complex32, Fft, FftPlanner};
use voicemeter::{
    connection::{
        buffer::{self, SampleBuffer},
        managed::{LoadError, ManagedConnection, Manager},
    },
    fft,
    ui::{DynamicLce, HorizontalBarWidget, Lce, LoadableExt},
    HzExt,
};

struct ChannelAnalysisSet {
    devices: Vec<ChannelAnalysis>,
    detail_view: Option<(usize, usize)>,
    fft: fft::Fft<f32>,
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
    pub fn new(from: Vec<ManagedConnection<i32>>, fft: fft::Fft<f32>) -> Self {
        Self {
            devices: from.into_iter().map(ChannelAnalysis::new).collect(),
            detail_view: None,
            fft,
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
        let mut sample_rate = 0.hz();

        if let Some(channel) = self.detail_channel_mut() {
            sample_rate = channel.sample_rate();

            ui.label(format!("Sample Rate: {}", sample_rate));
        }

        egui_plot::Plot::new("fft_plot").show(ui, |plot| {
            plot.set_plot_bounds(egui_plot::PlotBounds::from_min_max(
                [0.0, 0.0],
                [f64::INFINITY, 1.0],
            ));
            plot.set_auto_bounds([true, false].into());

            let Some((dev, channel)) = self.detail_view else {
                return;
            };
            let buffer = &self.devices[dev].connection.buffers()[channel];

            let Some(points) = self.fft.process_samples(buffer.backbuffer().copied()) else {
                return;
            };

            let mut mag = Vec::with_capacity(points.len());
            let mut max = (0.hz(), 0.0);

            for point in points {
                use egui_plot::PlotPoint;

                // let x = (points_len as f64).log10() - ((points_len - idx) as f64).log10();
                let x = ((point.index + 1) as f64).log10();
                // let x = idx as f32;

                let norm = point.amplitude();

                if norm > max.1 {
                    max = (point.frequency(buffer.sample_rate()), norm);
                }

                mag.push(PlotPoint::new(x, norm));
            }

            tracing::info!("Max: {} {}", max.0, max.1);

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
    builder: fft::Builder<f32>,
}

impl App {
    fn reload_connections(&mut self) {
        let loader = self.manager.load();
        let builder = self.builder.clone();
        let mapped_loader = loader.map_res(move |res| {
            res.map(|connections| ChannelAnalysisSet::new(connections, builder.build()))
        });

        self.connections = Lce::Loading(mapped_loader).to_dyn_lce();
    }
}

impl App {
    fn new(manager: Manager<i32>) -> Self {
        App {
            manager,
            connections: Default::default(),
            builder: fft::Fft::builder().with_size(fft::FftSize::Samples1024),
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

    if let Ok(value) = std::env::var("RUST_LOG") {
        println!("Value: {value}");
    } else {
        println!("Could not get RUST_LOG var");
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
