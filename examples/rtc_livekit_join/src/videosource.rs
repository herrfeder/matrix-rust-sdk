#[cfg(all(feature = "v4l2", target_os = "linux"))]
use std::sync::{Arc, Mutex};

#[cfg(all(feature = "v4l2", target_os = "linux"))]
use anyhow::{anyhow, Context};
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use matrix_sdk_rtc_livekit::Room;
#[cfg(all(feature = "v4l2", target_os = "linux"))]
use tracing::{debug, info, warn};

#[cfg(all(feature = "v4l2", target_os = "linux"))]
use crate::{
    optional_env,
    utils::{spawn_stdin_text_task, TextOverlayState},
};

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Clone, Debug)]
pub(crate) struct V4l2Config {
    pub(crate) device: String,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    source: V4l2VideoSource,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug, Default)]
enum V4l2VideoSource {
    #[default]
    Camera,
    TestRedFrames,
    ZmqQueue,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
const ZMQ_SOURCE_FRAMERATE: u32 = 30;

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct V4l2PublishError(pub(crate) anyhow::Error);

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::fmt::Display for V4l2PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl std::error::Error for V4l2PublishError {}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub(crate) struct V4l2CameraPublisher {
    room: std::sync::Arc<Room>,
    track: matrix_sdk_rtc_livekit::livekit::track::LocalVideoTrack,
    stop_tx: std::sync::mpsc::Sender<()>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    stdin_overlay_task: tokio::task::JoinHandle<()>,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug)]
enum V4l2PixelFormat {
    Nv12,
    Yuyv,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug, Default)]
enum ZmqPixelFormat {
    I420,
    #[default]
    Nv12,
    Yuyv,
    Jpeg,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
#[derive(Copy, Clone, Debug, Default)]
enum ZmqPayloadEncoding {
    #[default]
    Auto,
    Raw,
    Base64,
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
impl V4l2CameraPublisher {
    pub(crate) async fn start(
        room: std::sync::Arc<Room>,
        config: V4l2Config,
    ) -> anyhow::Result<Self> {
        use matrix_sdk_rtc_livekit::livekit::options::{TrackPublishOptions, VideoCodec};
        use matrix_sdk_rtc_livekit::livekit::track::{LocalTrack, TrackSource};
        use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::RtcVideoSource;

        let (resolution, rtc_source, capture_mode) =
            configure_v4l2_capture_mode(&config).context("configure V4L2 capture")?;
        let text_overlay = Arc::new(Mutex::new(TextOverlayState::default()));
        let stdin_overlay_task = spawn_stdin_text_task(text_overlay.clone());

        let track = matrix_sdk_rtc_livekit::livekit::track::LocalVideoTrack::create_video_track(
            "v4l2_camera",
            RtcVideoSource::Native(rtc_source.clone()),
        );

        info!(
            room_name = %room.name(),
            "publishing V4L2 camera track"
        );
        room.local_participant()
            .publish_track(
                LocalTrack::Video(track.clone()),
                TrackPublishOptions {
                    source: TrackSource::Camera,
                    video_codec: VideoCodec::VP8,
                    ..Default::default()
                },
            )
            .await
            .context("publish V4L2 camera track")?;

        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let task = tokio::task::spawn_blocking(move || match capture_mode {
            V4l2CaptureMode::Camera { mut device, pixel_format } => run_v4l2_capture_loop(
                &mut device,
                resolution,
                pixel_format,
                rtc_source,
                stop_rx,
                text_overlay,
            ),
            V4l2CaptureMode::TestRedFrames => {
                run_generated_red_capture_loop(resolution, rtc_source, stop_rx)
            }
            V4l2CaptureMode::ZmqQueue { endpoint, pixel_format, payload_encoding } => {
                run_zmq_capture_loop(
                    resolution,
                    rtc_source,
                    stop_rx,
                    endpoint,
                    pixel_format,
                    payload_encoding,
                    text_overlay,
                )
            }
        });

        Ok(Self { room, track, stop_tx, task, stdin_overlay_task })
    }

    pub(crate) async fn stop(self) -> anyhow::Result<()> {
        info!(room_name = %self.room.name(), "stopping V4L2 camera track");
        let _ = self.stop_tx.send(());
        self.stdin_overlay_task.abort();
        let _ = self.task.await?;
        self.room
            .local_participant()
            .unpublish_track(&self.track.sid())
            .await
            .context("unpublish V4L2 camera track")?;
        Ok(())
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
enum V4l2CaptureMode {
    Camera {
        device: v4l::Device,
        pixel_format: V4l2PixelFormat,
    },
    TestRedFrames,
    ZmqQueue {
        endpoint: String,
        pixel_format: ZmqPixelFormat,
        payload_encoding: ZmqPayloadEncoding,
    },
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn configure_v4l2_capture_mode(
    config: &V4l2Config,
) -> anyhow::Result<(
    matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    V4l2CaptureMode,
)> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution;
    use matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource;

    if matches!(config.source, V4l2VideoSource::TestRedFrames) {
        let resolution = VideoResolution {
            width: config.width.unwrap_or(640),
            height: config.height.unwrap_or(480),
        };
        let rtc_source = NativeVideoSource::new(resolution.clone(), true);
        info!(
            width = resolution.width,
            height = resolution.height,
            "configured generated red test video source"
        );
        return Ok((resolution, rtc_source, V4l2CaptureMode::TestRedFrames));
    }

    if matches!(config.source, V4l2VideoSource::ZmqQueue) {
        let width = config.width.unwrap_or(640);
        let height = config.height.unwrap_or(480);
        let resolution = VideoResolution { width, height };
        let rtc_source = NativeVideoSource::new(resolution.clone(), true);
        let pixel_format = zmq_pixel_format_from_env()?;
        let payload_encoding = zmq_payload_encoding_from_env()?;
        info!(
            width = resolution.width,
            height = resolution.height,
            endpoint = %config.device,
            framerate = ZMQ_SOURCE_FRAMERATE,
            pixel_format = ?pixel_format,
            payload_encoding = ?payload_encoding,
            "configured ZMQ queue video source"
        );
        return Ok((
            resolution,
            rtc_source,
            V4l2CaptureMode::ZmqQueue {
                endpoint: config.device.clone(),
                pixel_format,
                payload_encoding,
            },
        ));
    }

    use v4l::video::Capture;
    use v4l::Device;

    let mut device = Device::with_path(&config.device).context("open V4L2 device")?;
    let mut format = device.format().context("read V4L2 format")?;

    if let Some(width) = config.width {
        format.width = width;
    }
    if let Some(height) = config.height {
        format.height = height;
    }
    let format = set_format_with_fallback(&mut device, format)?;
    let pixel_format = match &format.fourcc.repr {
        b"NV12" => V4l2PixelFormat::Nv12,
        b"YUYV" => V4l2PixelFormat::Yuyv,
        _ => {
            return Err(anyhow!(
                "V4L2 device did not accept NV12 or YUYV; got {:?} instead",
                format.fourcc
            ));
        }
    };

    let resolution = VideoResolution { width: format.width, height: format.height };
    info!(
        device = %config.device,
        width = format.width,
        height = format.height,
        fourcc = ?format.fourcc,
        "configured V4L2 device format"
    );
    let rtc_source = NativeVideoSource::new(resolution.clone(), true);
    Ok((resolution, rtc_source, V4l2CaptureMode::Camera { device, pixel_format }))
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn run_v4l2_capture_loop(
    device: &mut v4l::Device,
    resolution: matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    pixel_format: V4l2PixelFormat,
    rtc_source: matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    stop_rx: std::sync::mpsc::Receiver<()>,
    text_overlay: Arc<Mutex<TextOverlayState>>,
) -> anyhow::Result<()> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::native::yuv_helper;
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};
    use v4l::buffer::Type;
    use v4l::io::mmap::Stream;
    use v4l::io::traits::CaptureStream;
    use v4l::video::Capture;

    let format = device.format().context("re-read V4L2 format")?;
    let width = format.width as usize;
    let height = format.height as usize;
    let stride = if format.stride == 0 {
        match pixel_format {
            V4l2PixelFormat::Nv12 => width,
            V4l2PixelFormat::Yuyv => width * 2,
        }
    } else {
        format.stride as usize
    };
    let expected_size = match pixel_format {
        V4l2PixelFormat::Nv12 => stride * height + (stride * height / 2),
        V4l2PixelFormat::Yuyv => stride * height,
    };

    let mut stream =
        Stream::with_buffers(device, Type::VideoCapture, 4).context("start V4L2 stream")?;
    let start = std::time::Instant::now();

    let mut frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        buffer: I420Buffer::new(resolution.width, resolution.height),
        timestamp_us: 0,
    };

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let (data, _meta) = stream.next().context("read V4L2 frame")?;
        if data.len() < expected_size {
            warn!(
                data_len = data.len(),
                expected = expected_size,
                "V4L2 frame shorter than expected"
            );
            continue;
        }

        let (stride_y, stride_u, stride_v) = frame.buffer.strides();
        let (dst_y, dst_u, dst_v) = frame.buffer.data_mut();

        match pixel_format {
            V4l2PixelFormat::Nv12 => {
                let y_plane_len = stride * height;
                let (src_y, src_uv) = data.split_at(y_plane_len);

                yuv_helper::nv12_to_i420(
                    src_y,
                    stride as u32,
                    src_uv,
                    stride as u32,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                    resolution.width as i32,
                    resolution.height as i32,
                );
            }
            V4l2PixelFormat::Yuyv => {
                yuyv_to_i420(
                    data, width, stride, height, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
                );
            }
        }

        if let Ok(mut overlay) = text_overlay.lock() {
            overlay.tick_and_draw(&resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v);
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn run_generated_red_capture_loop(
    resolution: matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    rtc_source: matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    stop_rx: std::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};

    let mut frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        buffer: I420Buffer::new(resolution.width, resolution.height),
        timestamp_us: 0,
    };
    let (stride_y, stride_u, stride_v) = frame.buffer.strides();
    let (dst_y, dst_u, dst_v) = frame.buffer.data_mut();

    fill_plane(dst_y, stride_y as usize, resolution.width as usize, resolution.height as usize, 76);
    fill_plane(
        dst_u,
        stride_u as usize,
        (resolution.width / 2) as usize,
        (resolution.height / 2) as usize,
        84,
    );
    fill_plane(
        dst_v,
        stride_v as usize,
        (resolution.width / 2) as usize,
        (resolution.height / 2) as usize,
        255,
    );

    let frame_duration = std::time::Duration::from_millis(33);
    let start = std::time::Instant::now();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
        std::thread::sleep(frame_duration);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn run_zmq_capture_loop(
    resolution: matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    rtc_source: matrix_sdk_rtc_livekit::livekit::webrtc::video_source::native::NativeVideoSource,
    stop_rx: std::sync::mpsc::Receiver<()>,
    endpoint: String,
    pixel_format: ZmqPixelFormat,
    payload_encoding: ZmqPayloadEncoding,
    text_overlay: Arc<Mutex<TextOverlayState>>,
) -> anyhow::Result<()> {
    use matrix_sdk_rtc_livekit::livekit::webrtc::prelude::{I420Buffer, VideoFrame, VideoRotation};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create local runtime for ZMQ source")?;

    let mut socket = runtime.block_on(async {
        use zeromq::{Socket, SubSocket};

        let mut socket = SubSocket::new();
        socket
            .connect(&endpoint)
            .await
            .with_context(|| format!("connect to ZMQ endpoint {endpoint}"))?;
        socket.subscribe("").await.context("subscribe to ZMQ queue")?;
        anyhow::Ok(socket)
    })?;

    let width = resolution.width as usize;
    let height = resolution.height as usize;
    let expected_i420_size = width * height * 3 / 2;
    let expected_size = match pixel_format {
        ZmqPixelFormat::I420 | ZmqPixelFormat::Nv12 => Some(expected_i420_size),
        ZmqPixelFormat::Yuyv => Some(width * height * 2),
        ZmqPixelFormat::Jpeg => None,
    };
    debug!(
        endpoint = %endpoint,
        ?pixel_format,
        ?payload_encoding,
        ?expected_size,
        width,
        height,
        "starting ZMQ capture loop"
    );

    let mut frame = VideoFrame {
        rotation: VideoRotation::VideoRotation0,
        buffer: I420Buffer::new(resolution.width, resolution.height),
        timestamp_us: 0,
    };

    let frame_duration = std::time::Duration::from_millis(1000 / ZMQ_SOURCE_FRAMERATE as u64);
    let start = std::time::Instant::now();

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let payload_parts = match runtime.block_on(async {
            use tokio::time::{timeout, Duration};
            use zeromq::SocketRecv;

            match timeout(Duration::from_millis(100), socket.recv()).await {
                Ok(Ok(message)) => Ok(Some(message.into_vec())),
                Ok(Err(err)) => Err(anyhow!(err).context("receive frame from ZMQ queue")),
                Err(_) => Ok(None),
            }
        })? {
            Some(parts) => parts,
            None => continue,
        };

        debug!(
            num_parts = payload_parts.len(),
            part_sizes = ?payload_parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            "received ZMQ message"
        );

        let selected_idx = payload_parts.iter().position(|part| match expected_size {
            Some(size) => part.len() == size,
            None => is_probably_jpeg(part),
        });
        let payload =
            selected_idx.and_then(|idx| payload_parts.get(idx)).or_else(|| payload_parts.last());

        let Some(payload) = payload else {
            debug!("dropping empty ZMQ multipart message");
            continue;
        };

        debug!(
            selected_part = ?selected_idx,
            selected_payload_len = payload.len(),
            "selected ZMQ payload part"
        );

        if let Some(expected_size) = expected_size {
            if payload.len() != expected_size {
                warn!(
                    payload_len = payload.len(),
                    expected_size,
                    num_parts = payload_parts.len(),
                    "dropping ZMQ frame with unexpected size for configured pixel format"
                );
                continue;
            }
        }

        let (stride_y, stride_u, stride_v) = frame.buffer.strides();
        let (dst_y, dst_u, dst_v) = frame.buffer.data_mut();

        match pixel_format {
            ZmqPixelFormat::I420 => {
                let expected_y_len = width * height;
                let expected_u_len = expected_y_len / 4;
                copy_plane(&payload[..expected_y_len], width, height, dst_y, stride_y as usize);
                copy_plane(
                    &payload[expected_y_len..expected_y_len + expected_u_len],
                    width / 2,
                    height / 2,
                    dst_u,
                    stride_u as usize,
                );
                copy_plane(
                    &payload[expected_y_len + expected_u_len..],
                    width / 2,
                    height / 2,
                    dst_v,
                    stride_v as usize,
                );
            }
            ZmqPixelFormat::Nv12 => {
                use matrix_sdk_rtc_livekit::livekit::webrtc::native::yuv_helper;

                let y_plane_len = width * height;
                let (src_y, src_uv) = payload.split_at(y_plane_len);
                yuv_helper::nv12_to_i420(
                    src_y,
                    width as u32,
                    src_uv,
                    width as u32,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                    resolution.width as i32,
                    resolution.height as i32,
                );
            }
            ZmqPixelFormat::Yuyv => {
                yuyv_to_i420(
                    payload,
                    width,
                    width * 2,
                    height,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                );
            }
            ZmqPixelFormat::Jpeg => {
                decode_zmq_jpeg_payload_to_i420(
                    payload,
                    payload_encoding,
                    &resolution,
                    dst_y,
                    stride_y,
                    dst_u,
                    stride_u,
                    dst_v,
                    stride_v,
                )?;
            }
        }

        if let Ok(mut overlay) = text_overlay.lock() {
            overlay.tick_and_draw(&resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v);
        }

        frame.timestamp_us = start.elapsed().as_micros() as i64;
        rtc_source.capture_frame(&frame);
        std::thread::sleep(frame_duration);
    }

    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn copy_plane(src: &[u8], width: usize, height: usize, dst: &mut [u8], dst_stride: usize) {
    for row in 0..height {
        let src_start = row * width;
        let dst_start = row * dst_stride;
        dst[dst_start..dst_start + width].copy_from_slice(&src[src_start..src_start + width]);
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn is_probably_jpeg(data: &[u8]) -> bool {
    data.len() >= 4 && data.starts_with(&[0xFF, 0xD8]) && data.ends_with(&[0xFF, 0xD9])
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn decode_zmq_jpeg_payload_to_i420(
    payload: &[u8],
    payload_encoding: ZmqPayloadEncoding,
    resolution: &matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    dst_y: &mut [u8],
    stride_y: u32,
    dst_u: &mut [u8],
    stride_u: u32,
    dst_v: &mut [u8],
    stride_v: u32,
) -> anyhow::Result<()> {
    match payload_encoding {
        ZmqPayloadEncoding::Raw => decode_jpeg_to_i420(
            payload, resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
        ),
        ZmqPayloadEncoding::Base64 => {
            let decoded = decode_base64_payload(payload)?;
            decode_jpeg_to_i420(
                &decoded, resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
            )
        }
        ZmqPayloadEncoding::Auto => {
            if is_probably_jpeg(payload)
                && decode_jpeg_to_i420(
                    payload, resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
                )
                .is_ok()
            {
                return Ok(());
            }
            let decoded = decode_base64_payload(payload)
                .context("decode base64 JPEG payload from ZMQ queue (auto mode)")?;
            decode_jpeg_to_i420(
                &decoded, resolution, dst_y, stride_y, dst_u, stride_u, dst_v, stride_v,
            )
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn decode_base64_payload(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let payload_str = std::str::from_utf8(payload).context("payload is not UTF-8 base64 text")?;
    let trimmed = payload_str.trim();
    let decoded = STANDARD.decode(trimmed).context("base64 decode payload from ZMQ queue")?;
    debug!(
        encoded_len = payload.len(),
        decoded_len = decoded.len(),
        "decoded base64 payload from ZMQ queue"
    );
    Ok(decoded)
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn decode_jpeg_to_i420(
    jpeg_payload: &[u8],
    resolution: &matrix_sdk_rtc_livekit::livekit::webrtc::prelude::VideoResolution,
    dst_y: &mut [u8],
    stride_y: u32,
    dst_u: &mut [u8],
    stride_u: u32,
    dst_v: &mut [u8],
    stride_v: u32,
) -> anyhow::Result<()> {
    use jpeg_decoder::{Decoder as JpegDecoder, PixelFormat};

    let mut decoder = JpegDecoder::new(std::io::Cursor::new(jpeg_payload));
    let decoded = decoder.decode().context("decode JPEG payload from ZMQ queue")?;
    let info = decoder.info().context("read JPEG info from ZMQ queue")?;

    let src_width = info.width as usize;
    let src_height = info.height as usize;
    let dst_width = resolution.width as usize;
    let dst_height = resolution.height as usize;

    if src_width != dst_width || src_height != dst_height {
        return Err(anyhow!(
            "JPEG frame size {}x{} does not match configured output {}x{}",
            src_width,
            src_height,
            dst_width,
            dst_height
        ));
    }

    let rgb = match info.pixel_format {
        PixelFormat::RGB24 => decoded,
        PixelFormat::L8 => {
            let mut rgb = Vec::with_capacity(src_width * src_height * 3);
            for y in decoded {
                rgb.push(y);
                rgb.push(y);
                rgb.push(y);
            }
            rgb
        }
        other => {
            return Err(anyhow!("unsupported JPEG pixel format {:?}; expected RGB24 or L8", other));
        }
    };

    rgb_to_i420(
        &rgb,
        dst_width,
        dst_height,
        dst_y,
        stride_y as usize,
        dst_u,
        stride_u as usize,
        dst_v,
        stride_v as usize,
    );
    Ok(())
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn rgb_to_i420(
    src_rgb: &[u8],
    width: usize,
    height: usize,
    dst_y: &mut [u8],
    dst_stride_y: usize,
    dst_u: &mut [u8],
    dst_stride_u: usize,
    dst_v: &mut [u8],
    dst_stride_v: usize,
) {
    for y in 0..height {
        let src_row = &src_rgb[y * width * 3..(y + 1) * width * 3];
        let dst_y_row = &mut dst_y[y * dst_stride_y..(y + 1) * dst_stride_y];
        for x in 0..width {
            let r = src_row[x * 3] as f32;
            let g = src_row[x * 3 + 1] as f32;
            let b = src_row[x * 3 + 2] as f32;
            let y_value = (0.257 * r + 0.504 * g + 0.098 * b + 16.0).round().clamp(0.0, 255.0);
            dst_y_row[x] = y_value as u8;
        }
    }

    for y in 0..(height / 2) {
        let dst_u_row = &mut dst_u[y * dst_stride_u..(y + 1) * dst_stride_u];
        let dst_v_row = &mut dst_v[y * dst_stride_v..(y + 1) * dst_stride_v];

        for x in 0..(width / 2) {
            let mut r_sum = 0.0f32;
            let mut g_sum = 0.0f32;
            let mut b_sum = 0.0f32;

            for dy in 0..2 {
                for dx in 0..2 {
                    let sx = x * 2 + dx;
                    let sy = y * 2 + dy;
                    let base = (sy * width + sx) * 3;
                    r_sum += src_rgb[base] as f32;
                    g_sum += src_rgb[base + 1] as f32;
                    b_sum += src_rgb[base + 2] as f32;
                }
            }

            let r = r_sum / 4.0;
            let g = g_sum / 4.0;
            let b = b_sum / 4.0;

            let u_value = (-0.148 * r - 0.291 * g + 0.439 * b + 128.0).round().clamp(0.0, 255.0);
            let v_value = (0.439 * r - 0.368 * g - 0.071 * b + 128.0).round().clamp(0.0, 255.0);

            dst_u_row[x] = u_value as u8;
            dst_v_row[x] = v_value as u8;
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn fill_plane(dst: &mut [u8], stride: usize, width: usize, height: usize, value: u8) {
    for y in 0..height {
        let row = &mut dst[y * stride..y * stride + width];
        row.fill(value);
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn set_format_with_fallback(
    device: &mut v4l::Device,
    mut format: v4l::format::Format,
) -> anyhow::Result<v4l::format::Format> {
    use v4l::video::Capture;
    use v4l::FourCC;

    let nv12 = FourCC::new(b"NV12");
    let yuyv = FourCC::new(b"YUYV");

    format.fourcc = nv12;
    let format = device.set_format(&format).context("set V4L2 format")?;
    if format.fourcc == nv12 {
        return Ok(format);
    }

    let mut format = format;
    format.fourcc = yuyv;
    let format = device.set_format(&format).context("set V4L2 format (YUYV)")?;
    Ok(format)
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn yuyv_to_i420(
    src: &[u8],
    width: usize,
    src_stride: usize,
    height: usize,
    dst_y: &mut [u8],
    dst_stride_y: u32,
    dst_u: &mut [u8],
    dst_stride_u: u32,
    dst_v: &mut [u8],
    dst_stride_v: u32,
) {
    let dst_stride_y = dst_stride_y as usize;
    let dst_stride_u = dst_stride_u as usize;
    let dst_stride_v = dst_stride_v as usize;

    for y in 0..height {
        let src_row = &src[y * src_stride..(y + 1) * src_stride];
        let dst_y_row = &mut dst_y[y * dst_stride_y..(y + 1) * dst_stride_y];
        for x in 0..width {
            let pair = x & !1;
            let base = pair * 2;
            let y_offset = if x % 2 == 0 { 0 } else { 2 };
            dst_y_row[x] = src_row[base + y_offset];
        }
    }

    let chroma_height = height / 2;
    for y in 0..chroma_height {
        let src_row0 = &src[(y * 2) * src_stride..(y * 2 + 1) * src_stride];
        let src_row1 = if y * 2 + 1 < height {
            &src[(y * 2 + 1) * src_stride..(y * 2 + 2) * src_stride]
        } else {
            src_row0
        };
        let dst_u_row = &mut dst_u[y * dst_stride_u..(y + 1) * dst_stride_u];
        let dst_v_row = &mut dst_v[y * dst_stride_v..(y + 1) * dst_stride_v];

        for x in 0..(width / 2) {
            let base = x * 4;
            let u0 = src_row0[base + 1] as u16;
            let v0 = src_row0[base + 3] as u16;
            let u1 = src_row1[base + 1] as u16;
            let v1 = src_row1[base + 3] as u16;
            dst_u_row[x] = ((u0 + u1) / 2) as u8;
            dst_v_row[x] = ((v0 + v1) / 2) as u8;
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn zmq_pixel_format_from_env() -> anyhow::Result<ZmqPixelFormat> {
    match optional_env("V4L2_ZMQ_PIXEL_FORMAT").as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("nv12") => Ok(ZmqPixelFormat::Nv12),
        Some("i420") => Ok(ZmqPixelFormat::I420),
        Some("yuyv") => Ok(ZmqPixelFormat::Yuyv),
        Some("jpeg") | Some("jpg") => Ok(ZmqPixelFormat::Jpeg),
        Some(other) => Err(anyhow!(
            "invalid V4L2_ZMQ_PIXEL_FORMAT '{other}'; expected nv12|i420|yuyv|jpeg|jpg"
        )),
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
fn zmq_payload_encoding_from_env() -> anyhow::Result<ZmqPayloadEncoding> {
    match optional_env("V4L2_ZMQ_PAYLOAD_ENCODING")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("auto") => Ok(ZmqPayloadEncoding::Auto),
        Some("raw") => Ok(ZmqPayloadEncoding::Raw),
        Some("base64") => Ok(ZmqPayloadEncoding::Base64),
        Some(other) => {
            Err(anyhow!("invalid V4L2_ZMQ_PAYLOAD_ENCODING '{other}'; expected auto|raw|base64"))
        }
    }
}

#[cfg(all(feature = "v4l2", target_os = "linux"))]
pub(crate) fn v4l2_config_from_env() -> anyhow::Result<Option<V4l2Config>> {
    let source = match optional_env("V4L2_VIDEO_SOURCE")
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("camera") | Some("webcam") | None => V4l2VideoSource::Camera,
        Some("test_red") | Some("test-red") | Some("red") => V4l2VideoSource::TestRedFrames,
        Some("zmq") | Some("queue") | Some("zmq_queue") | Some("zmq-queue") => {
            V4l2VideoSource::ZmqQueue
        }
        Some(other) => {
            return Err(anyhow!(
                "invalid V4L2_VIDEO_SOURCE '{other}'; expected camera|webcam|test_red|test-red|red|zmq|queue|zmq_queue|zmq-queue"
            ));
        }
    };

    let device = if matches!(source, V4l2VideoSource::Camera) {
        match optional_env("V4L2_DEVICE") {
            Some(device) => device,
            None => return Ok(None),
        }
    } else if matches!(source, V4l2VideoSource::ZmqQueue) {
        let ip = optional_env("V4L2_ZMQ_IP").unwrap_or_else(|| "127.0.0.1".to_owned());
        let port = optional_env("V4L2_ZMQ_PORT").unwrap_or_else(|| "5555".to_owned());
        format!("tcp://{ip}:{port}")
    } else {
        optional_env("V4L2_DEVICE").unwrap_or_else(|| "generated-test-source".to_owned())
    };

    let width = optional_env("V4L2_WIDTH")
        .as_deref()
        .map(str::parse::<u32>)
        .transpose()
        .context("parse V4L2_WIDTH")?;
    let height = optional_env("V4L2_HEIGHT")
        .as_deref()
        .map(str::parse::<u32>)
        .transpose()
        .context("parse V4L2_HEIGHT")?;

    Ok(Some(V4l2Config { device, width, height, source }))
}

#[cfg(not(all(feature = "v4l2", target_os = "linux")))]
pub(crate) fn v4l2_config_from_env() -> anyhow::Result<()> {
    Ok(())
}
