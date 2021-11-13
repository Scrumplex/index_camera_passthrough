//! The overlay in VR space can be seen as a "portal" to the real world. By projecting it to the
//! Index camera's clipping space, using the camera's projection matrix, we can decide which
//! portion of the camera's view can be seen through this portal.
//!
//! Overlay vertex * Overlay Model * HMD View * Camera Project -> Texture coordinates used to
//! sample the camera's view.
//!
//! Overlay vertex: calculate based on Overlay width we set
//! Overlay Model: the overlay transform matrix we set
//! HMD View: inverse of HMD pose
//! Camera Project: estimated from camera calibration.
use anyhow::{anyhow, Result};
use std::sync::Arc;
use vulkano::{
    buffer::{BufferUsage, CpuAccessibleBuffer, TypedBufferAccess},
    command_buffer::{
        AutoCommandBufferBuilder, CommandBufferUsage::OneTimeSubmit, SubpassContents,
    },
    descriptor_set::single_layout_pool::SingleLayoutDescSetPool,
    device::{Device, Queue},
    image::{view::ImageView, AttachmentImage},
    pipeline::{viewport::Viewport, GraphicsPipeline, PipelineBindPoint},
    render_pass::{Framebuffer, RenderPass, Subpass},
    sampler::{Filter, MipmapMode, Sampler, SamplerAddressMode},
    sync::GpuFuture,
};
mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "shaders/trivial.vert",
    }
}

mod fs {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "shaders/projection.frag",
    }
}

/// Because your eye and the camera is at different physical locations, it is impossible
/// to project camera view into VR space perfectly. There are trade offs approximating
/// this projection.
///
/// In FromCamera mode, we assume your eyes are at the cameras' physical location. This mode
/// has larger viewing range, but everything will _seem_ smaller to you.
///
/// In FromEye mode, we assume your cameras are at your eyes' physical location. Everything will
/// have right scale in this mode, but the viewing range is smaller.
#[derive(Eq, PartialEq, Debug)]
pub enum ProjectionMode {
    FromCamera,
    FromEye,
}

pub struct Projection {
    device: Arc<Device>,
    source: Arc<AttachmentImage>,
    pipeline: Arc<GraphicsPipeline>,
    render_pass: Arc<RenderPass>,
    mode: ProjectionMode,
}

#[derive(Default, Debug, Clone)]
struct Vertex {
    position: [f32; 2],
    in_tex_coord: [f32; 3],
}
vulkano::impl_vertex!(Vertex, position, in_tex_coord);

#[allow(dead_code)]
fn format_matrix<
    A: Scalar + ToString,
    B: nalgebra::Dim,
    C: nalgebra::Dim,
    D: RawStorage<A, B, C>,
>(
    m: &nalgebra::Matrix<A, B, C, D>,
) -> String {
    use itertools::Itertools;
    format!(
        "numpy.matrix([{}])",
        m.row_iter()
            .map(|r| {
                let it: MatrixIter<_, _, _, _> = r.into_iter();
                format!("[{}]", it.map(|v| v.to_string()).join(","))
            })
            .join(",")
    )
}

use nalgebra::{iter::MatrixIter, matrix, Matrix4, RawStorage, Scalar};
impl Projection {
    /// Calculate the _physical_ camera's MVP, for each eye.
    /// camera_fov = focal length / sensor width.
    /// frame_time = how long after the first frame is the current frame taken
    /// time_origin = instant when the first frame is taken
    pub fn calculate_mvp(
        &self,
        overlay_transform: &Matrix4<f32>,
        camera_fov: f32,
        ivrsystem: &crate::openvr::VRSystem,
        frame_time: std::time::Duration,
        time_origin: std::time::Instant,
    ) -> (Matrix4<f32>, Matrix4<f32>) {
        let mut hmd_transform = std::mem::MaybeUninit::<openvr_sys::TrackedDevicePose_t>::uninit();
        // We try to get the pose at the time when the camera frame is captured. GetDeviceToAbsoluteTrackingPose
        // doesn't specifically say if a negative time offset will work...
        let elapsed = std::time::Instant::now() - time_origin - frame_time;
        let hmd_transform = unsafe {
            ivrsystem.pin_mut().GetDeviceToAbsoluteTrackingPose(
                openvr_sys::ETrackingUniverseOrigin::TrackingUniverseStanding,
                -elapsed.as_secs_f32(),
                hmd_transform.as_mut_ptr(),
                1,
            );
            hmd_transform.assume_init()
        };
        let transform: Matrix4<_> = hmd_transform.mDeviceToAbsoluteTracking.into();
        let left_eye: Matrix4<_> = ivrsystem
            .pin_mut()
            .GetEyeToHeadTransform(openvr_sys::EVREye::Eye_Left)
            .into();
        let right_eye: Matrix4<_> = ivrsystem
            .pin_mut()
            .GetEyeToHeadTransform(openvr_sys::EVREye::Eye_Right)
            .into();

        // Camera space to HMD space transform, based on physical measurements
        let left_cam: Matrix4<_> = matrix![
            1.0, 0.0, 0.0, -0.067;
            0.0, 1.0, 0.0, -0.039;
            0.0, 0.0, 1.0, -0.07;
            0.0, 0.0, 0.0, 1.0;
        ];
        let right_cam: Matrix4<_> = matrix![
            1.0, 0.0, 0.0, 0.067;
            0.0, 1.0, 0.0, -0.039;
            0.0, 0.0, 1.0, -0.07;
            0.0, 0.0, 0.0, 1.0;
        ];

        let (left_eye, right_eye) = match self.mode {
            ProjectionMode::FromEye => (transform * left_eye, transform * right_eye),
            ProjectionMode::FromCamera => (transform * left_cam, transform * right_cam),
        };
        let left_view = left_eye
            .try_inverse()
            .expect("HMD transform not invertable?");
        let right_view = right_eye
            .try_inverse()
            .expect("HMD transform not invertable?");

        // X gets camera_fov / 2.0 because the source texture is a side-by-side stereo texture
        // X translation element is used to map them to left/right side of the texture,
        // respectively.
        //
        // For debug only： Y should be negative because vulkan clip space has Y+ downward, while
        // texture sampling has Y+ upward.
        let camera_projection = matrix![
            camera_fov / 2.0, 0.0, 0.0, 0.0;
            0.0, camera_fov, 0.0, 0.0;
            0.0, 0.0, -1.0, 0.0;
            0.0, 0.0, 0.0, 1.0;
        ];
        (
            camera_projection * left_view * overlay_transform,
            camera_projection * right_view * overlay_transform,
        )
    }
    pub fn new(device: Arc<Device>, source: Arc<AttachmentImage>, mode: ProjectionMode) -> Result<Self> {
        let [w, h, _] = source.dimensions();
        if w != h * 2 {
            return Err(anyhow!("Input not square"));
        }
        let vs = vs::Shader::load(device.clone())?;
        let fs = fs::Shader::load(device.clone())?;
        let render_pass = Arc::new(
            vulkano::single_pass_renderpass!(device.clone(),
                attachments: {
                    color: {
                        load: Load,
                        store: Store,
                        format: vulkano::format::Format::R8G8B8A8_UNORM,
                        samples: 1,
                    }
                },
                pass: {
                    color: [color],
                    depth_stencil: {}
                }
            )
            .unwrap(),
        );
        let pipeline = Arc::new(
            GraphicsPipeline::start()
                .vertex_input_single_buffer::<Vertex>()
                .vertex_shader(vs.main_entry_point(), ())
                .triangle_strip()
                .viewports_dynamic_scissors_irrelevant(1)
                .fragment_shader(fs.main_entry_point(), ())
                .render_pass(Subpass::from(render_pass.clone(), 0).unwrap())
                .build(device.clone())?,
        );
        Ok(Self {
            device,
            render_pass,
            pipeline,
            source,
            mode
        })
    }
    pub fn project(
        &self,
        after: impl GpuFuture,
        queue: Arc<Queue>,
        output: Arc<AttachmentImage>,
        overlay_width: f32,
        ipd: f32,
        (left, right): (&Matrix4<f32>, &Matrix4<f32>),
    ) -> Result<impl GpuFuture> {
        let framebuffer = Arc::new(
            Framebuffer::start(self.render_pass.clone())
                .add(ImageView::new(output.clone())?)?
                .build()?,
        );
        let [w, h, _] = self.source.dimensions();
        let mut desc_set_pool = SingleLayoutDescSetPool::new(
            self.pipeline
                .layout()
                .descriptor_set_layouts()
                .get(0)
                .unwrap()
                .clone(),
        );
        let mut cmdbuf =
            AutoCommandBufferBuilder::primary(self.device.clone(), queue.family(), OneTimeSubmit)?;
        cmdbuf.copy_image(
            self.source.clone(),
            [0, 0, 0],
            0,
            0,
            output.clone(),
            [0, 0, 0],
            0,
            0,
            [w, h, 1],
            1,
        )?;

        let sampler = Sampler::new(
            self.device.clone(),
            Filter::Linear,
            Filter::Linear,
            MipmapMode::Nearest,
            SamplerAddressMode::ClampToEdge,
            SamplerAddressMode::ClampToEdge,
            SamplerAddressMode::ClampToEdge,
            0.0,
            1.0,
            0.0,
            0.0,
        )?;
        // Y is flipped from the vertex Y because texture coordinate is top-down
        let vertex_buffer = CpuAccessibleBuffer::<[Vertex]>::from_iter(
            self.device.clone(),
            BufferUsage::vertex_buffer(),
            false,
            [
                Vertex {
                    position: [-1.0, -1.0],
                    in_tex_coord: [-overlay_width / 2.0, overlay_width / 2.0, 0.0],
                },
                Vertex {
                    position: [-1.0, 1.0],
                    in_tex_coord: [-overlay_width / 2.0, -overlay_width / 2.0, 0.0],
                },
                Vertex {
                    position: [1.0, -1.0],
                    in_tex_coord: [overlay_width / 2.0, overlay_width / 2.0, 0.0],
                },
                Vertex {
                    position: [1.0, 1.0],
                    in_tex_coord: [overlay_width / 2.0, -overlay_width / 2.0, 0.0],
                },
            ]
            .iter()
            .cloned(),
        )
        .unwrap();

        let eye_offset = if self.mode == ProjectionMode::FromEye { 0.067 - ipd / 2.0 } else { 0.0 };
        // Left
        let uniform = fs::ty::Info {
            mvp: left.as_ref().clone(),
            texOffset: [0.0, 0.0],
            overlayWidth: overlay_width,
            windowSize: [(w / 2) as f32, h as f32],
            eyeOffset: eye_offset,
        };
        let uniform = CpuAccessibleBuffer::from_data(
            self.device.clone(),
            BufferUsage {
                uniform_buffer: true,
                ..BufferUsage::none()
            },
            false,
            uniform,
        )?;
        let mut desc_set_builder = desc_set_pool.next();
        desc_set_builder
            .add_buffer(uniform)?
            .add_sampled_image(ImageView::new(self.source.clone())?, sampler.clone())?;
        let desc_set = Arc::new(desc_set_builder.build()?);

        cmdbuf
            .begin_render_pass(
                framebuffer.clone(),
                SubpassContents::Inline,
                [vulkano::format::ClearValue::None],
            )?
            .set_viewport(
                0,
                [Viewport {
                    origin: [0.0, 0.0],
                    dimensions: [(w / 2) as f32, h as f32],
                    depth_range: -1.0..1.0,
                }],
            )
            .bind_pipeline_graphics(self.pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.pipeline.layout().clone(),
                0,
                desc_set,
            )
            .bind_vertex_buffers(0, vertex_buffer.clone())
            .draw(vertex_buffer.len() as u32, 1, 0, 0)?
            .end_render_pass()?;

        // Right
        let uniform = fs::ty::Info {
            mvp: right.as_ref().clone(),
            texOffset: [0.5, 0.0],
            overlayWidth: overlay_width,
            windowSize: [(w / 2) as f32, h as f32],
            eyeOffset: eye_offset,
        };
        let uniform = CpuAccessibleBuffer::from_data(
            self.device.clone(),
            BufferUsage {
                uniform_buffer: true,
                ..BufferUsage::none()
            },
            false,
            uniform,
        )?;
        let mut desc_set_builder = desc_set_pool.next();
        desc_set_builder
            .add_buffer(uniform)?
            .add_sampled_image(ImageView::new(self.source.clone())?, sampler.clone())?;
        let desc_set = Arc::new(desc_set_builder.build()?);

        cmdbuf
            .begin_render_pass(
                framebuffer.clone(),
                SubpassContents::Inline,
                [vulkano::format::ClearValue::None],
            )?
            .set_viewport(
                0,
                [Viewport {
                    origin: [(w / 2) as f32, 0.0],
                    dimensions: [(w / 2) as f32, h as f32],
                    depth_range: -1.0..1.0,
                }],
            )
            .bind_pipeline_graphics(self.pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.pipeline.layout().clone(),
                0,
                desc_set,
            )
            .bind_vertex_buffers(0, vertex_buffer.clone())
            .draw(vertex_buffer.len() as u32, 1, 0, 0)?
            .end_render_pass()?;
        Ok(after.then_execute(queue, cmdbuf.build()?)?)
    }
}