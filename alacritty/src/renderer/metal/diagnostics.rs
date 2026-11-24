use crate::renderer::metrics::Samples;

#[derive(Default)]
pub(super) struct Diagnostics {
    pub window: u64,
    pub pipeline_build_us: f64,
    pub pipeline_reused: bool,
    pub submitted: u64,
    pub completed: u64,
    pub drawable_retries: u64,
    pub in_flight_retries: u64,
    pub instance_allocations: u64,
    pub upload_allocations: u64,
    pub buffer_reuses: u64,
    pub cached_buffer_bytes: usize,
    pub instance_bytes: u64,
    pub upload_bytes: u64,
    pub gpu: Samples,
    pub acquire: Samples,
    pub encode: Samples,
}

impl Diagnostics {
    pub fn log(&self) {
        log::info!(
            "Metal metrics: {}",
            serde_json::json!({
                "window": self.window,
                "pipeline_build_us": self.pipeline_build_us,
                "pipeline_reused": self.pipeline_reused,
                "submitted_frames": self.submitted,
                "completed_frames": self.completed,
                "drawable_retries": self.drawable_retries,
                "in_flight_retries": self.in_flight_retries,
                "instance_allocations": self.instance_allocations,
                "upload_allocations": self.upload_allocations,
                "buffer_reuses": self.buffer_reuses,
                "cached_buffer_bytes": self.cached_buffer_bytes,
                "instance_bytes": self.instance_bytes,
                "upload_bytes": self.upload_bytes,
                "gpu_execution_us": self.gpu.summary(),
                "drawable_acquire_us": self.acquire.summary(),
                "cpu_encode_us": self.encode.summary(),
            })
        );
    }
}
