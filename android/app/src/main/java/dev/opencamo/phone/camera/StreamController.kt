package dev.opencamo.phone.camera

import android.app.Activity
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Matrix
import android.hardware.camera2.CaptureRequest
import android.util.Log
import android.util.Size
import android.view.Surface
import androidx.camera.camera2.interop.Camera2CameraControl
import androidx.camera.camera2.interop.Camera2CameraInfo
import androidx.camera.camera2.interop.CaptureRequestOptions
import androidx.camera.camera2.interop.ExperimentalCamera2Interop
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.lifecycle.LifecycleOwner
import java.io.ByteArrayOutputStream
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.TimeUnit

/**
 * Single-slot frame channel: [publish] overwrites the previous frame unconditionally and
 * wakes every reader (latest-frame-wins, frames are dropped, never queued). Readers that
 * cannot keep up simply miss frames because they always receive the newest slot contents.
 */
class FrameHub {
    private var latest: ByteArray? = null

    @Synchronized
    fun publish(frame: ByteArray) {
        latest = frame
        notifyAll()
    }

    /**
     * Returns the newest frame that is not identical to [after], waiting for one to arrive.
     * Returns null when the thread is interrupted or the hub was cleared past [after].
     */
    @Synchronized
    fun awaitNext(after: ByteArray?): ByteArray? {
        try {
            while (latest === after && !Thread.currentThread().isInterrupted) {
                wait(WAIT_POLL_MS)
            }
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
            return null
        }
        return if (Thread.currentThread().isInterrupted) null else latest
    }

    /** Drops the current frame, e.g. when the camera is reconfigured. */
    @Synchronized
    fun clear() {
        latest = null
    }

    private companion object {
        const val WAIT_POLL_MS = 250L
    }
}

/**
 * Owns the CameraX pipeline. Binds a single [ImageAnalysis] use case at the active stream
 * configuration and publishes JPEG-encoded frames into [hub]. Reconfiguration is serialized
 * on a dedicated bind thread.
 */
class StreamController(
    context: Context,
    private val lifecycleOwner: LifecycleOwner,
) {
    val hub = FrameHub()

    private val appContext = context.applicationContext
    private val displayRotation: Int =
        (context as? Activity)?.windowManager?.defaultDisplay?.rotation ?: Surface.ROTATION_0
    private val analyzerExecutor: ExecutorService =
        Executors.newSingleThreadExecutor { r -> Thread(r, "opencamo-analyzer") }
    private val bindExecutor: ExecutorService =
        Executors.newSingleThreadExecutor { r -> Thread(r, "opencamo-bind") }

    private var provider: ProcessCameraProvider? = null

    /** (Re)binds the camera for the given configuration. Returns immediately; runs async. */
    fun configure(camera: CameraDescriptor, size: Size, fps: Int) {
        try {
            bindExecutor.execute { doConfigure(camera, size, fps) }
        } catch (e: RejectedExecutionException) {
            // Controller already released; nothing left to configure.
            Log.w(TAG, "configure() after release ignored")
        }
    }

    /** Unbinds the camera and releases the camera provider. The controller is unusable afterwards. */
    fun release() {
        bindExecutor.execute {
            provider?.unbindAll()
            hub.clear()
        }
        bindExecutor.shutdown()
        analyzerExecutor.shutdown()
    }

    @OptIn(ExperimentalCamera2Interop::class)
    private fun doConfigure(camera: CameraDescriptor, size: Size, fps: Int) {
        try {
            val cameraProvider = provider
                ?: ProcessCameraProvider.getInstance(appContext)
                    .get(BIND_TIMEOUT_SECONDS, TimeUnit.SECONDS)
                    .also { provider = it }

            cameraProvider.unbindAll()
            hub.clear()

            val resolutionSelector = ResolutionSelector.Builder()
                .setResolutionStrategy(
                    ResolutionStrategy(
                        size,
                        ResolutionStrategy.FALLBACK_RULE_CLOSEST_HIGHER_THEN_LOWER,
                    )
                )
                .build()
            val imageAnalysis = ImageAnalysis.Builder()
                .setResolutionSelector(resolutionSelector)
                // CameraX drops frames at the source as well; combined with FrameHub this
                // keeps end-to-end latency bounded (latest-frame-wins, never queued).
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .setTargetRotation(displayRotation)
                .build()
            imageAnalysis.setAnalyzer(analyzerExecutor, ::analyze)

            val selector = CameraSelector.Builder()
                .addCameraFilter { cameras ->
                    cameras.filter { Camera2CameraInfo.from(it).cameraId == camera.camera2Id }
                }
                .build()
            val bound = cameraProvider.bindToLifecycle(lifecycleOwner, selector, imageAnalysis)
            applyAeTargetFps(bound, camera, fps)
        } catch (e: Exception) {
            Log.e(TAG, "Failed to bind camera ${camera.id} at ${size.width}x${size.height}", e)
        }
    }

    private fun analyze(imageProxy: ImageProxy) {
        try {
            val frame = encodeJpeg(imageProxy)
            if (frame != null) {
                hub.publish(frame)
            }
        } catch (e: Exception) {
            Log.w(TAG, "Frame dropped: ${e.message}")
        } finally {
            imageProxy.close()
        }
    }

    /** YUV -> Bitmap (CameraX libyuv conversion), upright rotation, then JPEG at ~quality 80. */
    private fun encodeJpeg(imageProxy: ImageProxy): ByteArray? {
        val degrees = imageProxy.imageInfo.rotationDegrees
        val raw = imageProxy.toBitmap()
        val upright = if (degrees == 0 || raw.width == 0 || raw.height == 0) {
            raw
        } else {
            val matrix = Matrix().apply { postRotate(degrees.toFloat()) }
            Bitmap.createBitmap(raw, 0, 0, raw.width, raw.height, matrix, true)
        }
        val output = ByteArrayOutputStream()
        upright.compress(Bitmap.CompressFormat.JPEG, JPEG_QUALITY, output)
        if (upright !== raw) upright.recycle()
        raw.recycle()
        return output.toByteArray()
    }

    @OptIn(ExperimentalCamera2Interop::class)
    private fun applyAeTargetFps(camera: Camera, descriptor: CameraDescriptor, fps: Int) {
        val range = descriptor.aeRangeFor(fps) ?: return
        val options = CaptureRequestOptions.Builder()
            .setCaptureRequestParameter(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, range)
            .build()
        Camera2CameraControl.from(camera.cameraControl).setCaptureRequestOptions(options)
    }

    private companion object {
        const val TAG = "StreamController"
        const val JPEG_QUALITY = 80
        const val BIND_TIMEOUT_SECONDS = 10L
    }
}
