//! Real application rendering to a wgpu texture without an OS window or display.
//!
//! This uses the same EditorApp, document loading, highlighting, layout, font
//! atlas and egui-wgpu renderer as the native app. It is not a mockup renderer.

use crate::{app::WorkspaceApp, preferences::Theme};
use eframe::{egui, egui_wgpu, wgpu};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;

#[derive(Default)]
pub(crate) struct CaptureOptions {
    pub theme: Theme,
    pub vim: bool,
    pub folder: Option<PathBuf>,
    pub tabs: Vec<PathBuf>,
    pub cursor_byte: Option<usize>,
    pub startup_warning: bool,
}

pub(crate) fn screenshot(
    output: &Path,
    input: &Path,
    benchmark: bool,
    options: CaptureOptions,
) -> Result<(), Box<dyn Error>> {
    if benchmark && options.startup_warning {
        return Err("Startup-warning capture is not a file benchmark".into());
    }
    // Normal capture fails early for a missing fixture. Only the explicit
    // startup-warning diagnostic renders native startup's recovery behavior.
    let input_bytes = if options.startup_warning {
        std::fs::metadata(input).map_or(0, |metadata| metadata.len())
    } else {
        std::fs::metadata(input)?.len()
    };
    let mut instance_options = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_options.backends = wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY);
    let instance = wgpu::Instance::new(instance_options);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
        ..Default::default()
    }))?;
    let adapter_info = adapter.get_info();
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("SimpTxt headless renderer"),
        ..Default::default()
    }))?;
    let format = wgpu::TextureFormat::Rgba8Unorm;
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("SimpTxt screenshot"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&Default::default());
    let mut renderer =
        egui_wgpu::Renderer::new(&device, format, egui_wgpu::RendererOptions::PREDICTABLE);
    let ctx = egui::Context::default();
    ctx.set_pixels_per_point(1.0);
    let started = Instant::now();
    let mut app = if options.startup_warning {
        WorkspaceApp::from_startup_path(&ctx, Some(input.to_path_buf()), false)
    } else {
        WorkspaceApp::headless(&ctx, input)
    };
    app.configure_capture(
        options.theme,
        options.vim,
        options.folder,
        options.tabs,
        &ctx,
    );
    let mut frames = 0;
    let mut ready_frames = 0;
    let mut first_frame_ms = None;
    let mut editable_ms = None;
    let mut exercise_step = 0;
    let mut cursor_applied = options.cursor_byte.is_none();
    let mut edit_frame_ms = Vec::new();
    let deadline = Duration::from_secs(if benchmark { 120 } else { 20 });
    loop {
        if started.elapsed() > deadline {
            return Err(format!(
                "The document or highlighting did not become ready within {} seconds",
                deadline.as_secs()
            )
            .into());
        }
        let frame_started = Instant::now();
        if !cursor_applied && app.ready_to_capture() {
            app.capture_cursor(options.cursor_byte.expect("capture cursor was requested"));
            cursor_applied = true;
        }
        let exercise_this_frame = benchmark && ready_frames >= 3 && exercise_step < 6;
        if exercise_this_frame {
            app.headless_exercise(&ctx, exercise_step);
        }
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WIDTH as f32, HEIGHT as f32),
            )),
            time: Some(started.elapsed().as_secs_f64()),
            focused: true,
            ..Default::default()
        };
        let mut output = ctx.run_ui(raw_input, |ui| app.render(ui));
        for (id, deltas) in output.textures_delta.set.drain() {
            for delta in deltas {
                renderer.update_texture(&device, &queue, id, &delta);
            }
        }
        let paint_jobs =
            ctx.tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [WIDTH, HEIGHT],
            pixels_per_point: output.pixels_per_point,
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("SimpTxt offscreen frame"),
        });
        let mut commands =
            renderer.update_buffers(&device, &queue, &mut encoder, &paint_jobs, &screen);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("SimpTxt app UI"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            renderer.render(&mut pass.forget_lifetime(), &paint_jobs, &screen);
        }
        commands.push(encoder.finish());
        queue.submit(commands);
        if benchmark {
            // Include GPU completion, not merely command submission, in reported
            // preview/edit frame times. Input uses EditorApp's real edit paths.
            device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(Duration::from_secs(20)),
            })?;
        }
        for id in output.textures_delta.free.drain() {
            renderer.free_texture(&id);
        }
        frames += 1;
        if benchmark && first_frame_ms.is_none() {
            let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
            first_frame_ms = Some(elapsed);
            println!("SIMPTXT_UI_PREVIEW first_gpu_frame_ms={elapsed:.3}");
        }
        if app.ready_to_capture() {
            ready_frames += 1;
            if benchmark && editable_ms.is_none() {
                let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
                editable_ms = Some(elapsed);
                println!("SIMPTXT_UI_EDITABLE open_to_ready_ms={elapsed:.3}");
            }
        } else {
            ready_frames = 0;
        }
        if exercise_this_frame {
            let elapsed = frame_started.elapsed().as_secs_f64() * 1_000.0;
            let label = [
                "begin_insert",
                "begin_undo",
                "middle_insert",
                "middle_undo",
                "end_insert",
                "end_undo",
            ][exercise_step];
            println!("SIMPTXT_UI_FRAME operation={label} input_to_gpu_ms={elapsed:.3}");
            edit_frame_ms.push(elapsed);
            exercise_step += 1;
        }
        // Let both asynchronous document/highlight work and layout settle.
        if ready_frames >= 3 && frames >= 5 && (!benchmark || exercise_step == 6) {
            break;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    let bytes_per_row = WIDTH * 4;
    let padded_bytes_per_row = bytes_per_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("SimpTxt screenshot readback"),
        size: u64::from(padded_bytes_per_row) * u64::from(HEIGHT),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("SimpTxt screenshot copy"),
    });
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: None,
            },
        },
        texture.size(),
    );
    queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
    device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(20)),
    })?;
    rx.recv_timeout(Duration::from_secs(20))??;
    let mapped = buffer.slice(..).get_mapped_range()?;
    let mut rgba = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for row in mapped.chunks(padded_bytes_per_row as usize) {
        rgba.extend_from_slice(&row[..bytes_per_row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    image::save_buffer_with_format(
        output,
        &rgba,
        WIDTH,
        HEIGHT,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    if benchmark {
        let max_ms = edit_frame_ms.iter().copied().fold(0.0_f64, f64::max);
        let mean_ms = edit_frame_ms.iter().sum::<f64>() / edit_frame_ms.len() as f64;
        println!(
            "SIMPTXT_UI_BENCHMARK_OK input_bytes={} preview_first_gpu_frame_ms={:.3} \
             editing_ready_ms={:.3} edit_frames={} mean_input_to_gpu_ms={mean_ms:.3} \
             max_input_to_gpu_ms={max_ms:.3} input_method=in_process",
            input_bytes,
            first_frame_ms.expect("at least one frame"),
            editable_ms.expect("document became ready"),
            edit_frame_ms.len(),
        );
    }
    println!(
        "SIMPTXT_HEADLESS_OK {}x{} frames={} adapter={:?} backend={:?} output={}",
        WIDTH,
        HEIGHT,
        frames,
        adapter_info.name,
        adapter_info.backend,
        output.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_warning_capture_cannot_be_reported_as_a_file_benchmark() {
        let options = CaptureOptions {
            startup_warning: true,
            ..Default::default()
        };
        let error = screenshot(
            Path::new("not-written.png"),
            Path::new("not-opened.txt"),
            true,
            options,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Startup-warning capture is not a file benchmark"
        );
    }

    #[test]
    fn normal_capture_rejects_missing_fixture_before_rendering_or_writing() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("missing.txt");
        let output = temp.path().join("not-written.png");
        let error = screenshot(&output, &input, false, CaptureOptions::default()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::NotFound
        );
        assert!(!input.exists());
        assert!(!output.exists());
    }
}
