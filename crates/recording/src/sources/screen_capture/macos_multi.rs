use super::*;

pub(super) struct Composite {
    frames: Mutex<Vec<Option<arc::R<cm::SampleBuf>>>>,
    pool: arc::R<cv::PixelBufPool>,
    params: Arc<CapturerRebuildParams>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

unsafe impl Send for Composite {}
unsafe impl Sync for Composite {}

impl Drop for Composite {
    fn drop(&mut self) {
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            worker.abort();
        }
    }
}

struct PixelLock<'a>(&'a mut cv::PixelBuf, cv::pixel_buffer::LockFlags);

impl<'a> PixelLock<'a> {
    fn new(buf: &'a mut cv::PixelBuf, flags: cv::pixel_buffer::LockFlags) -> anyhow::Result<Self> {
        unsafe { buf.lock_base_addr(flags) }.result()?;
        Ok(Self(buf, flags))
    }
}

impl Drop for PixelLock<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.0.unlock_lock_base_addr(self.1) };
    }
}

fn tile(index: usize, count: usize, width: usize, height: usize) -> (usize, usize, usize, usize) {
    let rows = count.div_ceil(2);
    let cell_width = width / 2 / 2 * 2;
    let cell_height = height / rows / 2 * 2;
    (
        index % 2 * cell_width,
        index / 2 * cell_height,
        cell_width,
        cell_height,
    )
}

fn blit(dst: &mut cv::PixelBuf, src: &cv::PixelBuf, x: usize, y: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        src.pixel_format() == cv::PixelFormat::_420V && src.plane_count() == 2,
        "Expected NV12 window frame"
    );
    anyhow::ensure!(
        x + src.width() <= dst.width() && y + src.height() <= dst.height(),
        "Window frame exceeds composite bounds"
    );
    let mut source = src.retained();
    let locked = PixelLock::new(&mut source, cv::pixel_buffer::LockFlags::READ_ONLY)?;
    for plane in 0..2 {
        let rows = locked.0.plane_height(plane);
        let source_stride = locked.0.plane_bytes_per_row(plane);
        let destination_stride = dst.plane_bytes_per_row(plane);
        let destination_y = if plane == 0 { y } else { y / 2 };
        let source_ptr = locked.0.plane_base_address(plane);
        let destination_ptr = dst.plane_base_address(plane).cast_mut();
        anyhow::ensure!(
            !source_ptr.is_null() && !destination_ptr.is_null(),
            "Missing pixel plane"
        );
        anyhow::ensure!(
            source_stride >= src.width() && destination_stride >= x + src.width(),
            "Invalid pixel stride"
        );
        for row in 0..rows {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source_ptr.add(row * source_stride),
                    destination_ptr.add((destination_y + row) * destination_stride + x),
                    src.width(),
                );
            }
        }
    }
    Ok(())
}

impl Composite {
    pub(super) fn start(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let period = Duration::from_secs_f64(1.0 / f64::from(self.params.config.fps));
        let worker = tokio::spawn(async move {
            let mut timer = tokio::time::interval(period);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                let Some(composite) = weak.upgrade() else {
                    break;
                };
                if let Err(error) = composite.emit_frame() {
                    error!(%error, "Multiple-window composition failed");
                    let error = ns::Error::with_domain(ns::ErrorDomain::os_status(), -1, None);
                    let _ = composite.params.error_tx.send(error);
                    break;
                }
            }
        });
        if let Ok(mut slot) = self.worker.lock()
            && let Some(previous) = slot.replace(worker)
        {
            previous.abort();
        }
    }

    pub(super) fn stop(&self) {
        if let Ok(mut slot) = self.worker.lock()
            && let Some(worker) = slot.take()
        {
            worker.abort();
        }
    }

    fn emit_frame(&self) -> anyhow::Result<()> {
        let frames = self
            .frames
            .lock()
            .map_err(|_| anyhow!("Window frame lock poisoned"))?
            .clone();
        if frames.iter().any(Option::is_none) {
            return Ok(());
        }
        let mut output = self.pool.pixel_buf()?;
        {
            let locked = PixelLock::new(&mut output, cv::pixel_buffer::LockFlags::DEFAULT)?;
            for plane in 0..2 {
                let ptr = locked.0.plane_base_address(plane).cast_mut();
                anyhow::ensure!(!ptr.is_null(), "Missing output pixel plane");
                let length = locked.0.plane_bytes_per_row(plane) * locked.0.plane_height(plane);
                unsafe {
                    std::ptr::write_bytes(ptr, if plane == 0 { 16 } else { 128 }, length);
                }
            }
            for (index, frame) in frames.iter().enumerate() {
                let image = frame
                    .as_ref()
                    .and_then(|frame| frame.image_buf())
                    .ok_or_else(|| anyhow!("Missing window image"))?;
                let (x, y, _, _) = tile(index, frames.len(), locked.0.width(), locked.0.height());
                blit(locked.0, image, x, y)?;
            }
        }
        if let Some(image) = frames
            .first()
            .and_then(|frame| frame.as_ref())
            .and_then(|frame| frame.image_buf())
        {
            copy_color_attachments(image, &mut output);
        }
        let pts = cm::Clock::host_time_clock().time();
        let timing = cm::SampleTimingInfo {
            pts,
            duration: cm::Time::new(1, self.params.config.fps as i32),
            dts: cm::Time::invalid(),
        };
        let desc = cm::VideoFormatDesc::with_image_buf(&output)?;
        let sample_buf =
            cm::SampleBuf::with_image_buf(&output, true, None, ptr::null(), &desc, &timing)?;
        let timestamp = Timestamp::MachAbsoluteTime(cap_timestamp::MachAbsoluteTimestamp::new(
            cm::Clock::convert_host_time_to_sys_units(pts),
        ));
        match self.params.video_tx.try_send(VideoFrame {
            sample_buf,
            timestamp,
        }) {
            Ok(()) => {
                self.params
                    .video_frame_counter
                    .fetch_add(1, atomic::Ordering::Relaxed);
            }
            Err(flume::TrySendError::Full(_)) => {
                self.params
                    .drop_counter
                    .fetch_add(1, atomic::Ordering::Relaxed);
            }
            Err(flume::TrySendError::Disconnected(_)) => self.stop(),
        }
        Ok(())
    }
}

pub(super) fn build(
    params: Arc<CapturerRebuildParams>,
    content: &sc::ShareableContent,
) -> anyhow::Result<Capturer> {
    let count = params.config.windows.len();
    anyhow::ensure!(
        (2..=4).contains(&count),
        "Select between two and four windows"
    );
    let composite = Arc::new(Composite {
        frames: Mutex::new(vec![None; count]),
        pool: create_pixel_buffer_pool(
            params.video_info.width as usize,
            params.video_info.height as usize,
        )
        .ok_or_else(|| anyhow!("Could not create composite frame pool"))?,
        params: params.clone(),
        worker: Mutex::new(None),
    });
    let mut capturers = Vec::new();
    for (index, id) in params.config.windows.iter().enumerate() {
        anyhow::ensure!(
            !params.excluded_windows.contains(id),
            "A selected window is excluded from recording"
        );
        let filter = window_content_filter(id, content, "multiple windows")
            .ok_or_else(|| anyhow!("Selected window {id} is no longer available"))?;
        let (_, _, width, height) = tile(
            index,
            count,
            params.video_info.width as usize,
            params.video_info.height as usize,
        );
        let mut settings = scap_screencapturekit::StreamCfgBuilder::default()
            .with_width(width)
            .with_height(height)
            .with_fps(params.config.fps as f32)
            .with_shows_cursor(true)
            .with_captures_audio(false)
            .with_queue_depth(3)
            .build();
        settings.set_pixel_format(cv::PixelFormat::_420V);
        settings.set_color_space_name(cg::color_space::names::srgb());
        configure_window_capture(&mut settings);
        let scaler = FrameScaler::new(width, height)
            .ok_or_else(|| anyhow!("Could not create window scaler"))?;
        let composite_for_frame = composite.clone();
        let error_tx = params.error_tx.clone();
        let capturer = scap_screencapturekit::Capturer::builder(filter, settings)
            .with_output_sample_buf_cb(move |frame| match frame {
                scap_screencapturekit::Frame::Screen(frame) => {
                    let sample = frame.sample_buf();
                    let Some(image) = sample.image_buf() else {
                        return;
                    };
                    if image.width() == 0 || image.height() == 0 {
                        return;
                    }
                    let scaled = if image.width() == width && image.height() == height {
                        Some(sample.retained())
                    } else {
                        scaler.scale_frame(sample)
                    };
                    if let Some(scaled) = scaled
                        && let Ok(mut frames) = composite_for_frame.frames.lock()
                    {
                        frames[index] = Some(scaled);
                    }
                }
                _ => {}
            })
            .with_stop_with_err_cb(move |_, error| {
                let _ = error_tx.send(error.retained());
            })
            .build()?;
        capturers.push(Arc::new(capturer));
    }
    if params.system_audio {
        capturers.push(Arc::new(build_system_audio(params, content)?));
    }
    Ok(Capturer {
        started: Arc::new(AtomicBool::new(false)),
        capturers,
        composite: Some(composite),
    })
}

fn copy_color_attachments(source: &cv::PixelBuf, destination: &mut cv::PixelBuf) {
    use cv::image_buf_attachment::keys;
    destination.remove_all_attaches();
    if let Some(attachments) = source.copy_attaches(cv::buffer::AttachMode::ShouldPropagate) {
        for key in [
            keys::color_space(),
            keys::gamma_level(),
            keys::iic_profile(),
            keys::ycbcr_matrix(),
            keys::color_primaries(),
            keys::transfer_fn(),
        ] {
            if let Some(value) = attachments.value(key.as_ref()) {
                destination.set_attach(key, value, cv::buffer::AttachMode::ShouldPropagate);
            }
        }
    }
}

fn build_system_audio(
    params: Arc<CapturerRebuildParams>,
    content: &sc::ShareableContent,
) -> anyhow::Result<scap_screencapturekit::Capturer> {
    // A window stream only captures its owning app's audio. Use one display
    // stream for system audio and discard its tiny video frames to avoid gaps
    // or doubled audio when windows belong to different or identical apps.
    let display = Display::from_id(&params.display_id)
        .ok_or_else(|| anyhow!("Audio capture display is unavailable"))?;
    let filter = display
        .raw_handle()
        .as_content_filter_excluding_windows(
            content.retained(),
            excluded_shareable_windows(&params.excluded_windows, content, "multiple-window audio"),
        )
        .ok_or_else(|| anyhow!("Could not create system audio filter"))?;
    let settings = scap_screencapturekit::StreamCfgBuilder::default()
        .with_width(2)
        .with_height(2)
        .with_fps(1.)
        .with_shows_cursor(false)
        .with_captures_audio(true)
        .with_queue_depth(3)
        .build();
    let mut audio_tx = params.audio_tx.clone();
    let error_tx = params.error_tx.clone();
    let params_for_audio = params;
    Ok(scap_screencapturekit::Capturer::builder(filter, settings)
        .with_output_sample_buf_cb(move |frame| {
            let scap_screencapturekit::Frame::Audio(frame) = frame else {
                return;
            };

            let Some(tx) = audio_tx.as_mut() else { return };
            let sample = frame.sample_buf();
            let Ok(list) = sample.audio_buf_list::<2>() else {
                return;
            };
            let Ok(data) = list.block().as_slice() else {
                return;
            };
            let bytes = list.list().buffers[0].data_bytes_size as usize;
            let mut output = ffmpeg::frame::Audio::new(
                ffmpeg::format::Sample::F32(ffmpeg::format::sample::Type::Planar),
                sample.num_samples() as usize,
                ffmpeg::ChannelLayout::STEREO,
            );
            output.set_rate(48_000);
            for plane in 0..output.planes() {
                let Some(source) = data.get(plane * bytes..(plane + 1) * bytes) else {
                    return;
                };
                let destination = output.data_mut(plane);
                if source.len() != destination.len() {
                    return;
                }
                destination.copy_from_slice(source);
            }
            let timestamp = Timestamp::MachAbsoluteTime(cap_timestamp::MachAbsoluteTimestamp::new(
                cm::Clock::convert_host_time_to_sys_units(sample.pts()),
            ));
            if tx.try_send(AudioFrame::new(output, timestamp)).is_ok() {
                params_for_audio
                    .system_audio_frame_counter
                    .fetch_add(1, atomic::Ordering::Relaxed);
            } else {
                params_for_audio
                    .system_audio_drop_counter
                    .fetch_add(1, atomic::Ordering::Relaxed);
            }
        })
        .with_stop_with_err_cb(move |_, error| {
            let _ = error_tx.send(error.retained());
        })
        .build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: usize, height: usize, y: u8, uv: u8) -> arc::R<cv::PixelBuf> {
        let mut image = create_pixel_buffer_pool(width, height)
            .unwrap()
            .pixel_buf()
            .unwrap();
        let locked = PixelLock::new(&mut image, cv::pixel_buffer::LockFlags::DEFAULT).unwrap();
        for (plane, value) in [y, uv].into_iter().enumerate() {
            unsafe {
                std::ptr::write_bytes(
                    locked.0.plane_base_address(plane).cast_mut(),
                    value,
                    locked.0.plane_bytes_per_row(plane) * locked.0.plane_height(plane),
                );
            }
        }
        drop(locked);
        image
    }

    #[test]
    fn composite_copies_color_but_not_single_window_geometry() {
        use cv::image_buf_attachment::{keys, ycbcr_matrix};
        let mut source = solid(6, 8, 70, 90);
        source.set_attach(
            keys::ycbcr_matrix(),
            ycbcr_matrix::itu_r_709_2().as_ref(),
            cv::buffer::AttachMode::ShouldPropagate,
        );
        source.set_attach(
            keys::clean_aperture(),
            cf::Dictionary::new().as_ref(),
            cv::buffer::AttachMode::ShouldPropagate,
        );
        let mut output = solid(12, 8, 16, 128);
        copy_color_attachments(&source, &mut output);
        assert!(output.has_attach(keys::ycbcr_matrix()));
        assert!(!output.has_attach(keys::clean_aperture()));
        assert_eq!(output.width(), 12);
    }

    #[test]
    fn nv12_composition_preserves_both_windows_and_chroma_with_padded_rows() {
        let left = solid(6, 8, 70, 90);
        let right = solid(6, 8, 180, 210);
        let mut output = solid(12, 8, 16, 128);
        let locked = PixelLock::new(&mut output, cv::pixel_buffer::LockFlags::DEFAULT).unwrap();
        blit(locked.0, &left, 0, 0).unwrap();
        blit(locked.0, &right, 6, 0).unwrap();
        for (plane, values) in [(0, [70, 180]), (1, [90, 210])] {
            for row in 0..locked.0.plane_height(plane) {
                let start = unsafe {
                    locked
                        .0
                        .plane_base_address(plane)
                        .add(row * locked.0.plane_bytes_per_row(plane))
                };
                let bytes = unsafe { std::slice::from_raw_parts(start, 12) };
                assert_eq!(&bytes[..6], &[values[0]; 6]);
                assert_eq!(&bytes[6..], &[values[1]; 6]);
            }
        }
        assert!(blit(locked.0, &right, 8, 0).is_err());
    }

    #[test]
    fn two_windows_are_side_by_side_and_four_use_nonoverlapping_even_cells() {
        assert_eq!(tile(0, 2, 1920, 1080), (0, 0, 960, 1080));
        assert_eq!(tile(1, 2, 1920, 1080), (960, 0, 960, 1080));
        assert_eq!(tile(2, 3, 1920, 1080), (0, 540, 960, 540));
        assert_eq!(tile(3, 4, 1920, 1080), (960, 540, 960, 540));
    }

    #[test]
    fn lower_grid_row_does_not_overwrite_upper_row() {
        let source = solid(6, 4, 200, 100);
        let mut output = solid(12, 8, 16, 128);
        let locked = PixelLock::new(&mut output, cv::pixel_buffer::LockFlags::DEFAULT).unwrap();
        blit(locked.0, &source, 6, 4).unwrap();
        for (plane, foreground, background) in [(0, 200, 16), (1, 100, 128)] {
            let middle = locked.0.plane_height(plane) / 2;
            for row in 0..locked.0.plane_height(plane) {
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        locked
                            .0
                            .plane_base_address(plane)
                            .add(row * locked.0.plane_bytes_per_row(plane)),
                        12,
                    )
                };
                assert_eq!(&bytes[..6], &[background; 6]);
                assert_eq!(
                    &bytes[6..],
                    &[if row < middle { background } else { foreground }; 6]
                );
            }
        }
    }
}
