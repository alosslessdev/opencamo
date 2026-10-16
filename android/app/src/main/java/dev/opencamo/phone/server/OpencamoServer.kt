package dev.opencamo.phone.server

import android.util.Log
import android.util.Size
import dev.opencamo.phone.camera.CameraDescriptor
import dev.opencamo.phone.camera.CameraRepository
import dev.opencamo.phone.camera.StreamController
import org.json.JSONArray
import org.json.JSONObject
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.io.OutputStream
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicLong

/** A resolved /v1/stream configuration. Two sessions with the same [key] share one stream. */
data class StreamConfig(
    val camera: CameraDescriptor,
    val size: Size,
    val fps: Int,
) {
    val key: String get() = "${camera.id}:${size.width}x${size.height}:$fps"
}

/**
 * Minimal HTTP/1.1 server implementing the opencamo wire protocol v1 on raw [ServerSocket]s:
 *
 *  - `GET /v1/capabilities` – JSON camera/resolution/fps list, re-queried per request
 *  - `GET /v1/stream?camera&width&height&fps` – multipart MJPEG, boundary `frame`
 *  - `GET /v1/ping` – `pong`
 *
 * Only one stream configuration is active at a time: a request with a new camera/width/height/
 * fps closes the sessions of the previous configuration. Clients that request the same
 * configuration join the running stream; every session writer reads only the latest frame
 * from the [FrameHub][dev.opencamo.phone.camera.FrameHub] (drop, never queue).
 */
class OpencamoServer(
    private val port: Int,
    private val cameraRepository: CameraRepository,
    private val streamController: StreamController,
    private val deviceName: String,
    private val onClientsChanged: (Int) -> Unit,
) {
    private val streamSessions = CopyOnWriteArrayList<StreamSession>()
    private val deliveredFrames = AtomicLong(0)
    private val streamLock = Any()

    @Volatile
    private var currentConfig: StreamConfig? = null

    @Volatile
    private var running = false

    private var serverSocket: ServerSocket? = null

    private class StreamSession(val socket: Socket, val config: StreamConfig, val thread: Thread)

    @Throws(IOException::class)
    fun start() {
        check(!running) { "server already started" }
        val socket = ServerSocket(port)
        serverSocket = socket
        running = true
        Thread({
            while (running) {
                try {
                    val client = socket.accept()
                    Thread({ handleConnection(client) }, "opencamo-conn").start()
                } catch (e: IOException) {
                    if (running) Log.w(TAG, "Accept failed: ${e.message}")
                }
            }
        }, "opencamo-accept").start()
    }

    fun stop() {
        if (!running) return
        running = false
        try {
            serverSocket?.close()
        } catch (e: IOException) {
            Log.w(TAG, "Error closing server socket: ${e.message}")
        }
        serverSocket = null
        closeAllStreamSessions()
        currentConfig = null
        onClientsChanged(0)
    }

    fun clientCount(): Int = streamSessions.size

    fun deliveredCount(): Long = deliveredFrames.get()

    fun currentConfig(): StreamConfig? = currentConfig

    // --------------------------------------------------------------------- request handling

    private fun handleConnection(socket: Socket) {
        try {
            socket.tcpNoDelay = true
            val reader = BufferedReader(InputStreamReader(socket.getInputStream(), Charsets.UTF_8))
            val output = socket.getOutputStream()
            processRequest(socket, reader, output)
        } catch (e: IOException) {
            // Client went away mid-request; nothing to clean up beyond the finally block.
        } finally {
            try {
                socket.close()
            } catch (e: IOException) {
                // Already closed.
            }
        }
    }

    private fun processRequest(socket: Socket, reader: BufferedReader, output: OutputStream) {
        val requestLine = reader.readLine() ?: return
        var headerLines = 0
        while (true) {
            val line = reader.readLine() ?: break
            if (line.isEmpty()) break
            if (++headerLines > MAX_HEADER_LINES) {
                sendError(output, 400, "too many headers")
                return
            }
        }

        val requestParts = requestLine.split(' ')
        if (requestParts.size < 3) {
            sendError(output, 400, "malformed request line")
            return
        }
        if (requestParts[0] != "GET") {
            sendError(output, 405, "method not allowed")
            return
        }

        val (path, query) = splitTarget(requestParts[1])
        try {
            when (path) {
                PATH_CAPABILITIES -> sendJson(output, STATUS_OK, buildCapabilities())
                PATH_PING -> sendPong(output)
                PATH_STREAM -> handleStream(socket, output, parseQuery(query))
                else -> sendError(output, 404, "not found")
            }
        } catch (e: IOException) {
            throw e // client went away mid-response
        } catch (e: Exception) {
            // Never leave the client hanging: e.g. CameraAccessException during enumeration.
            Log.e(TAG, "Request failed: ${e.message}")
            try {
                sendError(output, 500, "internal error")
            } catch (ignored: IOException) {
                // Client already gone.
            }
        }
    }

    // --------------------------------------------------------------------------- capabilities

    private fun buildCapabilities(): String {
        // Fresh query per request so hot-plugged/switched cameras appear (protocol rule).
        val cameras = cameraRepository.query()
        val camerasJson = JSONArray()
        for (camera in cameras) {
            val resolutions = JSONArray()
            for (size in camera.sizes) {
                resolutions.put(
                    JSONObject()
                        .put("width", size.width)
                        .put("height", size.height)
                        .put("fps", JSONArray(camera.fpsRates))
                )
            }
            camerasJson.put(
                JSONObject()
                    .put("id", camera.id)
                    .put("label", camera.label)
                    .put("resolutions", resolutions)
            )
        }
        return JSONObject()
            .put("protocol", PROTOCOL_VERSION)
            .put("app", APP_ID)
            .put("version", APP_VERSION)
            .put("device_name", deviceName)
            .put("cameras", camerasJson)
            .toString()
    }

    // ------------------------------------------------------------------------------- streaming

    private fun handleStream(socket: Socket, output: OutputStream, query: Map<String, String>) {
        val cameras = cameraRepository.query()
        val cameraId = query[PARAM_CAMERA] ?: DEFAULT_CAMERA_ID
        val camera = cameras.find { it.id == cameraId }
            ?: run {
                sendError(output, 404, "unknown camera: $cameraId")
                return
            }

        val hasWidth = query.containsKey(PARAM_WIDTH)
        val hasHeight = query.containsKey(PARAM_HEIGHT)
        val size: Size = when {
            !hasWidth && !hasHeight -> camera.largestSize
            hasWidth && hasHeight -> {
                val width = query[PARAM_WIDTH]!!.toIntOrNull()
                val height = query[PARAM_HEIGHT]!!.toIntOrNull()
                if (width == null || height == null) {
                    sendError(output, 400, "width and height must be integers")
                    return
                }
                camera.sizes.find { it.width == width && it.height == height }
                    ?: run {
                        sendError(output, 400, "unsupported resolution: ${width}x${height}")
                        return
                    }
            }
            else -> {
                sendError(output, 400, "width and height must be specified together")
                return
            }
        }

        val requestedFps = query[PARAM_FPS]?.let { raw ->
            raw.toIntOrNull() ?: run {
                sendError(output, 400, "fps must be an integer")
                return
            }
        } ?: DEFAULT_FPS
        val fps = camera.clampFps(requestedFps)

        val config = StreamConfig(camera, size, fps)
        val session = synchronized(streamLock) {
            val active = currentConfig
            if (active == null || active.key != config.key) {
                // Changing camera/width/height/fps closes the old stream (single active stream).
                closeAllStreamSessions()
                currentConfig = config
                streamController.configure(camera, size, fps)
            }
            val newSession = StreamSession(socket, config, Thread.currentThread())
            streamSessions.add(newSession)
            newSession
        }
        onClientsChanged(streamSessions.size)

        try {
            sendStreamHeaders(output)
            writeFrames(session, output)
        } finally {
            streamSessions.remove(session)
            onClientsChanged(streamSessions.size)
        }
    }

    private fun sendStreamHeaders(output: OutputStream) {
        val head = "HTTP/1.1 200 OK\r\n" +
            "Content-Type: multipart/x-mixed-replace; boundary=frame\r\n" +
            "Cache-Control: no-cache\r\n" +
            "Connection: close\r\n" +
            "\r\n"
        output.write(head.toByteArray(Charsets.US_ASCII))
        output.flush()
    }

    private fun writeFrames(session: StreamSession, output: OutputStream) {
        val hub = streamController.hub
        var last: ByteArray? = null
        while (!session.socket.isClosed && !Thread.currentThread().isInterrupted) {
            val frame = hub.awaitNext(last) ?: break
            last = frame
            val partHeader = "--frame\r\n" +
                "Content-Type: image/jpeg\r\n" +
                "Content-Length: ${frame.size}\r\n" +
                "\r\n"
            output.write(partHeader.toByteArray(Charsets.US_ASCII))
            output.write(frame)
            output.write(CRLF_BYTES)
            output.flush()
            deliveredFrames.incrementAndGet()
        }
    }

    private fun closeAllStreamSessions() {
        for (session in streamSessions) {
            session.thread.interrupt()
            try {
                session.socket.close()
            } catch (e: IOException) {
                // Already closed.
            }
        }
        streamSessions.clear()
    }

    // ------------------------------------------------------------------------- response writes

    private fun sendJson(output: OutputStream, statusLine: String, body: String) {
        val bytes = body.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 $statusLine\r\n" +
            "Content-Type: application/json\r\n" +
            "Content-Length: ${bytes.size}\r\n" +
            "Connection: close\r\n" +
            "\r\n"
        output.write(head.toByteArray(Charsets.US_ASCII))
        output.write(bytes)
        output.flush()
    }

    private fun sendError(output: OutputStream, code: Int, message: String) {
        val statusLine = when (code) {
            400 -> "400 Bad Request"
            404 -> "404 Not Found"
            405 -> "405 Method Not Allowed"
            else -> "$code Error"
        }
        sendJson(output, statusLine, JSONObject().put("error", message).toString())
    }

    private fun sendPong(output: OutputStream) {
        val body = PONG.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 200 OK\r\n" +
            "Content-Type: text/plain\r\n" +
            "Content-Length: ${body.size}\r\n" +
            "Connection: close\r\n" +
            "\r\n"
        output.write(head.toByteArray(Charsets.US_ASCII))
        output.write(body)
        output.flush()
    }

    // --------------------------------------------------------------------------- query parsing

    private fun splitTarget(target: String): Pair<String, String?> {
        val index = target.indexOf('?')
        return if (index >= 0) {
            target.substring(0, index) to target.substring(index + 1)
        } else {
            target to null
        }
    }

    private fun parseQuery(query: String?): Map<String, String> {
        if (query.isNullOrEmpty()) return emptyMap()
        return query.split('&')
            .mapNotNull { pair ->
                if (pair.isEmpty()) return@mapNotNull null
                val eq = pair.indexOf('=')
                val key = if (eq >= 0) pair.substring(0, eq) else pair
                val value = if (eq >= 0) pair.substring(eq + 1) else ""
                urlDecode(key) to urlDecode(value)
            }
            .toMap()
    }

    private fun urlDecode(value: String): String {
        if (value.indexOf('%') < 0 && value.indexOf('+') < 0) return value
        val out = StringBuilder(value.length)
        var i = 0
        while (i < value.length) {
            val c = value[i]
            when {
                c == '+' -> {
                    out.append(' ')
                    i++
                }
                c == '%' && i + 2 < value.length -> {
                    val decoded = value.substring(i + 1, i + 3).toIntOrNull(16)
                    if (decoded != null) {
                        out.append(decoded.toChar())
                        i += 3
                    } else {
                        out.append(c)
                        i++
                    }
                }
                else -> {
                    out.append(c)
                    i++
                }
            }
        }
        return out.toString()
    }

    companion object {
        const val DEFAULT_PORT = 47777

        private const val TAG = "OpencamoServer"
        private const val STATUS_OK = "200 OK"
        private const val PROTOCOL_VERSION = 1
        private const val APP_ID = "opencamo"
        private const val APP_VERSION = "0.1.0"

        private const val PATH_CAPABILITIES = "/v1/capabilities"
        private const val PATH_STREAM = "/v1/stream"
        private const val PATH_PING = "/v1/ping"

        private const val PARAM_CAMERA = "camera"
        private const val PARAM_WIDTH = "width"
        private const val PARAM_HEIGHT = "height"
        private const val PARAM_FPS = "fps"

        private const val DEFAULT_CAMERA_ID = "back"
        private const val DEFAULT_FPS = 30

        private const val MAX_HEADER_LINES = 100
        private const val PONG = "pong"
        private val CRLF_BYTES = "\r\n".toByteArray(Charsets.US_ASCII)
    }
}
