package dev.opencamo.phone.camera

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.util.Range
import android.util.Size
import android.view.SurfaceHolder
import kotlin.math.abs

/**
 * A single physical camera as reported to protocol clients.
 *
 * [sizes] comes from the Camera2 [StreamConfigurationMap][android.hardware.camera2.params.StreamConfigurationMap]
 * via `getOutputSizes(SurfaceHolder::class.java)`, and [fpsRates] is derived from the camera's
 * `CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES` (see [supportedFpsRates]).
 */
data class CameraDescriptor(
    /** Protocol id, e.g. "back", "front", "back2". */
    val id: String,
    val label: String,
    /** The Camera2 camera id used with [CameraManager.getCameraCharacteristics]. */
    val camera2Id: String,
    val lensFacing: Int?,
    /** Supported sizes, sorted largest first. */
    val sizes: List<Size>,
    /** Discrete supported frame rates, sorted ascending. */
    val fpsRates: List<Int>,
    /** Raw Camera2 AE target FPS ranges the rates were derived from. */
    val aeFpsRanges: List<Range<Int>>,
) {
    val largestSize: Size
        get() = sizes.maxByOrNull { it.width * it.height }
            ?: throw IllegalStateException("camera $id has no supported sizes")

    /** Clamps a requested frame rate to the nearest supported rate (ties resolve to the lower rate). */
    fun clampFps(requested: Int): Int =
        fpsRates.minWithOrNull(compareBy<Int> { abs(it - requested) }.thenBy { it }) ?: requested

    /** The narrowest advertised AE range containing [fps], used as the hardware AE target. */
    fun aeRangeFor(fps: Int): Range<Int>? =
        aeFpsRanges
            .filter { fps >= it.lower && fps <= it.upper }
            .minByOrNull { it.upper - it.lower }
}

/**
 * Enumerates cameras with plain Camera2 queries. Every call re-queries the camera list so
 * hot-plugged/switched cameras appear immediately (required by the protocol for each
 * `/v1/capabilities` request).
 */
class CameraRepository(private val context: Context) {

    fun query(): List<CameraDescriptor> {
        val cameraManager = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val descriptors = mutableListOf<CameraDescriptor>()
        var backCount = 0
        var frontCount = 0
        var otherCount = 0

        for (camera2Id in cameraManager.cameraIdList) {
            val characteristics = cameraManager.getCameraCharacteristics(camera2Id)
            val facing = characteristics.get(CameraCharacteristics.LENS_FACING)
            val streamMap =
                characteristics.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: continue
            val sizes = streamMap.getOutputSizes(SurfaceHolder::class.java)
                ?.map { Size(it.width, it.height) }
                ?.distinct()
                ?: continue
            if (sizes.isEmpty()) continue

            val aeRanges = (
                characteristics.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES)
                    ?: arrayOf(Range(15, 30))
                ).toList()

            val baseId = when (facing) {
                CameraCharacteristics.LENS_FACING_BACK -> "back"
                CameraCharacteristics.LENS_FACING_FRONT -> "front"
                else -> "camera"
            }
            val index = when (baseId) {
                "back" -> ++backCount
                "front" -> ++frontCount
                else -> ++otherCount
            }
            val id = if (index == 1) baseId else "$baseId$index"
            val label = when (baseId) {
                "back" -> if (index == 1) "Back camera" else "Back camera $index"
                "front" -> if (index == 1) "Front camera" else "Front camera $index"
                else -> "Camera $camera2Id"
            }

            descriptors += CameraDescriptor(
                id = id,
                label = label,
                camera2Id = camera2Id,
                lensFacing = facing,
                sizes = sizes.sortedWith(
                    compareByDescending<Size> { it.width * it.height }.thenBy { it.width }
                ),
                fpsRates = supportedFpsRates(aeRanges),
                aeFpsRanges = aeRanges,
            )
        }

        // Protocol example order: back cameras first, then front, then anything else.
        return descriptors.sortedBy {
            when (it.lensFacing) {
                CameraCharacteristics.LENS_FACING_BACK -> 0
                CameraCharacteristics.LENS_FACING_FRONT -> 1
                else -> 2
            }
        }
    }

    companion object {
        private val STANDARD_RATES = listOf(15, 20, 24, 30, 50, 60, 90, 120)

        /**
         * Derives the discrete frame rates the camera can deliver from its Camera2 AE target
         * FPS ranges: every standard rate that falls inside at least one advertised range.
         * A single `[15,30]` range therefore reports `[15, 20, 24, 30]`; a device that only
         * advertises `[7,10]` falls back to its maximum (`[10]`).
         */
        fun supportedFpsRates(ranges: List<Range<Int>>): List<Int> {
            val rates = ranges
                .flatMap { range -> STANDARD_RATES.filter { it >= range.lower && it <= range.upper } }
                .distinct()
                .sorted()
            return rates.ifEmpty { listOf(ranges.maxOf { it.upper }) }
        }
    }
}
