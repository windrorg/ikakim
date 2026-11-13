#ifndef IKAKIM_FFI_H
#define IKAKIM_FFI_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct IkakimEngineHandle IkakimEngineHandle;

typedef struct IkakimEngineConfig {
    uint32_t main_width;
    uint32_t main_height;
    uint32_t preview_width;
    uint32_t preview_height;
    uint32_t target_fps;
    uint32_t initial_bitrate_bps;
    uint16_t http_port;
} IkakimEngineConfig;

typedef struct IkakimLayerTransform {
    float x;
    float y;
    float scale_x;
    float scale_y;
    float rotation_rad;
    float opacity;
    int32_t z_order;
    bool visible;
} IkakimLayerTransform;

typedef struct IkakimEngineStats {
    uint64_t ticks;
    uint16_t ui_port;
    uint32_t current_bitrate_bps;
    uint64_t encoded_video_packets;
    uint64_t encoded_audio_packets;
    uint32_t active_gpu_layers;
    bool streaming_active;
    bool apple_zero_copy_active;
    bool android_ahb_active;
} IkakimEngineStats;

/**
 * Returns recommended default configuration (1080p60 main, 720p preview, 4 Mbps, ephemeral HTTP port 0).
 */
IkakimEngineConfig ikakim_engine_default_config(void);

/**
 * Creates a new streaming engine instance and starts the embedded local HTTP/WS UI server.
 * Passing NULL uses default configuration. Returns NULL on initialization failure.
 */
IkakimEngineHandle *ikakim_engine_create(const IkakimEngineConfig *config);

/**
 * Stops the embedded server/RTMPS publisher and frees all engine resources. Safe on NULL.
 */
void ikakim_engine_destroy(IkakimEngineHandle *handle);

/**
 * Returns the bound TCP port of the embedded local Web UI server, or 0 on invalid handle.
 */
uint16_t ikakim_engine_ui_port(const IkakimEngineHandle *handle);

/**
 * Uploads or updates an RGBA8 layer (including Top-Bottom Alpha RGBA when top_bottom_alpha=true).
 * Returns 0 on success, negative error code on failure.
 */
int32_t ikakim_engine_submit_rgba_layer(
    IkakimEngineHandle *handle,
    uint32_t layer_id,
    bool is_base_layer,
    bool top_bottom_alpha,
    uint32_t width,
    uint32_t height,
    const uint8_t *rgba_ptr,
    size_t rgba_len
);

/**
 * Zero-copy imports a CoreVideo CVPixelBufferRef via CVMetalTextureCache on Apple (iOS/macOS).
 * Accepts void* so pure C headers do not require CoreVideo.h.
 * Returns 0 on success, -2 if unsupported on this OS, or other negative error code.
 */
int32_t ikakim_engine_submit_cv_pixel_buffer(
    IkakimEngineHandle *handle,
    uint32_t layer_id,
    bool is_base_layer,
    void *cv_pixel_buffer_ref
);

/**
 * Records/updates an Android AHardwareBuffer external Vulkan import descriptor for a layer.
 * Returns 0 on success, negative error code on failure.
 */
int32_t ikakim_engine_submit_ahardware_buffer_meta(
    IkakimEngineHandle *handle,
    uint32_t layer_id,
    bool is_base_layer,
    uint64_t buffer_id,
    uint32_t width,
    uint32_t height,
    uint32_t android_format,
    uint64_t usage_flags
);

/**
 * Pushes interleaved f32 stereo PCM samples (48kHz target; resampled automatically if needed)
 * into the lock-free audio mixer channel (0 = primary mic/camera, 1 = external).
 * Returns 0 on success, negative error code on failure.
 */
int32_t ikakim_engine_push_audio_f32(
    IkakimEngineHandle *handle,
    uint32_t channel_index,
    const float *interleaved_pcm,
    size_t sample_count
);

/**
 * Updates layer transform via the shared control bridge so both C-API and Web UI stay synchronized.
 */
int32_t ikakim_engine_set_layer_transform(
    IkakimEngineHandle *handle,
    uint32_t layer_id,
    const IkakimLayerTransform *transform
);

/**
 * Executes one A/V pipeline tick: audio mix + AAC encode, GPU compositing, ABR evaluation,
 * video encode, and RTMPS/MSE packet flush.
 */
int32_t ikakim_engine_step_tick(
    IkakimEngineHandle *handle,
    uint64_t queued_rtmp_bytes,
    uint64_t elapsed_ms
);

/**
 * Starts RTMPS publishing to the provided rtmp:// or rtmps:// URL (UTF-8 NUL-terminated).
 */
int32_t ikakim_engine_start_rtmps(
    IkakimEngineHandle *handle,
    const char *url_utf8
);

/**
 * Stops active RTMPS publishing if running.
 */
int32_t ikakim_engine_stop_rtmps(IkakimEngineHandle *handle);

/**
 * Requests an immediate IDR keyframe on the main video encoder.
 */
int32_t ikakim_engine_request_keyframe(IkakimEngineHandle *handle);

/**
 * Populates runtime statistics snapshot. Returns 0 on success, negative on error.
 */
int32_t ikakim_engine_get_stats(
    const IkakimEngineHandle *handle,
    IkakimEngineStats *out_stats
);

/**
 * Copies the last error message (NUL-terminated) into buf and returns string length,
 * or 0 if no error is recorded.
 */
int32_t ikakim_engine_last_error(
    const IkakimEngineHandle *handle,
    char *buf,
    size_t buf_len
);

#ifdef __cplusplus
}
#endif

#endif /* IKAKIM_FFI_H */
