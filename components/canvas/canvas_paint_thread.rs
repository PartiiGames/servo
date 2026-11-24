/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::borrow::ToOwned;
use std::{f32, thread};

use crossbeam_channel::{Sender, select, unbounded};
use euclid::default::{Rect, Size2D, Transform2D};
use log::warn;
use paint_api::CrossProcessPaintApi;
use pixels::{Snapshot, SnapshotPixelFormat};
use profile_traits::mem::{
    ProcessReports, ProfilerChan, Report, ReportsChan, perform_memory_report,
};
use rustc_hash::FxHashMap;
use servo_base::generic_channel::GenericSender;
use servo_base::{Epoch, generic_channel};
use servo_canvas_traits::ConstellationCanvasMsg;
use servo_canvas_traits::canvas::*;
use webrender_api::ImageKey;

use crate::canvas_data::*;

pub struct CanvasPaintThread {
    canvases: FxHashMap<CanvasId, Canvas>,
    next_canvas_id: CanvasId,
    paint_api: CrossProcessPaintApi,
}

impl CanvasPaintThread {
    fn new(paint_api: CrossProcessPaintApi) -> CanvasPaintThread {
        CanvasPaintThread {
            canvases: FxHashMap::default(),
            next_canvas_id: CanvasId(0),
            paint_api,
        }
    }

    /// Creates a new `CanvasPaintThread` and returns an `IpcSender` to
    /// communicate with it.
    pub fn start(
        paint_api: CrossProcessPaintApi,
        mem_profiler_chan: ProfilerChan,
    ) -> (Sender<ConstellationCanvasMsg>, GenericSender<CanvasMsg>) {
        let (ipc_sender, ipc_receiver) = generic_channel::channel::<CanvasMsg>().unwrap();
        let msg_receiver = ipc_receiver.route_preserving_errors();
        let (create_sender, create_receiver) = unbounded();
        let registration = mem_profiler_chan.prepare_memory_reporting(
            "canvas".into(),
            create_sender.clone(),
            ConstellationCanvasMsg::CollectMemoryReport,
        );
        thread::Builder::new()
            .name("Canvas".to_owned())
            .spawn(move || {
                let _registration = registration;
                let mut canvas_paint_thread = CanvasPaintThread::new(
                    paint_api);
                loop {
                    select! {
                        recv(msg_receiver) -> msg => {
                            match msg {
                                Ok(Ok((canvas_id, command))) => {
                                    // Partii (clips): every page's 2D canvases share this thread, and a shown
                                    // canvas's frame waits for its UpdateImage here: a long command for another
                                    // canvas (a contact sheet, a toBlob readback) delays it.
                                    // PARTII_CLIP_STATS=1 logs commands over 4 ms.
                                    canvas_paint_thread.process_command_timed(command, canvas_id);
                                },
                                Ok(Err(e)) => {
                                    warn!("CanvasPaintThread message deserialization error: {e:?}");
                                }
                                Err(_disconnected) => {
                                    warn!("CanvasMsg receiver disconnected");
                                    break;
                                },
                            }
                        }
                        recv(create_receiver) -> msg => {
                            match msg {
                                Ok(ConstellationCanvasMsg::Create { sender: creator, size }) => {
                                    if let Err(error) = creator.send(canvas_paint_thread.create_canvas(size)) {
                                        warn!("Create canvas response failed ({error})");
                                    }
                                },
                                Ok(ConstellationCanvasMsg::CollectMemoryReport(sender)) => {
                                    canvas_paint_thread.collect_memory_reports(sender);
                                },
                                Ok(ConstellationCanvasMsg::Exit(exit_sender)) => {
                                    let _ = exit_sender.send(());
                                    break;
                                },
                                Err(e) => {
                                    warn!("Error on CanvasPaintThread receive ({})", e);
                                    break;
                                },
                            }
                        }
                    }
                }
            })
            .expect("Thread spawning failed");

        (create_sender, ipc_sender)
    }

    #[servo_tracing::instrument(skip_all)]
    pub fn create_canvas(&mut self, size: Size2D<u64>) -> Option<CanvasId> {
        let canvas_id = self.next_canvas_id;
        self.next_canvas_id.0 += 1;

        let canvas = Canvas::new(size, self.paint_api.clone())?;
        self.canvases.insert(canvas_id, canvas);
        if slow_log() {
            eprintln!("PARTII_CANVAS_NEW canvas={canvas_id:?} size={}x{}", size.width, size.height);
        }

        Some(canvas_id)
    }

    fn collect_memory_reports(&self, sender: ReportsChan) {
        perform_memory_report(|ops| {
            let reports = self
                .canvases
                .iter()
                .flat_map(|(canvas_id, canvas)| canvas.collect_memory_report(*canvas_id, ops))
                .collect();
            sender.send(ProcessReports::new(reports));
        });
    }

    #[servo_tracing::instrument(
        skip_all,
        fields(message = message.to_string())
    )]
    fn process_command(&mut self, message: CanvasCommand, canvas_id: CanvasId) {
        match message {
            CanvasCommand::Recreate(size) => {
                if slow_log() {
                    if let Some(size) = size {
                        eprintln!("PARTII_CANVAS_NEW canvas={canvas_id:?} size={}x{}", size.width, size.height);
                    }
                }
                self.canvas(canvas_id).recreate(size)
            },
            CanvasCommand::Destroy => {
                self.canvases.remove(&canvas_id);
            },
            CanvasCommand::SetImageKey(image_key) => {
                self.canvas(canvas_id).set_image_key(image_key);
            },
            CanvasCommand::FillText(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).fill_text(
                    text_bounds,
                    text_runs,
                    fill_or_stroke_style,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::StrokeText(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).stroke_text(
                    text_bounds,
                    text_runs,
                    fill_or_stroke_style,
                    line_options,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::FillRect(
                rect,
                style,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).fill_rect(
                    &rect,
                    style,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::StrokeRect(
                rect,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).stroke_rect(
                    &rect,
                    style,
                    line_options,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::ClearRect(ref rect, transform) => {
                self.canvas(canvas_id).clear_rect(rect, transform)
            },
            CanvasCommand::FillPath(
                style,
                path,
                fill_rule,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).fill_path(
                    &path,
                    fill_rule,
                    style,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::StrokePath(
                path,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ) => {
                self.canvas(canvas_id).stroke_path(
                    &path,
                    style,
                    line_options,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::ClipPath(path, fill_rule, transform) => {
                self.canvas(canvas_id)
                    .clip_path(&path, fill_rule, transform);
            },
            CanvasCommand::DrawImage(
                snapshot,
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ) => self.canvas(canvas_id).draw_image(
                snapshot.to_owned(),
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ),
            CanvasCommand::DrawImageFromWebGL(
                WebGLReadback(receiver, size),
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ) => match receiver.recv() {
                Ok((data, alpha_mode)) => {
                    let mut data = data.to_vec();
                    pixels::flip_y_rgba8_image_inplace(size, &mut data);
                    let snapshot =
                        Snapshot::from_vec(size, SnapshotPixelFormat::RGBA, alpha_mode, data);
                    self.canvas(canvas_id).draw_image(
                        snapshot,
                        dest_rect,
                        source_rect,
                        smoothing_enabled,
                        shadow_options,
                        composition_options,
                        transform,
                    );
                },
                Err(error) => warn!("DrawImageFromWebGL: no readback ({error:?})"),
            },
            CanvasCommand::DrawEmptyImage(
                image_size,
                dest_rect,
                source_rect,
                shadow_options,
                composition_options,
                transform,
            ) => self.canvas(canvas_id).draw_image(
                Snapshot::cleared(image_size),
                dest_rect,
                source_rect,
                false,
                shadow_options,
                composition_options,
                transform,
            ),
            CanvasCommand::DrawImageInOther(
                other_canvas_id,
                dest_rect,
                source_rect,
                smoothing,
                shadow_options,
                composition_options,
                transform,
            ) => {
                let snapshot = self
                    .canvas(canvas_id)
                    .read_pixels(Some(source_rect.to_u32()));
                self.canvas(other_canvas_id).draw_image(
                    snapshot,
                    dest_rect,
                    source_rect,
                    smoothing,
                    shadow_options,
                    composition_options,
                    transform,
                );
            },
            CanvasCommand::GetImageData(dest_rect, sender) => {
                let snapshot = self.canvas(canvas_id).read_pixels(dest_rect);
                if let Err(error) = sender.send(snapshot.to_shared()) {
                    warn!("GetImageData response failed ({error})");
                }
            },
            CanvasCommand::GetImageDataToCallback(callback) => {
                let snapshot = self.canvas(canvas_id).read_pixels(None);
                if let Err(error) = callback.send(snapshot.to_shared()) {
                    warn!("GetImageDataToCallback failed ({error})");
                }
            },
            CanvasCommand::PutImageData(rect, snapshot) => {
                let prof = servo_base::partii_prof::start();
                self.canvas(canvas_id)
                    .put_image_data(snapshot.to_owned(), rect);
                servo_base::partii_prof::stop(servo_base::partii_prof::Counter::PutCanvas, prof);
            },
            CanvasCommand::UpdateImage(canvas_epoch) => {
                let prof = servo_base::partii_prof::start();
                self.canvas(canvas_id).update_image_rendering(canvas_epoch);
                servo_base::partii_prof::stop(servo_base::partii_prof::Counter::UpdateCanvas, prof);
            },
            CanvasCommand::PopClips(clips) => self.canvas(canvas_id).pop_clips(clips),
            CanvasCommand::ProcessBatchMessages(messages) => {
                for message in messages {
                    self.process_command_timed(message, canvas_id);
                }
            },
        }
    }

    /// Partii (clips): process_command, logging a command over 4 ms with PARTII_CLIP_STATS=1.
    fn process_command_timed(&mut self, message: CanvasCommand, canvas_id: CanvasId) {
        if !slow_log() || matches!(message, CanvasCommand::ProcessBatchMessages(..)) {
            return self.process_command(message, canvas_id);
        }
        let name = message.to_string();
        let started = std::time::Instant::now();
        self.process_command(message, canvas_id);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        if ms > 4.0 {
            eprintln!("PARTII_CANVAS_SLOW cmd={name} canvas={canvas_id:?} ms={ms:.1}");
        }
    }

    fn canvas(&mut self, canvas_id: CanvasId) -> &mut Canvas {
        self.canvases.get_mut(&canvas_id).expect("Bogus canvas id")
    }
}

#[cfg_attr(
    feature = "vello",
    expect(
        clippy::large_enum_variant,
        reason = "Current consensus is on keeping enum instead of boxing it. https://github.com/servo/servo/pull/46863"
    )
)]
enum Canvas {
    #[cfg(feature = "vello")]
    Vello(CanvasData<crate::vello_backend::VelloDrawTarget>),
    VelloCPU(CanvasData<crate::vello_cpu_backend::VelloCPUDrawTarget>),
}

impl Canvas {
    fn new(size: Size2D<u64>, paint_api: CrossProcessPaintApi) -> Option<Self> {
        match servo_config::pref!(dom_canvas_backend)
            .to_lowercase()
            .as_str()
        {
            #[cfg(feature = "vello")]
            "vello" => Some(Self::Vello(CanvasData::new(size, paint_api))),
            _ => Some(Self::VelloCPU(CanvasData::new(size, paint_api))),
        }
    }

    fn collect_memory_report(
        &self,
        canvas_id: CanvasId,
        ops: &mut malloc_size_of::MallocSizeOfOps,
    ) -> Vec<Report> {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.collect_memory_report(canvas_id, ops),
            Canvas::VelloCPU(canvas_data) => canvas_data.collect_memory_report(canvas_id, ops),
        }
    }

    fn set_image_key(&mut self, image_key: ImageKey) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.set_image_key(image_key),
            Canvas::VelloCPU(canvas_data) => canvas_data.set_image_key(image_key),
        }
    }

    fn pop_clips(&mut self, clips: usize) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.pop_clips(clips),
            Canvas::VelloCPU(canvas_data) => canvas_data.pop_clips(clips),
        }
    }

    fn stroke_text(
        &mut self,
        text_bounds: Rect<f64>,
        text_runs: Vec<TextRun>,
        fill_or_stroke_style: FillOrStrokeStyle,
        line_options: LineOptions,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.stroke_text(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.stroke_text(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn fill_text(
        &mut self,
        text_bounds: Rect<f64>,
        text_runs: Vec<TextRun>,
        fill_or_stroke_style: FillOrStrokeStyle,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.fill_text(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.fill_text(
                text_bounds,
                text_runs,
                fill_or_stroke_style,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn fill_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => {
                canvas_data.fill_rect(rect, style, shadow_options, composition_options, transform)
            },
            Canvas::VelloCPU(canvas_data) => {
                canvas_data.fill_rect(rect, style, shadow_options, composition_options, transform)
            },
        }
    }

    fn stroke_rect(
        &mut self,
        rect: &Rect<f32>,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.stroke_rect(
                rect,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.stroke_rect(
                rect,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn fill_path(
        &mut self,
        path: &Path,
        fill_rule: FillRule,
        style: FillOrStrokeStyle,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.fill_path(
                path,
                fill_rule,
                style,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.fill_path(
                path,
                fill_rule,
                style,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn stroke_path(
        &mut self,
        path: &Path,
        style: FillOrStrokeStyle,
        line_options: LineOptions,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.stroke_path(
                path,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.stroke_path(
                path,
                style,
                line_options,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn clear_rect(&mut self, rect: &Rect<f32>, transform: Transform2D<f64>) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.clear_rect(rect, transform),
            Canvas::VelloCPU(canvas_data) => canvas_data.clear_rect(rect, transform),
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn draw_image(
        &mut self,
        snapshot: Snapshot,
        dest_rect: Rect<f64>,
        source_rect: Rect<f64>,
        smoothing_enabled: bool,
        shadow_options: ShadowOptions,
        composition_options: CompositionOptions,
        transform: Transform2D<f64>,
    ) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.draw_image(
                snapshot,
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ),
            Canvas::VelloCPU(canvas_data) => canvas_data.draw_image(
                snapshot,
                dest_rect,
                source_rect,
                smoothing_enabled,
                shadow_options,
                composition_options,
                transform,
            ),
        }
    }

    fn read_pixels(&mut self, read_rect: Option<Rect<u32>>) -> Snapshot {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.read_pixels(read_rect),
            Canvas::VelloCPU(canvas_data) => canvas_data.read_pixels(read_rect),
        }
    }

    fn clip_path(&mut self, path: &Path, fill_rule: FillRule, transform: Transform2D<f64>) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.clip_path(path, fill_rule, transform),
            Canvas::VelloCPU(canvas_data) => canvas_data.clip_path(path, fill_rule, transform),
        }
    }

    fn put_image_data(&mut self, snapshot: Snapshot, rect: Rect<u32>) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.put_image_data(snapshot, rect),
            Canvas::VelloCPU(canvas_data) => canvas_data.put_image_data(snapshot, rect),
        }
    }

    fn update_image_rendering(&mut self, canvas_epoch: Option<Epoch>) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.update_image_rendering(canvas_epoch),
            Canvas::VelloCPU(canvas_data) => canvas_data.update_image_rendering(canvas_epoch),
        }
    }

    fn recreate(&mut self, size: Option<Size2D<u64>>) {
        match self {
            #[cfg(feature = "vello")]
            Canvas::Vello(canvas_data) => canvas_data.recreate(size),
            Canvas::VelloCPU(canvas_data) => canvas_data.recreate(size),
        }
    }
}

/// Partii (clips): PARTII_CLIP_STATS=1 logs canvas commands over 4 ms (PARTII_CANVAS_SLOW).
fn slow_log() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("PARTII_CLIP_STATS").is_some());
    *ON
}
