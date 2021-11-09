#![feature(untagged_unions, try_trait_v2)]

use anyhow::{anyhow, Context, Result};
use ash::vk::Handle;
use std::sync::Arc;
use std::{marker::PhantomData, pin::Pin};
use v4l::video::Capture;
use vulkano::buffer::CpuBufferPool;
use vulkano::{
    buffer::TypedBufferAccess,
    command_buffer::PrimaryCommandBuffer,
    command_buffer::SubpassContents,
    device::{self, physical::PhysicalDevice, Device, Queue},
    image::view::ImageView,
    image::{AttachmentImage, ImageAccess},
    instance::{Instance, InstanceExtensions, Version},
    pipeline::PipelineBindPoint,
    render_pass::Framebuffer,
    sync::GpuFuture,
    SynchronizedVulkanObject, VulkanObject,
};

#[allow(unused_imports)]
use log::info;

static APP_KEY: &str = "index_camera_passthrough_rs\0";
static APP_NAME: &str = "Camera\0";

pub struct VRSystem(*mut openvr_sys::IVRSystem);

pub struct VRCompositor<'a>(
    *mut openvr_sys::IVRCompositor,
    PhantomData<&'a openvr_sys::IVRSystem>,
);

impl<'a> VRCompositor<'a> {
    pub fn pin_mut(&self) -> Pin<&mut openvr_sys::IVRCompositor> {
        unsafe { Pin::new_unchecked(&mut *self.0) }
    }
    pub fn required_extensions<'b>(
        &self,
        pdev: PhysicalDevice,
        buf: &'b mut Vec<u8>,
    ) -> impl Iterator<Item = &'b std::ffi::CStr> {
        let bytes_needed = unsafe {
            self.pin_mut().GetVulkanDeviceExtensionsRequired(
                std::mem::transmute(pdev.internal_object().as_raw()),
                std::ptr::null_mut(),
                0,
            )
        };
        buf.reserve(bytes_needed as usize);
        unsafe {
            self.pin_mut().GetVulkanDeviceExtensionsRequired(
                std::mem::transmute(pdev.internal_object().as_raw()),
                buf.as_mut_ptr() as *mut _,
                bytes_needed,
            );
            buf.set_len(bytes_needed as usize);
        };
        let () = buf
            .iter_mut()
            .map(|item| {
                if *item == b' ' {
                    *item = b'\0';
                }
            })
            .collect();
        buf.as_slice()
            .split_inclusive(|ch| *ch == b'\0')
            .map(|slice| unsafe { std::ffi::CStr::from_bytes_with_nul_unchecked(slice) })
    }
}

impl VRSystem {
    pub fn init() -> Result<Self> {
        let mut error = openvr_sys::EVRInitError::VRInitError_None;
        let isystem_raw = unsafe {
            openvr_sys::VR_Init(
                &mut error,
                openvr_sys::EVRApplicationType::VRApplication_Overlay,
                std::ptr::null(),
            )
        };
        error.into_result()?;
        Ok(Self(isystem_raw))
    }
    pub fn overlay<'a>(&'a self) -> VROverlay<'a> {
        VROverlay(openvr_sys::VROverlay(), PhantomData)
    }
    pub fn compositor<'a>(&'a self) -> VRCompositor<'a> {
        VRCompositor(openvr_sys::VRCompositor(), PhantomData)
    }
    pub fn pin_mut(&self) -> Pin<&mut openvr_sys::IVRSystem> {
        unsafe { Pin::new_unchecked(&mut *self.0) }
    }
}

pub struct VROverlay<'a>(
    *mut openvr_sys::IVROverlay,
    PhantomData<&'a openvr_sys::IVRSystem>,
);

impl<'a> VROverlay<'a> {
    pub fn pin_mut(&self) -> Pin<&mut openvr_sys::IVROverlay> {
        unsafe { Pin::new_unchecked(&mut *self.0) }
    }
    pub fn create_overlay(&'a self, key: &'a str, name: &'a str) -> Result<VROverlayHandle<'a>> {
        if !key.contains('\0') || !name.contains('\0') {
            return Err(anyhow!("key and name must both contain a NUL byte"));
        }
        let mut overlayhandle = std::mem::MaybeUninit::<openvr_sys::VROverlayHandle_t>::uninit();
        unsafe {
            self.pin_mut().CreateOverlay(
                key.as_bytes().as_ptr() as *const _,
                name.as_bytes().as_ptr() as *const _,
                overlayhandle.as_mut_ptr(),
            )
        }
        .into_result()?;
        Ok(VROverlayHandle {
            raw: unsafe { overlayhandle.assume_init() },
            ivr_overlay: self,
            texture: None,
        })
    }
    /// Safety: could destroy an overlay that is still owned by a VROverlayHandle.
    unsafe fn destroy_overlay_raw(&self, overlay: openvr_sys::VROverlayHandle_t) -> Result<()> {
        let error = self.pin_mut().DestroyOverlay(overlay);
        if error != openvr_sys::EVROverlayError::VROverlayError_None {
            Err(anyhow!("Failed to destroy overlay {:?}", error))
        } else {
            Ok(())
        }
    }
}

struct TextureState {
    _image: Arc<dyn vulkano::image::ImageAccess>,
    _device: Arc<Device>,
    _queue: Arc<Queue>,
    _instance: Arc<Instance>,
}
pub struct VROverlayHandle<'a> {
    raw: openvr_sys::VROverlayHandle_t,
    ivr_overlay: &'a VROverlay<'a>,

    /// Used to hold references to vulkan objects so they don't die.
    texture: Option<TextureState>,
}

impl<'a> VROverlayHandle<'a> {
    pub fn as_raw(&self) -> openvr_sys::VROverlayHandle_t {
        self.raw
    }
    pub fn set_texture(
        &mut self,
        w: u32,
        h: u32,
        image: Arc<impl vulkano::image::ImageAccess + 'static>,
        dev: Arc<Device>,
        queue: Arc<Queue>,
        instance: Arc<Instance>,
    ) -> Result<(), openvr_sys::EVROverlayError> {
        let texture = TextureState {
            _image: image.clone() as Arc<_>,
            _device: dev.clone(),
            _queue: queue.clone(),
            _instance: instance.clone(),
        };
        self.texture.replace(texture);
        let mut vrimage = openvr_sys::VRVulkanTextureData_t {
            m_nWidth: w,
            m_nHeight: h,
            m_nFormat: image.format() as u32,
            m_nSampleCount: image.samples() as u32,
            m_nImage: image.inner().image.internal_object().as_raw(),
            m_pPhysicalDevice: unsafe {
                std::mem::transmute(dev.physical_device().internal_object().as_raw())
            },
            m_pDevice: unsafe { std::mem::transmute(dev.internal_object().as_raw()) },
            m_pQueue: unsafe { std::mem::transmute(queue.internal_object_guard().as_raw()) },
            m_pInstance: unsafe { std::mem::transmute(instance.internal_object().as_raw()) },
            m_nQueueFamilyIndex: queue.family().id(),
        };
        let vrtexture = openvr_sys::Texture_t {
            handle: &mut vrimage as *mut _ as *mut std::ffi::c_void,
            eType: openvr_sys::ETextureType::TextureType_Vulkan,
            eColorSpace: openvr_sys::EColorSpace::ColorSpace_Auto,
        };
        unsafe {
            self.ivr_overlay
                .pin_mut()
                .SetOverlayTexture(self.as_raw(), &vrtexture)
                .into_result()
        }
    }
}

impl<'a> Drop for VROverlayHandle<'a> {
    fn drop(&mut self) {
        if let Err(e) = unsafe { self.ivr_overlay.destroy_overlay_raw(self.raw) } {
            eprintln!("{}", e.to_string());
        }
    }
}

impl Drop for VRSystem {
    fn drop(&mut self) {
        openvr_sys::VR_Shutdown();
    }
}

fn find_index_camera() -> Result<std::path::PathBuf> {
    let mut it = udev::Enumerator::new()?;
    it.match_subsystem("video4linux")?;
    it.match_property("ID_VENDOR_ID", "28de")?;
    it.match_property("ID_MODEL_ID", "2400")?;

    let dev = it
        .scan_devices()?
        .next()
        .with_context(|| anyhow!("Index camera not found"))?;
    let devnode = dev
        .devnode()
        .with_context(|| anyhow!("Index camera cannot be accessed"))?;
    Ok(devnode.to_owned())
}

#[derive(Default, Debug, Clone)]
struct Vertex {
    position: [f32; 2],
}
vulkano::impl_vertex!(Vertex, position);

#[derive(thiserror::Error, Debug)]
enum ConverterError {
    #[error("something went wrong: {0}")]
    Anyhow(#[from] anyhow::Error),
    #[error("{0}")]
    VkOom(#[from] vulkano::OomError),
    #[error("{0}")]
    GraphicsPipelineCreationError(#[from] vulkano::pipeline::GraphicsPipelineCreationError),
    #[error("{0}")]
    ImageCreationError(#[from] vulkano::image::ImageCreationError),
    #[error("{0}")]
    ImageViewCreationError(#[from] vulkano::image::view::ImageViewCreationError),
    #[error("{0}")]
    DescriptorSetError(#[from] vulkano::descriptor_set::DescriptorSetError),
    #[error("{0}")]
    CopyBufferImageError(#[from] vulkano::command_buffer::CopyBufferImageError),
    #[error("{0}")]
    FramebufferCreationError(#[from] vulkano::render_pass::FramebufferCreationError),
}

struct GpuYuyvConverter {
    device: Arc<Device>,
    render_pass: Arc<vulkano::render_pass::RenderPass>,
    pipeline: Arc<vulkano::pipeline::GraphicsPipeline>,
    src: Arc<AttachmentImage>,
    desc_set: Arc<vulkano::descriptor_set::persistent::PersistentDescriptorSet>,
    w: u32,
    h: u32,
}

impl GpuYuyvConverter {
    fn new(device: Arc<Device>, w: u32, h: u32) -> Result<Self, ConverterError> {
        if w % 2 != 0 {
            return Err(ConverterError::Anyhow(anyhow!("Width can't be odd")));
        }
        let vs = vs::Shader::load(device.clone())?;
        let fs = fs::Shader::load(device.clone())?;
        let render_pass = Arc::new(
            vulkano::single_pass_renderpass!(device.clone(),
                attachments: {
                    color: {
                        load: DontCare,
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
            vulkano::pipeline::GraphicsPipeline::start()
                .vertex_input_single_buffer::<Vertex>()
                .vertex_shader(vs.main_entry_point(), ())
                .triangle_strip()
                .viewports([vulkano::pipeline::viewport::Viewport {
                    origin: [0.0, 0.0],
                    dimensions: [w as f32, h as f32],
                    depth_range: -1.0..1.0,
                }])
                .fragment_shader(fs.main_entry_point(), ())
                .render_pass(vulkano::render_pass::Subpass::from(render_pass.clone(), 0).unwrap())
                .build(device.clone())?,
        );
        let src = AttachmentImage::with_usage(
            device.clone(),
            [w / 2, h], // 1 pixel of YUYV = 2 pixels of RGB
            vulkano::format::Format::R8G8B8A8_UNORM,
            vulkano::image::ImageUsage {
                transfer_source: false,
                transfer_destination: true,
                sampled: true,
                storage: false,
                color_attachment: true,
                depth_stencil_attachment: false,
                transient_attachment: false,
                input_attachment: false,
            },
        )?;
        let desc_set_layout = pipeline.layout().descriptor_set_layouts().get(0).unwrap();
        let mut desc_set_builder =
            vulkano::descriptor_set::persistent::PersistentDescriptorSet::start(
                desc_set_layout.clone(),
            );
        use vulkano::sampler::{Filter, MipmapMode, Sampler, SamplerAddressMode};
        let sampler = Sampler::new(
            device.clone(),
            Filter::Linear,
            Filter::Linear,
            MipmapMode::Nearest,
            SamplerAddressMode::Repeat,
            SamplerAddressMode::Repeat,
            SamplerAddressMode::Repeat,
            0.0,
            1.0,
            0.0,
            0.0,
        )
        .unwrap();
        desc_set_builder.add_sampled_image(vulkano::image::view::ImageView::new(src.clone())?, sampler)?;
        let desc_set = Arc::new(desc_set_builder.build()?);
        Ok(Self {
            src,
            render_pass,
            pipeline,
            device,
            w,
            h,
            desc_set,
        })
    }
    /// receives a buffer containing a YUYV image, upload it to GPU,
    /// and convert it to RGBA8.
    ///
    /// Returns a GPU future representing the operation, and an image.
    /// You must make sure the previous conversion is completed before
    /// calling this function again.
    fn yuyv_buffer_to_vulkan_image(
        &self,
        buf: &[u8],
        queue: Arc<Queue>,
        buffer: &vulkano::buffer::CpuBufferPool<u8>,
    ) -> Result<(impl GpuFuture, Arc<AttachmentImage>), ConverterError> {
        use vulkano::device::DeviceOwned;
        if queue.device() != &self.device || buffer.device() != &self.device {
            return Err(ConverterError::Anyhow(anyhow!("Device mismatch")));
        }
        // Submit the source image to GPU
        let subbuffer = buffer
            .chunk(buf.iter().map(|x| *x))
            .map_err(|e| ConverterError::Anyhow(e.into()))?;
        let mut cmdbuf = vulkano::command_buffer::AutoCommandBufferBuilder::primary(
            self.device.clone(),
            queue.family(),
            vulkano::command_buffer::CommandBufferUsage::OneTimeSubmit,
        )?;
        cmdbuf.copy_buffer_to_image(subbuffer, self.src.clone())?;
        // Build a pipeline to do yuyv -> rgb
        let dst = AttachmentImage::with_usage(
            self.device.clone(),
            [self.w, self.h],
            vulkano::format::Format::R8G8B8A8_UNORM,
            vulkano::image::ImageUsage {
                transfer_source: true,
                transfer_destination: false,
                sampled: true,
                storage: false,
                color_attachment: true,
                depth_stencil_attachment: false,
                transient_attachment: false,
                input_attachment: false,
            },
        )?;
        let vertex_buffer = vulkano::buffer::CpuAccessibleBuffer::<[Vertex]>::from_iter(
            self.device.clone(),
            vulkano::buffer::BufferUsage::vertex_buffer(),
            false,
            [
                Vertex {
                    position: [-1.0, -1.0],
                },
                Vertex {
                    position: [-1.0, 1.0],
                },
                Vertex {
                    position: [1.0, -1.0],
                },
                Vertex {
                    position: [1.0, 1.0],
                },
            ]
            .iter()
            .cloned(),
        )
        .unwrap();
        let framebuffer = Arc::new(Framebuffer::start(self.render_pass.clone())
            .add(ImageView::new(dst.clone())?)?
            .build()?);
        cmdbuf
            .begin_render_pass(
                framebuffer.clone(),
                SubpassContents::Inline,
                [vulkano::format::ClearValue::None],
            )
            .map_err(|e| ConverterError::Anyhow(e.into()))?
            .bind_pipeline_graphics(self.pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.pipeline.layout().clone(),
                0,
                self.desc_set.clone(),
            )
            .bind_vertex_buffers(0, vertex_buffer.clone())
            .draw(vertex_buffer.len() as u32, 1, 0, 0)
            .map_err(|e| ConverterError::Anyhow(e.into()))?
            .end_render_pass()
            .map_err(|e| ConverterError::Anyhow(e.into()))?;
        Ok((
            cmdbuf
                .build()
                .map_err(|e| ConverterError::Anyhow(e.into()))?
                .execute(queue.clone())
                .map_err(|e| ConverterError::Anyhow(e.into()))?,
            dst,
        ))
    }
}

fn main() -> Result<()> {
    env_logger::init();
    let mut rd = renderdoc::RenderDoc::<renderdoc::V100>::new()?;
    let camera = v4l::Device::with_path(find_index_camera()?)?;
    if !camera
        .query_caps()?
        .capabilities
        .contains(v4l::capability::Flags::VIDEO_CAPTURE)
    {
        return Err(anyhow!("Cannot capture from index camera"));
    }
    camera.set_format(&v4l::Format::new(1920, 960, v4l::FourCC::new(b"YUYV")))?;
    camera.set_params(&v4l::video::capture::Parameters::with_fps(54))?;
    // FIXME proper buffer count
    let mut video_stream =
        v4l::prelude::MmapStream::with_buffers(&camera, v4l::buffer::Type::VideoCapture, 1)?;

    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, std::sync::atomic::Ordering::Relaxed);
    })
    .expect("Error setting Ctrl-C handler");

    // Create vulkan instance, and setup openvr.
    // Then create a vulkan device based on openvr's requirements
    let instance = Instance::new(
        None,
        Version::V1_1,
        &InstanceExtensions::supported_by_core()?,
        None,
    )?;
    let vrsys = VRSystem::init()?;
    println!("{}", openvr_sys::VR_IsHmdPresent());
    let mut target_device = 0u64;
    unsafe {
        vrsys.pin_mut().GetOutputDevice(
            &mut target_device,
            openvr_sys::ETextureType::TextureType_Vulkan,
            std::mem::transmute(instance.internal_object().as_raw()),
        )
    };

    let target_device = ash::vk::PhysicalDevice::from_raw(target_device);
    let device = PhysicalDevice::enumerate(&instance)
        .find(|physical_device| {
            if physical_device.internal_object() == target_device {
                println!(
                    "Found matching device: {}",
                    physical_device.properties().device_name
                );
                true
            } else {
                false
            }
        })
        .with_context(|| anyhow!("Cannot find the device openvr asked for"))?;
    let queue_family = device
        .queue_families()
        .find(|qf| {
            qf.supports_graphics() && qf.supports_stage(vulkano::sync::PipelineStage::AllGraphics)
        })
        .with_context(|| anyhow!("Cannot create a suitable queue"))?;
    let (device, mut queues) = {
        let mut buf = Vec::new();
        let extensions = device::DeviceExtensions::from(
            vrsys.compositor().required_extensions(device, &mut buf),
        )
        .union(&device.required_extensions());
        device::Device::new(
            device,
            &device::Features::none(),
            &extensions,
            [(queue_family, 1.0)],
        )?
    };
    let queue = queues.next().unwrap();
    let buffer = CpuBufferPool::upload(device.clone());
    let converter = GpuYuyvConverter::new(device.clone(), 1920, 960)?;
    rd.start_frame_capture(std::ptr::null(), std::ptr::null());
    let (frame, _metadata) = v4l::io::traits::CaptureStream::next(&mut video_stream)?;
    let (future, image) = converter.yuyv_buffer_to_vulkan_image(frame, queue.clone(), &buffer)?;
    future.then_signal_fence().wait(None)?;
    rd.end_frame_capture(std::ptr::null(), std::ptr::null());

    // Create a VROverlay and submit our image as its texture
    let vroverlay = vrsys.overlay();
    let mut overlay = vroverlay.create_overlay(APP_KEY, APP_NAME)?;
    vroverlay
        .pin_mut()
        .SetOverlayFlag(
            overlay.as_raw(),
            openvr_sys::VROverlayFlags::VROverlayFlags_SideBySide_Parallel,
            true,
        )
        .into_result()?;
    overlay.set_texture(1920, 960, image, device.clone(), queue.clone(), instance.clone())?;
    let transformation = openvr_sys::HmdMatrix34_t {
        m: [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, -1.0],
        ],
    };
    unsafe {
        vroverlay.pin_mut().SetOverlayTransformAbsolute(
            overlay.as_raw(),
            openvr_sys::ETrackingUniverseOrigin::TrackingUniverseStanding,
            &transformation,
        )
    };

    // Display the overlay
    vroverlay
        .pin_mut()
        .ShowOverlay(overlay.as_raw())
        .into_result()?;

    let mut event = std::mem::MaybeUninit::<openvr_sys::VREvent_t>::uninit();
    'main_loop: loop {
        while unsafe {
            vrsys.pin_mut().PollNextEvent(
                event.as_mut_ptr() as *mut _,
                std::mem::size_of::<openvr_sys::VREvent_t>() as u32,
            )
        } {
            let event = unsafe { event.assume_init_ref() };
            println!("{:?}", unsafe {
                std::mem::transmute::<_, openvr_sys::EVREventType>(event.eventType)
            });
            if event.eventType == openvr_sys::EVREventType::VREvent_ButtonPress as u32 {
                println!("{:?}", unsafe { event.data.controller.button });
                if unsafe { event.data.controller.button == 33 } {
                    break 'main_loop;
                }
            } else if event.eventType == openvr_sys::EVREventType::VREvent_Quit as u32 {
                vrsys.pin_mut().AcknowledgeQuit_Exiting();
                break 'main_loop;
            }
        }
        if !running.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        path: "shaders/trivial.vert",
    }
}

mod fs {
    vulkano_shaders::shader! {
        ty: "fragment",
        path: "shaders/yuyv2rgb.frag",
    }
}
