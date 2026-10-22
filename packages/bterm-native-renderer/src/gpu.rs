//! Private bounded texture upload and a single alpha-blended quad batch.
use crate::renderer::RenderError;
use bterm_native_platform::{PhysicalSize, SurfaceTarget};
use std::sync::{Arc, Mutex};
use wgpu::util::DeviceExt;

const ATLAS: u32 = 4096;
const MAX_VERTICES: usize = 4_000_000;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

pub(crate) struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    texture: wgpu::Texture,
    bind: wgpu::BindGroup,
    vertices: Vec<Vertex>,
    shelf: [u32; 3],
    size: PhysicalSize,
    failure: Arc<Mutex<Option<String>>>,
}

impl Gpu {
    pub async fn new(target: SurfaceTarget, size: PhysicalSize) -> Result<Self, RenderError> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance
            .create_surface(target)
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
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("terminal cells and images"), layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[
                wgpu::VertexBufferLayout { array_stride: std::mem::size_of::<Vertex>() as u64, step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4] }
            ]},
            fragment: Some(wgpu::FragmentState { module: &shader, entry_point: Some("fs"), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: config.format, blend: Some(wgpu::BlendState::ALPHA_BLENDING), write_mask: wgpu::ColorWrites::ALL })] }),
            primitive: Default::default(), depth_stencil: None, multisample: Default::default(), multiview: None, cache: None,
        });
        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            texture,
            bind,
            vertices: Vec::new(),
            shelf: [1, 0, 1],
            size,
            failure,
        })
    }
    fn check_failure(&self) -> Result<(), RenderError> {
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
        if !size.is_empty()
            && size.width <= self.device.limits().max_texture_dimension_2d
            && size.height <= self.device.limits().max_texture_dimension_2d
        {
            self.config.width = size.width;
            self.config.height = size.height;
            self.surface.configure(&self.device, &self.config);
        }
    }
    pub fn begin(&mut self) {
        self.vertices.clear();
        self.shelf = [1, 0, 1];
        self.write([0, 0], [1, 1], &[255; 4]);
    }
    fn write(&self, origin: [u32; 2], size: [u32; 2], pixels: &[u8]) {
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
    pub fn upload(
        &mut self,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<[f32; 4], RenderError> {
        if width == 0
            || height == 0
            || width > ATLAS
            || height > ATLAS
            || bytes.len() != width as usize * height as usize * 4
        {
            return Err(RenderError::Capacity("invalid atlas bitmap"));
        }
        if self.shelf[0] + width > ATLAS {
            self.shelf[0] = 0;
            self.shelf[1] += self.shelf[2];
            self.shelf[2] = 0;
        }
        if self.shelf[1] + height > ATLAS {
            return Err(RenderError::Capacity("frame texture atlas exhausted"));
        }
        let [x, y, _] = self.shelf;
        self.write([x, y], [width, height], bytes);
        self.shelf[0] += width;
        self.shelf[2] = self.shelf[2].max(height);
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
            return Err(RenderError::Capacity("quad batch exhausted"));
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
        Ok(())
    }
    pub fn present(&mut self) -> Result<(), RenderError> {
        self.check_failure()?;
        if self.size.width > self.device.limits().max_texture_dimension_2d
            || self.size.height > self.device.limits().max_texture_dimension_2d
        {
            return Err(RenderError::Capacity("surface exceeds device limits"));
        }
        if self.size.is_empty() {
            return Err(RenderError::Suspended);
        }
        let frame = self.surface.get_current_texture().map_err(|error| {
            if matches!(
                error,
                wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated
            ) {
                self.surface.configure(&self.device, &self.config);
            }
            RenderError::Surface(error.to_string())
        })?;
        self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("bounded frame vertices"),
                contents: bytemuck::cast_slice(&self.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
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
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..));
            pass.draw(0..self.vertices.len() as u32, 0..1);
        }
        let submission = self.queue.submit([encoder.finish()]);
        let validation = self.device.pop_error_scope();
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: Some(std::time::Duration::from_secs(2)),
            })
            .map_err(|error| RenderError::Device(error.to_string()))?;
        if let Some(error) = pollster::block_on(validation) {
            return Err(RenderError::Device(error.to_string()));
        }
        self.check_failure()?;
        frame.present();
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
