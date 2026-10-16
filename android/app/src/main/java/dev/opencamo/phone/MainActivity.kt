package dev.opencamo.phone

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.View
import android.widget.AdapterView
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.Spinner
import android.widget.TextView
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import dev.opencamo.phone.camera.CameraDescriptor
import dev.opencamo.phone.camera.CameraRepository
import dev.opencamo.phone.camera.StreamController
import dev.opencamo.phone.server.OpencamoServer
import java.io.IOException
import java.util.concurrent.Executors

/**
 * Single activity: shows the device address and port, lets the user pick camera/resolution/fps,
 * starts and stops the HTTP server, and shows live status (clients connected, delivered FPS,
 * active stream configuration).
 */
class MainActivity : AppCompatActivity() {

    private lateinit var addressText: TextView
    private lateinit var cameraSpinner: Spinner
    private lateinit var resolutionSpinner: Spinner
    private lateinit var fpsSpinner: Spinner
    private lateinit var toggleButton: Button
    private lateinit var statusText: TextView

    private val repository by lazy { CameraRepository(this) }
    private val enumerationExecutor =
        Executors.newSingleThreadExecutor { r -> Thread(r, "opencamo-enumerate") }
    private val uiHandler = Handler(Looper.getMainLooper())

    private var cameras: List<CameraDescriptor> = emptyList()
    private var server: OpencamoServer? = null
    private var streamController: StreamController? = null

    private var lastDelivered = 0L
    private var lastTickAt = 0L
    private var shownFps = 0

    private val cameraPermissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                startServer()
            } else {
                statusText.setText(R.string.camera_permission_denied)
            }
        }

    private val statusTicker = object : Runnable {
        override fun run() {
            val activeServer = server ?: return
            val now = SystemClock.elapsedRealtime()
            val delivered = activeServer.deliveredCount()
            val elapsed = now - lastTickAt
            if (elapsed > 0) {
                shownFps = ((delivered - lastDelivered) * 1000 / elapsed).toInt()
                lastDelivered = delivered
                lastTickAt = now
            }
            updateStatus()
            uiHandler.postDelayed(this, TICK_INTERVAL_MS)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        addressText = findViewById(R.id.text_address)
        cameraSpinner = findViewById(R.id.spinner_camera)
        resolutionSpinner = findViewById(R.id.spinner_resolution)
        fpsSpinner = findViewById(R.id.spinner_fps)
        toggleButton = findViewById(R.id.button_toggle)
        statusText = findViewById(R.id.text_status)

        addressText.text = serverAddress()
        toggleButton.setOnClickListener { onToggleClicked() }
        cameraSpinner.onItemSelectedListener = object : AdapterView.OnItemSelectedListener {
            override fun onItemSelected(
                parent: AdapterView<*>?,
                view: View?,
                position: Int,
                id: Long,
            ) {
                cameras.getOrNull(position)?.let { populateResolutionSpinners(it) }
            }

            override fun onNothingSelected(parent: AdapterView<*>?) = Unit
        }

        // The camera list is refreshed in onResume() (also on the first launch).
    }

    override fun onResume() {
        super.onResume()
        addressText.text = serverAddress()
        if (server == null) {
            refreshCameras()
        }
    }

    override fun onDestroy() {
        uiHandler.removeCallbacks(statusTicker)
        stopServer()
        enumerationExecutor.shutdownNow()
        super.onDestroy()
    }

    // ------------------------------------------------------------------------ camera list UI

    /** Re-queries the camera list in the background and repopulates the pickers. */
    private fun refreshCameras() {
        val previous = cameras.getOrNull(cameraSpinner.selectedItemPosition)
        enumerationExecutor.execute {
            val found = try {
                repository.query()
            } catch (e: Exception) {
                Log.w(TAG, "Camera enumeration failed: ${e.message}")
                emptyList()
            }
            runOnUiThread {
                cameras = found
                if (found.isEmpty()) {
                    toggleButton.isEnabled = false
                    statusText.setText(R.string.no_cameras)
                    return@runOnUiThread
                }
                toggleButton.isEnabled = true
                populateCameraSpinner(previous)
                updateStatus()
            }
        }
    }

    private fun populateCameraSpinner(previous: CameraDescriptor?) {
        cameraSpinner.adapter = spinnerAdapter(cameras.map { it.label })
        val index = if (previous != null) {
            cameras.indexOfFirst { it.id == previous.id }
        } else {
            cameras.indexOfFirst { it.id == "back" }
        }
        cameraSpinner.setSelection(if (index >= 0) index else 0)
    }

    private fun populateResolutionSpinners(camera: CameraDescriptor) {
        resolutionSpinner.adapter =
            spinnerAdapter(camera.sizes.map { "${it.width} x ${it.height}" })
        fpsSpinner.adapter = spinnerAdapter(camera.fpsRates.map { "$it fps" })
        val preferred = camera.fpsRates.indexOf(DEFAULT_FPS)
        fpsSpinner.setSelection(if (preferred >= 0) preferred else camera.fpsRates.size - 1)
    }

    private fun spinnerAdapter(items: List<String>): ArrayAdapter<String> =
        ArrayAdapter(this, android.R.layout.simple_spinner_item, items).apply {
            setDropDownViewResource(android.R.layout.simple_spinner_dropdown_item)
        }

    // -------------------------------------------------------------------------- server control

    private fun onToggleClicked() {
        if (server != null) {
            stopServer()
        } else {
            requestStart()
        }
    }

    private fun requestStart() {
        val granted = ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            startServer()
        } else {
            cameraPermissionLauncher.launch(Manifest.permission.CAMERA)
        }
    }

    private fun startServer() {
        if (server != null) return
        val camera = selectedCamera()
        if (camera == null) {
            statusText.setText(R.string.no_cameras)
            return
        }
        val size = selectedSize(camera)
        val fps = selectedFps(camera)

        val controller = StreamController(this, this)
        val newServer = OpencamoServer(
            port = OpencamoServer.DEFAULT_PORT,
            cameraRepository = repository,
            streamController = controller,
            deviceName = Build.MODEL,
            onClientsChanged = { runOnUiThread { updateStatus() } },
        )
        try {
            newServer.start()
        } catch (e: IOException) {
            Log.e(TAG, "Server failed to start: ${e.message}")
            statusText.text =
                "Failed to start server on port ${OpencamoServer.DEFAULT_PORT}: ${e.message}"
            controller.release()
            return
        }

        server = newServer
        streamController = controller
        // Pre-warm the camera at the configuration shown in the UI. Clients can still
        // request other configurations; the server rebinds and closes the old stream.
        controller.configure(camera, size, fps)

        cameraSpinner.isEnabled = false
        resolutionSpinner.isEnabled = false
        fpsSpinner.isEnabled = false
        toggleButton.setText(R.string.stop)

        lastDelivered = 0L
        lastTickAt = SystemClock.elapsedRealtime()
        shownFps = 0
        uiHandler.removeCallbacks(statusTicker)
        uiHandler.postDelayed(statusTicker, TICK_INTERVAL_MS)
        updateStatus()
    }

    private fun stopServer() {
        uiHandler.removeCallbacks(statusTicker)
        server?.stop()
        server = null
        streamController?.release()
        streamController = null
        cameraSpinner.isEnabled = true
        resolutionSpinner.isEnabled = true
        fpsSpinner.isEnabled = true
        toggleButton.setText(R.string.start)
        shownFps = 0
        updateStatus()
    }

    private fun updateStatus() {
        val activeServer = server
        if (activeServer == null) {
            statusText.setText(R.string.status_stopped)
            return
        }
        val config = activeServer.currentConfig()
        val active = if (config != null) {
            "${config.camera.label} - ${config.size.width} x ${config.size.height} @ ${config.fps} fps"
        } else {
            "waiting for first client"
        }
        statusText.text = "Server running on port ${OpencamoServer.DEFAULT_PORT}\n" +
            "Clients connected: ${activeServer.clientCount()}\n" +
            "Delivered: $shownFps fps\n" +
            "Active: $active"
    }

    private fun selectedCamera(): CameraDescriptor? =
        cameras.getOrNull(cameraSpinner.selectedItemPosition)

    private fun selectedSize(camera: CameraDescriptor) =
        camera.sizes.getOrNull(resolutionSpinner.selectedItemPosition) ?: camera.largestSize

    private fun selectedFps(camera: CameraDescriptor) =
        camera.fpsRates.getOrNull(fpsSpinner.selectedItemPosition) ?: DEFAULT_FPS

    private fun serverAddress(): String =
        "http://${deviceIpAddress()}:${OpencamoServer.DEFAULT_PORT}"

    /** Best-effort LAN IP from WifiManager; zero means Wi-Fi is off or not connected. */
    @Suppress("DEPRECATION")
    private fun deviceIpAddress(): String {
        val wifiManager =
            applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val ip = wifiManager.connectionInfo?.ipAddress ?: 0
        if (ip == 0) return "Wi-Fi not connected"
        return "${ip and 0xFF}.${(ip shr 8) and 0xFF}.${(ip shr 16) and 0xFF}.${(ip shr 24) and 0xFF}"
    }

    companion object {
        private const val TAG = "MainActivity"
        private const val DEFAULT_FPS = 30
        private const val TICK_INTERVAL_MS = 1000L
    }
}
