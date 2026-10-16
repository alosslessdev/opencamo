# opencamo phone app (Android)

Turns the phone into a LAN webcam: a small HTTP server serves live MJPEG frames
from the camera, implementing the wire protocol defined in
[`../PROTOCOL.md`](../PROTOCOL.md) on port **47777**.

- Kotlin, min SDK 26, target SDK 34, CameraX 1.3.1
- Package: `dev.opencamo.phone`
- Endpoints: `GET /v1/capabilities`, `GET /v1/stream?camera&width&height&fps`,
  `GET /v1/ping`
- Raw `ServerSocket` HTTP server (no third-party HTTP library)
- Latest-frame-wins streaming: frames are dropped, never queued, so latency stays
  bounded; multiple clients requesting the same configuration share one stream

## Requirements

- Android Studio (Iguana or newer) **or** a JDK 17 + Android SDK 34 toolchain
- A phone or tablet with Android 8.0 (API 26) or newer
- Phone and desktop on the same Wi-Fi network

## Build

### Android Studio

1. Open Android Studio and choose **Open**, then select this `android/` directory.
2. Let Gradle sync finish (Studio downloads Gradle 8.4 from the wrapper
   properties automatically and generates the wrapper jar if missing).
3. **Build > Make Project** (or **Run > Run 'app'** to install on a device).

### Command line

The Gradle wrapper *properties* are committed but the wrapper jar is not. Either
build with Android Studio (which regenerates it), or generate the wrapper once
with a locally installed Gradle:

```sh
cd android
gradle wrapper --gradle-version 8.4   # one-time, creates gradlew + wrapper jar
./gradlew assembleDebug
```

The APK lands in `app/build/outputs/apk/debug/app-debug.apk`.

## Install

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

Or press **Run** in Android Studio with the device connected/selected.

## Permissions

- **CAMERA** – requested at runtime the first time you tap *Start server*.
  Denying it prevents streaming; grant it in system settings if you dismissed
  the dialog.
- **INTERNET** and **ACCESS_WIFI_STATE** – normal permissions declared in the
  manifest, granted at install time (the latter is used to display the device
  IP).

## Usage

1. Connect the phone to the same Wi-Fi network as the desktop.
2. Open the app. It shows the address, e.g. `http://192.168.1.23:47777`, and
   pickers for camera, resolution and frame rate (all read live from the
   device's Camera2 capabilities — nothing is hardcoded).
3. Tap **Start server** and grant the camera permission. The status area shows
   server state, connected clients, delivered FPS and the active stream
   configuration.
4. From the desktop, probe and stream:

```sh
curl http://192.168.1.23:47777/v1/ping          # -> pong
curl http://192.168.1.23:47777/v1/capabilities  # -> JSON camera list
curl http://192.168.1.23:47777/v1/stream > out.mjpeg   # multipart MJPEG
```

All stream parameters are optional: `camera` (default `back`), `width`/`height`
(default: largest supported size) and `fps` (default 30, clamped to the nearest
supported rate). Unsupported sizes answer `400`, unknown cameras `404`, both
with a JSON `{"error": "..."}` body.

The MJPEG stream uses boundary `frame`; each part carries
`Content-Type: image/jpeg` and `Content-Length`, so any MJPEG client (or a
v4l2loopback writer) can consume it directly.

## Notes

- Only one stream configuration is active at a time: requesting a different
  camera/resolution/fps closes the previous stream, per the protocol. Clients
  requesting the *same* configuration are served concurrently.
- The camera keeps running while the server is up so the first frame arrives
  immediately; tap **Stop server** to release the camera.
