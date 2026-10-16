# opencamo-desktop

Linux desktop client for **opencamo**: turns a phone camera on your LAN into a
webcam via [PROTOCOL.md](../PROTOCOL.md) (HTTP + multipart MJPEG on port
**47777**), optionally re-exposing the frames through a
[v4l2loopback](https://github.com/v4l2loopback/v4l2loopback) device so any
browser or app can use them as a webcam.

## Build

Requires a stable Rust toolchain (edition 2021) on `x86_64-unknown-linux-gnu`:

```sh
cd desktop
cargo build --release
# binary: target/release/opencamo-desktop
```

Only standard crates are used: `eframe`/`egui` (UI), `ureq` (HTTP),
`serde`/`serde_json` (capabilities JSON), `image` (JPEG decode), `v4l`
(v4l2 ioctls), `crossbeam-channel`, `parking_lot`, `anyhow`.

## v4l2loopback setup (Arch Linux)

To make the phone camera visible to browsers/`/dev/video*` consumers:

```sh
# driver (DKMS module for your running kernel)
sudo pacman -S v4l2loopback-dkms

# create one loopback device, labeled "opencamo",
# exclusive_caps=1 so apps see a clean capture-only device
sudo modprobe v4l2loopback devices=1 card_label=opencamo exclusive_caps=1
```

Make it persistent across reboots if you like:

```sh
echo "v4l2loopback devices=1 card_label=opencamo exclusive_caps=1" | \
    sudo tee /etc/modules-load.d/opencamo.conf
```

Verify:

```sh
ls /dev/video*                 # expect /dev/videoN
v4l2-ctl -d /dev/videoN --all  # card label should be "opencamo"
```

Unload with `sudo modprobe -r v4l2loopback` (fails while a writer/consumer
holds the device).

## Usage

1. Start the **opencamo phone app**; note its IP (shown in the app).
2. Run the desktop client:

   ```sh
   ./target/release/opencamo-desktop
   ```

3. Enter the phone address (`192.168.1.100` or `192.168.1.100:47777`) and
   press **Connect**. The client fetches `/v1/capabilities` and shows the
   device name, camera and `WxH@fps` resolution dropdowns.
4. Pick a camera/resolution and press **Apply** to re-request `/v1/stream`
   with the new parameters. The preview updates at up to ~30 fps.
5. **Loopback output** (right panel): pick a `/dev/video*` device
   (populated by scanning `/dev/video*` and reading each card name via
   `VIDIOC_QUERYCAP`), press **Enable**. Frames are converted RGB→YUYV and
   written into the device, e.g. for a browser's webcam picker.

The status bar shows connection state plus delivered / decode / loopback FPS.

### Permissions

`/dev/video*` nodes are usually `video`-group devices. Add your user:

```sh
sudo usermod -aG video "$USER"
# log out and back in for the group to apply
```

## Headless self-test

Works without a display (never initializes eframe) — useful for CI or
verifying a phone/stream endpoint from SSH:

```sh
./target/release/opencamo-desktop --selftest http://192.168.1.100:47777/v1/stream 60
```

Accepts either a full `/v1/stream` URL (query parameters are kept) or a bare
base URL (the default `/v1/stream` path is appended). Downloads and decodes
the given number of JPEG frames, prints throughput/latency stats to stdout,
and exits `0` on success, nonzero on failure:

```
selftest: GET http://192.168.1.100:47777/v1/stream
selftest: OK — 60 frames decoded from http://192.168.1.100:47777/v1/stream
selftest: first frame: 1280x720
selftest: bytes read: 2411724 (2355.2 KiB total, 39.3 KiB avg/frame)
selftest: elapsed: 2.01s (29.9 fps delivered)
selftest: decode time: 6.4ms avg, 11.9ms max
```

## Architecture notes

- A background thread owns the HTTP MJPEG socket read loop; parts are parsed
  by `Content-Length`, JPEGs are decoded to RGB8 there.
- The decoded frame goes over **bounded(1) channels with producer-side
  eviction** (latest-frame-wins, frames are dropped — never queued) to both
  the UI thread and the loopback writer thread.
- The UI thread polls its channel and re-uploads the texture at most every
  33 ms (~30 fps).
- The loopback writer thread negotiates `V4L2_PIX_FMT_YUYV` at the current
  frame size with `VIDIOC_S_FMT` (`V4L2_BUF_TYPE_VIDEO_OUTPUT`) and issues
  one `write()` per frame; the driver drops stale frames on its side too.
- Camera/resolution lists come from `/v1/capabilities` only — nothing is
  hardcoded on the desktop.
