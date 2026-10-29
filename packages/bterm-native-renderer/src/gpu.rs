//! Private bounded texture upload and a single alpha-blended quad batch.
use crate::renderer::{FrameTelemetry, RenderError, telemetry::elapsed_ns};
use bterm_native_platform::{PhysicalSize, SurfaceTarget};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

const ATLAS: u32 = 4096;
const MAX_VERTICES: usize = 4_000_000;
const MAX_CACHE_ENTRIES: usize = 16_384;
const MAX_CACHE_BYTES: usize = ATLAS as usize * ATLAS as usize * 4;

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct ContentKey {
    width: u32,
    height: u32,
    hash: u64,
}
impl ContentKey {
    fn new(width: u32, height: u32, bytes: &[u8]) -> Self {
        let mut hasher = DefaultHasher::new();
        (width, height, bytes).hash(&mut hasher);
        Self {
            width,
            height,
            hash: hasher.finish(),
        }
    }
}
struct CachedBitmap {
    pixels: Box<[u8]>,
    origin: [u32; 2],
}

/// No eviction within a frame: every returned UV remains valid until next begin.
/// Pixel storage and collision-chain metadata have independent hard bounds.
struct AtlasCache {
    entries: HashMap<ContentKey, Vec<CachedBitmap>>,
    shelf: [u32; 3],
    pixel_bytes: usize,
    count: usize,
    reset_pending: bool,
    fresh_frame: bool,
    frame_error: Option<&'static str>,
}
impl Default for AtlasCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            // Reserve white even when the first bitmap wraps onto a new row.
            shelf: [1, 0, 1],
            pixel_bytes: 0,
            count: 0,
            reset_pending: false,
            fresh_frame: true,
            frame_error: None,
        }
    }
}
impl AtlasCache {
    fn begin(&mut self) {
        if self.reset_pending {
            // Drop allocations as well as logical entries; metadata stays bounded.
            *self = Self::default();
        }
        self.fresh_frame = self.count == 0;
        self.frame_error = None;
    }
    fn fail<T>(&mut self, message: &'static str) -> Result<T, RenderError> {
        self.frame_error = Some(message);
        Err(RenderError::Capacity(message))
    }
    fn check_frame(&self) -> Result<(), RenderError> {
        self.frame_error
            .map_or(Ok(()), |message| Err(RenderError::Capacity(message)))
    }
    fn admit(&mut self, key: ContentKey, bytes: &[u8]) -> Result<([u32; 2], bool), RenderError> {
        self.check_frame()?;
        let ContentKey { width, height, .. } = key;
        if width == 0
            || height == 0
            || width > ATLAS
            || height > ATLAS
            || bytes.len() != width as usize * height as usize * 4
        {
            return self.fail("invalid atlas bitmap: expected bounded nonzero RGBA dimensions");
        }
        if let Some(entries) = self.entries.get(&key)
            && let Some(entry) = entries.iter().find(|entry| entry.pixels.as_ref() == bytes)
        {
            return Ok((entry.origin, false));
        }
        // Plan on a copy: a failed reservation must never move or reuse old texels.
        let mut shelf = self.shelf;
        if shelf[0] + width > ATLAS {
            shelf = [0, shelf[1] + shelf[2], 0];
        }
        if shelf[1] + height > ATLAS
            || self.count >= MAX_CACHE_ENTRIES
            || bytes.len() > MAX_CACHE_BYTES - self.pixel_bytes
        {
            self.reset_pending = true;
            return self.fail(if self.fresh_frame {
                "frame bitmap set exceeds bounded atlas capacity (including reserved white texel)"
            } else {
                "persistent atlas full; reset scheduled for next frame begin"
            });
        }
        let origin = [shelf[0], shelf[1]];
        self.entries.entry(key).or_default().push(CachedBitmap {
            pixels: bytes.into(),
            origin,
        });
        self.shelf = [shelf[0] + width, shelf[1], shelf[2].max(height)];
        self.pixel_bytes += bytes.len();
        self.count += 1;
        Ok((origin, true))
    }
}

fn vertex_capacity(required: usize, current: usize) -> Result<usize, RenderError> {
    if required > MAX_VERTICES {
        return Err(RenderError::Capacity("quad batch exceeds vertex limit"));
    }
    if required <= current {
        return Ok(current);
    }
    Ok(required.max(6).next_power_of_two().min(MAX_VERTICES))
}

/// A timed-out/failed submission may still be reading either reusable resource.
/// Keep the failure sticky instead of assuming a later frame can overwrite it.
fn require_completion(
    result: Result<(), String>,
    sticky: &mut Option<String>,
) -> Result<(), RenderError> {
    if let Err(error) = result {
        sticky.get_or_insert_with(|| {
            format!("GPU completion uncertain; renderer must be recreated: {error}")
        });
    }
    sticky
        .as_ref()
        .map_or(Ok(()), |error| Err(RenderError::Device(error.clone())))
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

pub(crate) struct Gpu {
    surface: wgpu::Surface<'static>,
    instance: wgpu::Instance,
    target: SurfaceTarget,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    vertices: Vec<Vertex>,
    vertex_buffer: Option<wgpu::Buffer>,
    vertex_capacity: usize,
    atlas: AtlasCache,
    white_initialized: bool,
    completion_failure: Option<String>,
    size: PhysicalSize,
    failure: Arc<Mutex<Option<String>>>,
    telemetry: Option<FrameTelemetry>,
}

impl Gpu {
    pub async fn new(target: SurfaceTarget, size: PhysicalSize) -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(target.clone())
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("bterm renderer"),
                ..Default::default()
            })
            .await
            .map_err(|e| RenderError::Device(e.to_string()))?;
        if size.width > device.limits().max_texture_dimension_2d
            || size.height > device.limits().max_texture_dimension_2d
            || ATLAS > device.limits().max_texture_dimension_2d
        {
            return Err(RenderError::Capacity(
                "surface or atlas exceeds device limits",
            ));
        }
        let failure = Arc::new(Mutex::new(None));
        let lost = Arc::clone(&failure);
        device.set_device_lost_callback(move |reason, message| {
            if let Ok(mut error) = lost.lock() {
                *error = Some(format!("device lost ({reason:?}): {message}"));
            }
        });
        let uncaptured = Arc::clone(&failure);
        device.on_uncaptured_error(Arc::new(move |error: wgpu::Error| {
            if let Ok(mut failure) = uncaptured.lock() {
                *failure = Some(format!("{error}"));
            }
        }));
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or_else(|| RenderError::Device("no compatible surface format".into()))?;
        config.format = surface
            .get_capabilities(&adapter)
            .formats
            .into_iter()
            .find(wgpu::TextureFormat::is_srgb)
            .ok_or_else(|| RenderError::Device("surface has no sRGB output format".into()))?;
        surface.configure(&device, &config);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("bounded frame atlas"),
            size: wgpu::Extent3d {
                width: ATLAS,
                height: ATLAS,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &texture.create_view(&Default::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(
                        &device.create_sampler(&wgpu::SamplerDescriptor::default()),
                    ),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terminal quads"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terminal cells and images"), layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[
                Some(wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Vertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4] })
            ]},
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: config.format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })] }),
            primitive: Default::default(), depth_stencil: None, multisample: Default::default(), multiview_mask: None, cache: None,
        });
        Ok(Self {
            surface,
            instance,
            target,
            device,
            queue,
            config,
            pipeline,
            texture,
            bind,
            vertices: Vec::new(),
            vertex_buffer: None,
            vertex_capacity: 0,
            atlas: AtlasCache::default(),
            white_initialized: false,
            completion_failure: None,
            size,
            failure,
            telemetry: None,
        })
    }
    fn check_failure(&self) -> Result<(), RenderError> {
        if let Some(error) = &self.completion_failure {
            return Err(RenderError::Device(error.clone()));
        }
        let failure = self
            .failure
            .lock()
            .map_err(|_| RenderError::Device("device error lock poisoned".into()))?;
        if let Some(error) = failure.as_ref() {
            return Err(RenderError::Device(error.clone()));
        }
        Ok(())
    }
    pub fn resize(&mut self, size: PhysicalSize) {
        self.size = size;
        if self.check_failure().is_err() {
            return;
        }
        if !size.is_empty()
            && size.width <= self.device.limits().max_texture_dimension_2d
            && size.height <= self.device.limits().max_texture_dimension_2d
        {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }
    pub fn begin_telemetry(&mut self, enabled: bool) {
        self.telemetry = enabled.then(FrameTelemetry::default);
    }
    pub fn frame_telemetry(&self) -> FrameTelemetry {
        self.telemetry.unwrap_or_default()
    }
    pub fn begin(&mut self) {
        self.vertices.clear();
        // Completion uncertainty forbids *all* resource reuse, including atlas reset.
        if self.check_failure().is_err() {
            return;
        }
        self.atlas.begin();
        if !self.white_initialized {
            self.write([0, 0], [1, 1], &[255; 4]);
            self.white_initialized = true;
        }
    }
    fn write(&mut self, origin: [u32; 2], size: [u32; 2], pixels: &[u8]) {
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.atlas_upload_count += 1;
            telemetry.atlas_upload_bytes += pixels.len() as u64;
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: origin[0],
                    y: origin[1],
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size[0] * 4),
                rows_per_image: Some(size[1]),
            },
            wgpu::Extent3d {
                width: size[0],
                height: size[1],
                depth_or_array_layers: 1,
            },
        );
    }
    pub(crate) fn upload_cached(
        &mut self,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<[f32; 4], RenderError> {
        self.check_failure()?;
        if width == 0
            || height == 0
            || width > ATLAS
            || height > ATLAS
            || bytes.len() != width as usize * height as usize * 4
        {
            return self
                .atlas
                .fail("invalid atlas bitmap: expected bounded nonzero RGBA dimensions");
        }
        let (origin, upload) = self
            .atlas
            .admit(ContentKey::new(width, height, bytes), bytes)?;
        let [x, y] = origin;
        if upload {
            self.write(origin, [width, height], bytes);
        }
        Ok([
            x as f32 / ATLAS as f32,
            y as f32 / ATLAS as f32,
            width as f32 / ATLAS as f32,
            height as f32 / ATLAS as f32,
        ])
    }
    pub fn quad(
        &mut self,
        rect: [f32; 4],
        uv: Option<[f32; 4]>,
        color: [f32; 4],
        clip: [f32; 4],
    ) -> Result<(), RenderError> {
        self.check_failure()?;
        self.atlas.check_frame()?;
        let [x, y, w, h] = rect;
        if w <= 0.0 || h <= 0.0 {
            return Ok(());
        }
        let left = x.max(clip[0]).max(0.0);
        let top = y.max(clip[1]).max(0.0);
        let right = (x + w).min(clip[0] + clip[2]).min(self.size.width as f32);
        let bottom = (y + h).min(clip[1] + clip[3]).min(self.size.height as f32);
        if right <= left || bottom <= top {
            return Ok(());
        }
        if self.vertices.len() + 6 > MAX_VERTICES {
            return self.atlas.fail("quad batch exhausted");
        }
        let capacity = vertex_capacity(self.vertices.len() + 6, self.vertices.capacity())?;
        if capacity > self.vertices.capacity() {
            self.vertices.reserve_exact(capacity - self.vertices.len());
            if let Some(telemetry) = &mut self.telemetry {
                telemetry.cpu_vertex_allocations += 1;
                telemetry.cpu_vertex_allocation_bytes +=
                    (capacity * std::mem::size_of::<Vertex>()) as u64;
            }
        }
        let uv = uv.unwrap_or([0.5 / ATLAS as f32, 0.5 / ATLAS as f32, 0.0, 0.0]);
        for [px, py] in [
            [left, top],
            [left, bottom],
            [right, top],
            [right, top],
            [left, bottom],
            [right, bottom],
        ] {
            self.vertices.push(Vertex {
                position: [
                    px / self.size.width as f32 * 2.0 - 1.0,
                    1.0 - py / self.size.height as f32 * 2.0,
                ],
                uv: [uv[0] + (px - x) / w * uv[2], uv[1] + (py - y) / h * uv[3]],
                color,
            });
        }
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.vertex_count += 6;
            telemetry.vertex_bytes += (6 * std::mem::size_of::<Vertex>()) as u64;
        }
        Ok(())
    }
    pub fn present(&mut self) -> Result<(), RenderError> {
        self.check_failure()?;
        self.atlas.check_frame()?;
        if self.size.width > self.device.limits().max_texture_dimension_2d
            || self.size.height > self.device.limits().max_texture_dimension_2d
        {
            return Err(RenderError::Capacity("surface exceeds device limits"));
        }
        if self.size.is_empty() {
            return Err(RenderError::Suspended);
        }
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Err(RenderError::Surface(
                    "surface outdated; reconfigured".into(),
                ));
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self
                    .instance
                    .create_surface(self.target.clone())
                    .map_err(|error| RenderError::Surface(error.to_string()))?;
                self.surface.configure(&self.device, &self.config);
                return Err(RenderError::Surface("surface lost; recreated".into()));
            }
            status => {
                return Err(RenderError::Surface(format!(
                    "surface acquisition: {status:?}"
                )));
            }
        };
        let validation_scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let capacity = vertex_capacity(self.vertices.len().max(1), self.vertex_capacity)?;
        let buffer_bytes = (capacity * std::mem::size_of::<Vertex>()) as u64;
        if buffer_bytes > self.device.limits().max_buffer_size {
            return self.atlas.fail("vertex buffer exceeds device size limit");
        }
        if capacity > self.vertex_capacity {
            self.vertex_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("reusable bounded frame vertices"),
                size: buffer_bytes,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.vertex_capacity = capacity;
            if let Some(telemetry) = &mut self.telemetry {
                telemetry.gpu_vertex_allocations += 1;
                telemetry.gpu_vertex_bytes += buffer_bytes;
            }
        }
        let buffer = self
            .vertex_buffer
            .as_ref()
            .ok_or_else(|| RenderError::Device("vertex buffer missing after allocation".into()))?;
        if !self.vertices.is_empty() {
            self.queue
                .write_buffer(buffer, 0, bytemuck::cast_slice(&self.vertices));
        }
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..self.vertices.len() as u32, 0..1);
        }
        let submission = self.queue.submit([encoder.finish()]);
        let validation = validation_scope.pop();
        let wait_start = self.telemetry.as_ref().map(|_| std::time::Instant::now());
        let waited = self.device.poll(wgpu::PollType::Wait {
            submission_index: Some(submission),
            timeout: Some(std::time::Duration::from_secs(2)),
        });
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.gpu_wait_ns = elapsed_ns(wait_start);
        }
        require_completion(
            waited.map(|_| ()).map_err(|error| error.to_string()),
            &mut self.completion_failure,
        )?;
        let validation_start = self.telemetry.as_ref().map(|_| std::time::Instant::now());
        let validation_error = pollster::block_on(validation);
        if let Some(telemetry) = &mut self.telemetry {
            telemetry.gpu_validation_ns = elapsed_ns(validation_start);
        }
        if let Some(error) = validation_error {
            self.completion_failure = Some(format!(
                "GPU validation failed; renderer must be recreated: {error}"
            ));
            self.check_failure()?;
        }
        self.check_failure()?;
        self.queue.present(frame);
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        Ok(())
    }
}
const SHADER: &str = r#"
struct Out { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) color: vec4<f32> };
@vertex fn vs(@location(0) p: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) color: vec4<f32>) -> Out {
    var o: Out; o.position=vec4<f32>(p,0.0,1.0); o.uv=uv; o.color=color; return o;
}
@group(0) @binding(0) var atlas: texture_2d<f32>;
@group(0) @binding(1) var atlas_sampler: sampler;
@fragment fn fs(i: Out) -> @location(0) vec4<f32> { return textureSample(atlas,atlas_sampler,i.uv)*i.color; }
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn put(cache: &mut AtlasCache, width: u32, height: u32, bytes: &[u8]) -> ([u32; 2], bool) {
        cache
            .admit(ContentKey::new(width, height, bytes), bytes)
            .unwrap()
    }

    #[test]
    fn content_and_dimensions_are_part_of_cache_identity() {
        let mut cache = AtlasCache::default();
        let bytes = [7; 16];
        let first = put(&mut cache, 2, 2, &bytes);
        assert!(first.1);
        cache.begin();
        assert_eq!(put(&mut cache, 2, 2, &bytes), (first.0, false));
        assert_ne!(put(&mut cache, 1, 4, &bytes).0, first.0);
        assert_ne!(put(&mut cache, 2, 2, &[8; 16]).0, first.0);
        assert_eq!(cache.count, 3);
        assert_eq!(cache.pixel_bytes, 48);
    }

    #[test]
    fn forced_hash_collisions_compare_exact_pixels() {
        let mut cache = AtlasCache::default();
        let key = ContentKey {
            width: 1,
            height: 1,
            hash: 0,
        };
        let first = cache.admit(key, &[1; 4]).unwrap();
        let second = cache.admit(key, &[2; 4]).unwrap();
        assert_ne!(first.0, second.0);
        assert_eq!(cache.admit(key, &[1; 4]).unwrap(), (first.0, false));
        assert_eq!(cache.admit(key, &[2; 4]).unwrap(), (second.0, false));
        assert_eq!(cache.count, 2);
    }

    #[test]
    fn row_wrap_never_overwrites_reserved_white_texel() {
        let mut cache = AtlasCache::default();
        let row = vec![1; ATLAS as usize * 4];
        assert_eq!(put(&mut cache, ATLAS, 1, &row).0, [0, 1]);
        assert_eq!(put(&mut cache, 1, 1, &[2; 4]).0, [0, 2]);
        cache.reset_pending = true;
        cache.begin();
        assert_eq!(put(&mut cache, 1, 1, &[3; 4]).0, [1, 0]);
    }

    #[test]
    fn pressure_fails_without_reuse_and_resets_only_at_begin() {
        let mut cache = AtlasCache::default();
        let original = put(&mut cache, 1, 1, &[1; 4]).0;
        cache.begin();
        cache.shelf = [ATLAS, ATLAS - 1, 1];
        let shelf = cache.shelf;
        assert!(
            cache
                .admit(ContentKey::new(1, 1, &[2; 4]), &[2; 4])
                .is_err()
        );
        assert!(cache.reset_pending);
        assert!(cache.check_frame().is_err());
        assert_eq!(cache.shelf, shelf);
        assert_eq!(cache.count, 1);
        assert_eq!(cache.entries.values().next().unwrap()[0].origin, original);
        // Even a hit cannot turn a failed partial frame into a successful one.
        assert!(
            cache
                .admit(ContentKey::new(1, 1, &[1; 4]), &[1; 4])
                .is_err()
        );
        cache.begin();
        assert!(cache.check_frame().is_ok());
        assert_eq!(cache.pixel_bytes, 0);
        assert_eq!(cache.count, 0);
        assert!(cache.entries.is_empty());
        assert_eq!(put(&mut cache, 1, 1, &[2; 4]).0, [1, 0]);
    }

    #[test]
    fn frame_own_set_reports_explicit_capacity_failure() {
        let mut cache = AtlasCache::default();
        let pixels = vec![0; MAX_CACHE_BYTES];
        let result = cache.admit(ContentKey::new(ATLAS, ATLAS, &pixels), &pixels);
        assert!(matches!(result, Err(RenderError::Capacity(message))
            if message.contains("frame bitmap set")));
        assert_eq!(cache.shelf, [1, 0, 1]);
        assert_eq!(cache.count, 0);
    }

    #[test]
    fn metadata_and_pixel_budgets_are_independently_bounded() {
        let mut cache = AtlasCache::default();
        for value in 0..MAX_CACHE_ENTRIES {
            let bytes = (value as u32).to_le_bytes();
            put(&mut cache, 1, 1, &bytes);
        }
        assert_eq!(cache.count, MAX_CACHE_ENTRIES);
        assert!(
            cache
                .admit(ContentKey::new(1, 1, &[255; 4]), &[255; 4])
                .is_err()
        );
        assert_eq!(cache.count, MAX_CACHE_ENTRIES);
        assert_eq!(cache.pixel_bytes, MAX_CACHE_ENTRIES * 4);
        cache.begin();
        // Model a full pixel budget independently of shelf and metadata capacity.
        cache.pixel_bytes = MAX_CACHE_BYTES;
        assert!(
            cache
                .admit(ContentKey::new(1, 1, &[1; 4]), &[1; 4])
                .is_err()
        );
        assert_eq!(cache.count, 0);
        assert_eq!(cache.pixel_bytes, MAX_CACHE_BYTES);
    }

    #[test]
    fn malformed_bitmap_poisoning_lasts_only_for_current_frame() {
        let mut cache = AtlasCache::default();
        for (width, height, bytes) in [(0, 1, 0), (ATLAS + 1, 1, 4), (1, 1, 3)] {
            let pixels = vec![0; bytes];
            assert!(
                cache
                    .admit(ContentKey::new(width, height, &pixels), &pixels)
                    .is_err()
            );
            assert!(cache.check_frame().is_err());
            assert_eq!(cache.count, 0);
            assert!(!cache.reset_pending);
            cache.begin();
        }
        assert!(cache.check_frame().is_ok());
    }

    #[test]
    fn vertex_growth_reuses_and_stays_within_hard_bound() {
        let first = vertex_capacity(6, 0).unwrap();
        assert_eq!(vertex_capacity(6, first).unwrap(), first);
        assert_eq!(vertex_capacity(0, first).unwrap(), first);
        assert!(vertex_capacity(first + 1, first).unwrap() > first);
        assert_eq!(vertex_capacity(MAX_VERTICES, first).unwrap(), MAX_VERTICES);
        assert!(vertex_capacity(MAX_VERTICES + 1, first).is_err());
    }

    #[test]
    fn uncertain_completion_is_sticky_even_after_later_success() {
        let mut sticky = None;
        assert!(require_completion(Ok(()), &mut sticky).is_ok());
        assert!(require_completion(Err("timeout".into()), &mut sticky).is_err());
        let failure = sticky.clone();
        assert!(require_completion(Ok(()), &mut sticky).is_err());
        assert_eq!(sticky, failure);
    }
}
