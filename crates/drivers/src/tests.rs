use super::*;
use image::GenericImageView;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn parses_fragmentable_identity_and_touch_prefix() {
    let parsed = parse_reply(b"TouchD,0,12,34TouchU,0,15,16MODEL-0960*360S rest").unwrap();
    let (size, identity) = parsed.unwrap();
    assert_eq!(
        size,
        ScreenSize {
            width: 960,
            height: 360
        }
    );
    assert_eq!(identity, "MODEL-0960*360S");
}

#[test]
fn maps_documented_dls_model_orientations() {
    assert_eq!(rotation_for_identity("VMAXA170320*1480S"), 180);
    assert_eq!(rotation_for_identity("VMAXC17-0960*360S"), 0);
    assert_eq!(rotation_for_identity("VMAXB-0960*360S"), 270);
    assert_eq!(rotation_for_identity("VMAXD-0960*360S"), 90);
    assert_eq!(rotation_for_identity("MODEL-0960*360S"), 0);
}

#[test]
fn parser_waits_for_delimiter_and_rejects_bad_dimensions() {
    assert!(parse_reply(b"MODEL-0960*360").unwrap().is_none());
    assert!(parse_reply(b"MODEL-0960*99999S").is_err());
    assert!(parse_reply(b"MODEL-096x*360S").is_err());
}

#[test]
fn unicode_bare_endpoint_is_a_socket_without_panicking() {
    assert_eq!(endpoint_kind("屏.sock").unwrap(), Endpoint::Unix("屏.sock"));
}

#[tokio::test]
async fn handshake_writes_magic_and_accepts_fragmented_reply() {
    let (mut peer, device) = tokio::io::duplex(4096);
    let server = tokio::spawn(async move {
        let mut magic = [0; 4];
        peer.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic, MAGIC);
        peer.write_all(b"TouchD,0,12,34MODEL-0").await.unwrap();
        peer.write_all(b"960*3").await.unwrap();
        peer.write_all(b"60S").await.unwrap();
    });
    let display = negotiate(Box::new(device), 80).await.unwrap();
    server.await.unwrap();
    assert_eq!(
        display.size(),
        ScreenSize {
            width: 960,
            height: 360
        }
    );
    assert_eq!(display.identity(), "MODEL-0960*360S");
}

#[tokio::test]
async fn handshake_eof_is_actionable() {
    let (mut peer, device) = tokio::io::duplex(64);
    let server = tokio::spawn(async move {
        let mut magic = [0; 4];
        peer.read_exact(&mut magic).await.unwrap();
        assert_eq!(magic, MAGIC);
    });
    let error = negotiate(Box::new(device), 80)
        .await
        .err()
        .expect("handshake must fail on EOF");
    server.await.unwrap();
    assert!(format!("{error:#}").contains("complete DLS handshake"));
}

#[test]
fn jpeg_preserves_dimensions_and_basic_color() {
    let size = ScreenSize::new(2, 2).unwrap();
    let frame = Frame::new(size, [255, 0, 0, 255].repeat(4), 7).unwrap();
    let jpeg = encode_jpeg(frame, 100, 0, Vec::new()).unwrap();
    let image = image::load_from_memory(&jpeg).unwrap();
    assert_eq!(image.dimensions(), (2, 2));
    let pixel = image.to_rgb8().get_pixel(0, 0).0;
    assert!(
        pixel[0] > 180 && pixel[1] < 80 && pixel[2] < 80,
        "decoded pixel was {pixel:?}"
    );
}

#[test]
fn jpeg_rotation_preserves_logical_frame_and_moves_pixels_to_native_positions() {
    let logical = ScreenSize::new(32, 48).unwrap();
    let native = ScreenSize::new(48, 32).unwrap();
    let mut pixels = vec![0; logical.width as usize * logical.height as usize * 4];
    for y in 0..logical.height as usize {
        for x in 0..logical.width as usize {
            let color = match (x < 16, y < 24) {
                (true, true) => [255, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 255, 255],
                (false, false) => [255, 255, 0, 255],
            };
            pixels[(y * logical.width as usize + x) * 4..][..4].copy_from_slice(&color);
        }
    }
    let frame = Frame::new(logical, pixels.clone(), 1).unwrap();
    for (rotation, dimensions, sample, channel) in [
        (0, (32, 48), (8, 8), 0),
        (90, (48, 32), (40, 8), 0),
        (180, (32, 48), (24, 40), 0),
        (270, (48, 32), (8, 24), 0),
    ] {
        let jpeg = encode_jpeg(frame.clone(), 100, rotation, Vec::new()).unwrap();
        let decoded = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), dimensions);
        let pixel = decoded.get_pixel(sample.0, sample.1).0;
        assert!(
            pixel[channel] > 200 && pixel[1] < 60 && pixel[2] < 60,
            "rotation {rotation}: {pixel:?}"
        );
    }
    assert_eq!(frame.size, logical);
    assert_eq!(frame.rgba.as_slice(), pixels.as_slice());

    let (peer, device) = tokio::io::duplex(64);
    drop(peer);
    let mut display = Display {
        stream: Box::new(device),
        size: native,
        identity: "VMAXB-0048*032S".into(),
        quality: 100,
        rotation: 90,
    };
    assert_eq!(display.logical_size(), logical);
    display.set_rotation(Some(0)).unwrap();
    assert_eq!(display.logical_size(), native);
    assert!(display.set_rotation(Some(45)).is_err());
}

#[tokio::test]
async fn frame_size_mismatch_is_rejected_before_write() {
    let (mut peer, device) = tokio::io::duplex(64);
    let (_frames_tx, frames) = watch::channel(Some(
        Frame::new(ScreenSize::new(1, 1).unwrap(), vec![0, 0, 0, 255], 1).unwrap(),
    ));
    let (_shutdown_tx, shutdown) = watch::channel(false);
    let error = send_frames(
        device,
        ScreenSize::new(2, 2).unwrap(),
        0,
        80,
        frames,
        shutdown,
    )
    .await
    .expect_err("size mismatch must fail");
    assert!(format!("{error:#}").contains("do not match DLS screen"));
    let mut bytes = Vec::new();
    peer.read_to_end(&mut bytes).await.unwrap();
    assert!(bytes.is_empty());
}

fn solid_frame(size: ScreenSize, color: [u8; 4], sequence: u64) -> Frame {
    Frame::new(
        size,
        color.repeat(size.width as usize * size.height as usize),
        sequence,
    )
    .unwrap()
}

async fn read_jpeg(peer: &mut tokio::io::DuplexStream, mut bytes: Vec<u8>) -> Vec<u8> {
    while !bytes.ends_with(&[0xff, 0xd9]) {
        bytes.push(peer.read_u8().await.unwrap());
    }
    assert!(bytes.starts_with(&[0xff, 0xd8]));
    bytes
}

#[tokio::test]
async fn duplex_sends_complete_jpegs_and_coalesces_pending_frames() {
    let size = ScreenSize::new(16, 8).unwrap();
    let (mut peer, device) = tokio::io::duplex(8);
    let (frames_tx, frames) = watch::channel(Some(solid_frame(size, [255, 0, 0, 255], 1)));
    let (shutdown_tx, shutdown) = watch::channel(false);
    let display = Display {
        stream: Box::new(device),
        size,
        identity: "test".into(),
        quality: 100,
        rotation: 0,
    };
    let task = tokio::spawn(display.run(frames, shutdown));
    let mut prefix = vec![0; 2];
    peer.read_exact(&mut prefix).await.unwrap();
    // Backpressure holds the first write while two newer frames arrive.
    frames_tx
        .send(Some(solid_frame(size, [0, 255, 0, 255], 2)))
        .unwrap();
    frames_tx
        .send(Some(solid_frame(size, [0, 0, 255, 255], 3)))
        .unwrap();
    peer.write_all(b"TouchD,0,1,2TouchU,0,1,2").await.unwrap();
    let red = read_jpeg(&mut peer, prefix).await;
    let blue = read_jpeg(&mut peer, Vec::new()).await;
    for (jpeg, channel) in [(red, 0), (blue, 2)] {
        let image = image::load_from_memory(&jpeg).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (16, 8));
        let pixel = image.get_pixel(8, 4).0;
        assert!(pixel[channel] > 240, "decoded pixel was {pixel:?}");
        assert!(pixel[(channel + 1) % 3] < 15);
    }
    shutdown_tx.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn handshake_timeout_is_bounded() {
    let (_peer, device) = tokio::io::duplex(64);
    let error = negotiate(Box::new(device), 80).await.err().unwrap();
    assert!(error.to_string().contains("timed out after 5 seconds"));
}

#[tokio::test]
async fn handshake_rejects_long_non_ascii_and_invalid_replies() {
    for reply in [
        vec![b'X'; HANDSHAKE_LIMIT + 1],
        vec![0xff],
        b"MODEL-0000*360S".to_vec(),
    ] {
        let (mut peer, device) = tokio::io::duplex(HANDSHAKE_LIMIT + 8);
        let server = tokio::spawn(async move {
            let mut magic = [0; 4];
            peer.read_exact(&mut magic).await.unwrap();
            peer.write_all(&reply).await.unwrap();
        });
        assert!(negotiate(Box::new(device), 80).await.is_err());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn run_reports_eof_and_stops_on_shutdown() {
    let size = ScreenSize::new(1, 1).unwrap();
    let (peer, device) = tokio::io::duplex(64);
    let (_frames_tx, frames) = watch::channel(None);
    let (_shutdown_tx, shutdown) = watch::channel(false);
    let display = Display {
        stream: Box::new(device),
        size,
        identity: "test".into(),
        quality: 80,
        rotation: 0,
    };
    drop(peer);
    assert!(
        display
            .run(frames, shutdown)
            .await
            .unwrap_err()
            .to_string()
            .contains("EOF")
    );
    let (_peer, device) = tokio::io::duplex(64);
    let (_frames_tx, frames) = watch::channel(None);
    let (_shutdown_tx, shutdown) = watch::channel(true);
    let display = Display {
        stream: Box::new(device),
        size,
        identity: "test".into(),
        quality: 80,
        rotation: 0,
    };
    display.run(frames, shutdown).await.unwrap();
}

#[tokio::test]
async fn invalid_configuration_is_rejected_before_opening() {
    assert!(
        connect("not-a-real-endpoint", Some("unknown"), 0, 9600, 80)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("unknown display driver")
    );
    for quality in [0, 101] {
        assert!(
            connect("not-a-real-endpoint", None, 0, 9600, quality)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("JPEG quality")
        );
    }
}

#[test]
fn parses_usb_selector_and_rejects_ambiguous_forms() {
    assert_eq!(
        parse_usb_selector("usb:33C3:F101").unwrap(),
        Some((0x33c3, 0xf101))
    );
    assert!(parse_usb_selector("usb:33c3:f101:extra").is_err());
    assert!(parse_usb_selector("usb:33c:f101").is_err());
}

#[test]
fn forced_driver_overrides_the_usb_driver_map() {
    assert_eq!(
        driver_for_usb(0x1234, 0x5678, Some("soeyi-dls")).unwrap(),
        "soeyi-dls"
    );
    assert!(driver_for_usb(0x1234, 0x5678, None).is_err());
}

#[test]
fn usb_index_filters_models_and_uses_port_order() {
    use tokio_serial::{SerialPortInfo, SerialPortType, UsbPortInfo};

    let usb = |name: &str, vid, pid| SerialPortInfo {
        port_name: name.into(),
        port_type: SerialPortType::UsbPort(UsbPortInfo {
            vid,
            pid,
            serial_number: None,
            manufacturer: None,
            product: None,
        }),
    };
    let ports = vec![
        usb("/dev/ttyACM2", 0x33c3, 0xf101),
        usb("/dev/ttyACM0", 0x33c3, 0x7788),
        usb("/dev/ttyACM3", 0x1234, 0xf101),
        usb("/dev/ttyACM1", 0x33c3, 0xf101),
        SerialPortInfo {
            port_name: "/dev/ttyS0".into(),
            port_type: SerialPortType::Unknown,
        },
    ];
    for (index, expected) in [(0, "/dev/ttyACM1"), (1, "/dev/ttyACM2")] {
        let selected = select_usb_port(ports.clone(), 0x33c3, 0xf101, index).unwrap();
        assert_eq!(selected.port_name, expected);
    }
    assert!(select_usb_port(ports, 0x33c3, 0xf101, 2).is_err());
}
