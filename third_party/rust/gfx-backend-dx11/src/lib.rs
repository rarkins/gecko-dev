/*!
# DX11 backend internals.

## Pipeline Layout

In D3D11 there are tables of CBVs, SRVs, UAVs, and samplers.

Each descriptor type can take 1 or two of those entry points.

The descriptor pool is just and array of handles, belonging to descriptor set 1, descriptor set 2, etc.
Each range of descriptors in a descriptor set area of the pool is split into shader stages,
which in turn is split into CBS/SRV/UAV/Sampler parts. That allows binding a descriptor set as a list
of continuous descriptor ranges (per type, per shader stage).

!*/

//#[deny(missing_docs)]

#[macro_use]
extern crate bitflags;
#[macro_use]
extern crate log;

use auxil::ShaderStage;
use hal::{
    adapter, buffer, command, format, image, memory, pass, pso, query, queue, window, DrawCount,
    IndexCount, InstanceCount, Limits, TaskCount, VertexCount, VertexOffset, WorkGroupCount,
};
use range_alloc::RangeAllocator;
use crate::{
    device::DepthStencilState,
    debug::set_debug_name,
};

use winapi::{shared::{
    dxgi::{IDXGIAdapter, IDXGIFactory, IDXGISwapChain},
    dxgiformat,
    minwindef::{FALSE, HMODULE, UINT},
    windef::{HWND, RECT},
    winerror,
}, um::{d3d11, d3d11_1, d3dcommon, winuser::GetClientRect}, Interface as _};

use wio::com::ComPtr;

use arrayvec::ArrayVec;
use parking_lot::{Condvar, Mutex, RwLock};

use std::{borrow::Borrow, cell::RefCell, fmt, mem, ops::Range, os::raw::c_void, ptr, sync::{Arc, Weak}};

macro_rules! debug_scope {
    ($context:expr, $($arg:tt)+) => ({
        #[cfg(debug_assertions)]
        {
            $crate::debug::DebugScope::with_name(
                $context,
                format_args!($($arg)+),
            )
        }
        #[cfg(not(debug_assertions))]
        {
            ()
        }
    });
}

macro_rules! debug_marker {
    ($context:expr, $($arg:tt)+) => ({
        #[cfg(debug_assertions)]
        {
            $crate::debug::debug_marker(
                $context,
                &format!($($arg)+),
            );
        }
    });
}

mod conv;
mod debug;
mod device;
mod dxgi;
mod internal;
mod shader;

type CreateFun = unsafe extern "system" fn(
    *mut IDXGIAdapter,
    UINT,
    HMODULE,
    UINT,
    *const UINT,
    UINT,
    UINT,
    *mut *mut d3d11::ID3D11Device,
    *mut UINT,
    *mut *mut d3d11::ID3D11DeviceContext,
) -> winerror::HRESULT;

#[derive(Clone)]
pub(crate) struct ViewInfo {
    resource: *mut d3d11::ID3D11Resource,
    kind: image::Kind,
    caps: image::ViewCapabilities,
    view_kind: image::ViewKind,
    format: dxgiformat::DXGI_FORMAT,
    levels: Range<image::Level>,
    layers: Range<image::Layer>,
}

impl fmt::Debug for ViewInfo {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("ViewInfo")
    }
}

#[derive(Debug)]
pub struct Instance {
    pub(crate) factory: ComPtr<IDXGIFactory>,
    pub(crate) dxgi_version: dxgi::DxgiVersion,
    library_d3d11: Arc<libloading::Library>,
    library_dxgi: libloading::Library,
}

unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl Instance {
    pub fn create_surface_from_hwnd(&self, hwnd: *mut c_void) -> Surface {
        Surface {
            factory: self.factory.clone(),
            wnd_handle: hwnd as *mut _,
            presentation: None,
        }
    }
}

fn get_features(
    _device: ComPtr<d3d11::ID3D11Device>,
    feature_level: d3dcommon::D3D_FEATURE_LEVEL,
) -> hal::Features {
    let mut features = hal::Features::empty()
        | hal::Features::ROBUST_BUFFER_ACCESS // TODO: verify
        | hal::Features::INSTANCE_RATE
        | hal::Features::INDEPENDENT_BLENDING // TODO: verify
        | hal::Features::SAMPLER_MIP_LOD_BIAS
        | hal::Features::SAMPLER_MIRROR_CLAMP_EDGE
        | hal::Features::SAMPLER_ANISOTROPY
        | hal::Features::DEPTH_CLAMP
        | hal::Features::NDC_Y_UP;

    features.set(
        hal::Features::TEXTURE_DESCRIPTOR_ARRAY
        | hal::Features::FULL_DRAW_INDEX_U32
        | hal::Features::GEOMETRY_SHADER,
        feature_level >= d3dcommon::D3D_FEATURE_LEVEL_10_0
    );

    features.set(
        hal::Features::IMAGE_CUBE_ARRAY,
        feature_level >= d3dcommon::D3D_FEATURE_LEVEL_10_1
    );

    features.set(
        hal::Features::VERTEX_STORES_AND_ATOMICS
        | hal::Features::FRAGMENT_STORES_AND_ATOMICS
        | hal::Features::FORMAT_BC
        | hal::Features::TESSELLATION_SHADER
        | hal::Features::DRAW_INDIRECT_FIRST_INSTANCE,
        feature_level >= d3dcommon::D3D_FEATURE_LEVEL_11_0
    );

    features.set(
        hal::Features::LOGIC_OP, // TODO: Optional at 10_0 -> 11_0
        feature_level >= d3dcommon::D3D_FEATURE_LEVEL_11_1
    );

    features
}

const MAX_PUSH_CONSTANT_SIZE: usize = 256;

fn get_limits(feature_level: d3dcommon::D3D_FEATURE_LEVEL) -> hal::Limits {
    let max_texture_uv_dimension = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1 | d3dcommon::D3D_FEATURE_LEVEL_9_2 => 2048,
        d3dcommon::D3D_FEATURE_LEVEL_9_3 => 4096,
        d3dcommon::D3D_FEATURE_LEVEL_10_0 | d3dcommon::D3D_FEATURE_LEVEL_10_1 => 8192,
        d3dcommon::D3D_FEATURE_LEVEL_11_0 | d3dcommon::D3D_FEATURE_LEVEL_11_1 | _ => 16384,
    };

    let max_texture_w_dimension = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1
        | d3dcommon::D3D_FEATURE_LEVEL_9_2
        | d3dcommon::D3D_FEATURE_LEVEL_9_3 => 256,
        d3dcommon::D3D_FEATURE_LEVEL_10_0
        | d3dcommon::D3D_FEATURE_LEVEL_10_1
        | d3dcommon::D3D_FEATURE_LEVEL_11_0
        | d3dcommon::D3D_FEATURE_LEVEL_11_1
        | _ => 2048,
    };

    let max_texture_cube_dimension = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1
        | d3dcommon::D3D_FEATURE_LEVEL_9_2 => 512,
        _ => max_texture_uv_dimension,
    };

    let max_image_uav = 2;
    let max_buffer_uav = d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT as usize - max_image_uav;

    let max_input_slots = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1
        | d3dcommon::D3D_FEATURE_LEVEL_9_2
        | d3dcommon::D3D_FEATURE_LEVEL_9_3
        | d3dcommon::D3D_FEATURE_LEVEL_10_0 => 16,
        d3dcommon::D3D_FEATURE_LEVEL_10_1
        | d3dcommon::D3D_FEATURE_LEVEL_11_0
        | d3dcommon::D3D_FEATURE_LEVEL_11_1
        | _ => 32,
    };

    let max_color_attachments = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1
        | d3dcommon::D3D_FEATURE_LEVEL_9_2
        | d3dcommon::D3D_FEATURE_LEVEL_9_3
        | d3dcommon::D3D_FEATURE_LEVEL_10_0 => 4,
        d3dcommon::D3D_FEATURE_LEVEL_10_1
        | d3dcommon::D3D_FEATURE_LEVEL_11_0
        | d3dcommon::D3D_FEATURE_LEVEL_11_1
        | _ => 8,
    };

    // https://docs.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11device-checkmultisamplequalitylevels#remarks
    // for more information.
    let max_samples = match feature_level {
        d3dcommon::D3D_FEATURE_LEVEL_9_1
        | d3dcommon::D3D_FEATURE_LEVEL_9_2
        | d3dcommon::D3D_FEATURE_LEVEL_9_3
        | d3dcommon::D3D_FEATURE_LEVEL_10_0 => 0b0001, // Conservative, MSAA isn't required.
        d3dcommon::D3D_FEATURE_LEVEL_10_1 => 0b0101, // Optimistic, 4xMSAA is required on all formats _but_ RGBA32.
        d3dcommon::D3D_FEATURE_LEVEL_11_0
        | d3dcommon::D3D_FEATURE_LEVEL_11_1
        | _ => 0b1101, // Optimistic, 8xMSAA and 4xMSAA is required on all formats _but_ RGBA32 which requires 4x.
    };

    let max_constant_buffers = d3d11::D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT - 1;

    hal::Limits {
        max_image_1d_size: max_texture_uv_dimension,
        max_image_2d_size: max_texture_uv_dimension,
        max_image_3d_size: max_texture_w_dimension,
        max_image_cube_size: max_texture_cube_dimension,
        max_image_array_layers: max_texture_cube_dimension as _,
        max_per_stage_descriptor_samplers: d3d11::D3D11_COMMONSHADER_SAMPLER_SLOT_COUNT as _,
        // Leave top buffer for push constants
        max_per_stage_descriptor_uniform_buffers: max_constant_buffers as _,
        max_per_stage_descriptor_storage_buffers: max_buffer_uav,
        max_per_stage_descriptor_storage_images: max_image_uav,
        max_per_stage_descriptor_sampled_images: d3d11::D3D11_COMMONSHADER_INPUT_RESOURCE_REGISTER_COUNT as _,
        max_descriptor_set_uniform_buffers_dynamic: max_constant_buffers as _,
        max_descriptor_set_storage_buffers_dynamic: 0, // TODO: Implement dynamic offsets for storage buffers
        max_texel_elements: max_texture_uv_dimension as _, //TODO
        max_patch_size: d3d11::D3D11_IA_PATCH_MAX_CONTROL_POINT_COUNT as _,
        max_viewports: d3d11::D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as _,
        max_viewport_dimensions: [d3d11::D3D11_VIEWPORT_BOUNDS_MAX; 2],
        max_framebuffer_extent: hal::image::Extent {
            //TODO
            width: 4096,
            height: 4096,
            depth: 1,
        },
        max_compute_work_group_count: [
            d3d11::D3D11_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION,
            d3d11::D3D11_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION,
            d3d11::D3D11_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION,
        ],
        max_compute_work_group_invocations: d3d11::D3D11_CS_THREAD_GROUP_MAX_THREADS_PER_GROUP as _,
        max_compute_work_group_size: [
            d3d11::D3D11_CS_THREAD_GROUP_MAX_X,
            d3d11::D3D11_CS_THREAD_GROUP_MAX_Y,
            d3d11::D3D11_CS_THREAD_GROUP_MAX_Z,
        ], // TODO
        max_vertex_input_attribute_offset: 255, // TODO
        max_vertex_input_attributes: max_input_slots,
        max_vertex_input_binding_stride: d3d11::D3D11_REQ_MULTI_ELEMENT_STRUCTURE_SIZE_IN_BYTES as _,
        max_vertex_input_bindings: d3d11::D3D11_IA_VERTEX_INPUT_RESOURCE_SLOT_COUNT as _, // TODO: verify same as attributes
        max_vertex_output_components: d3d11::D3D11_VS_OUTPUT_REGISTER_COUNT as _, // TODO
        min_texel_buffer_offset_alignment: 1,                                     // TODO
        min_uniform_buffer_offset_alignment: 16,
        min_storage_buffer_offset_alignment: 16, // TODO
        framebuffer_color_sample_counts: max_samples,
        framebuffer_depth_sample_counts: max_samples,
        framebuffer_stencil_sample_counts: max_samples,
        max_color_attachments,
        buffer_image_granularity: 1,
        non_coherent_atom_size: 1, // TODO
        max_sampler_anisotropy: 16.,
        optimal_buffer_copy_offset_alignment: 1, // TODO
        // buffer -> image and image -> buffer paths use compute shaders that, at maximum, read 4 pixels from the buffer
        // at a time, so need an alignment of at least 4.
        optimal_buffer_copy_pitch_alignment: 4,
        min_vertex_input_binding_stride_alignment: 1,
        max_push_constants_size: MAX_PUSH_CONSTANT_SIZE,
        max_uniform_buffer_range: 1 << 16,
        ..hal::Limits::default() //TODO
    }
}

fn get_format_properties(
    device: ComPtr<d3d11::ID3D11Device>,
) -> [format::Properties; format::NUM_FORMATS] {
    let mut format_properties = [format::Properties::default(); format::NUM_FORMATS];
    for (i, props) in &mut format_properties.iter_mut().enumerate().skip(1) {
        let format: format::Format = unsafe { mem::transmute(i as u32) };

        let dxgi_format = match conv::map_format(format) {
            Some(format) => format,
            None => continue,
        };

        let mut support = d3d11::D3D11_FEATURE_DATA_FORMAT_SUPPORT {
            InFormat: dxgi_format,
            OutFormatSupport: 0,
        };
        let mut support_2 = d3d11::D3D11_FEATURE_DATA_FORMAT_SUPPORT2 {
            InFormat: dxgi_format,
            OutFormatSupport2: 0,
        };

        let hr = unsafe {
            device.CheckFeatureSupport(
                d3d11::D3D11_FEATURE_FORMAT_SUPPORT,
                &mut support as *mut _ as *mut _,
                mem::size_of::<d3d11::D3D11_FEATURE_DATA_FORMAT_SUPPORT>() as UINT,
            )
        };

        if hr == winerror::S_OK {
            let can_buffer = 0 != support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_BUFFER;
            let can_image = 0
                != support.OutFormatSupport
                    & (d3d11::D3D11_FORMAT_SUPPORT_TEXTURE1D
                        | d3d11::D3D11_FORMAT_SUPPORT_TEXTURE2D
                        | d3d11::D3D11_FORMAT_SUPPORT_TEXTURE3D
                        | d3d11::D3D11_FORMAT_SUPPORT_TEXTURECUBE);
            let can_linear = can_image && !format.surface_desc().is_compressed();
            if can_image {
                props.optimal_tiling |=
                    format::ImageFeature::SAMPLED | format::ImageFeature::BLIT_SRC;
            }
            if can_linear {
                props.linear_tiling |=
                    format::ImageFeature::SAMPLED | format::ImageFeature::BLIT_SRC;
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_IA_VERTEX_BUFFER != 0 {
                props.buffer_features |= format::BufferFeature::VERTEX;
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_SHADER_SAMPLE != 0 {
                props.optimal_tiling |= format::ImageFeature::SAMPLED_LINEAR;
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_RENDER_TARGET != 0 {
                props.optimal_tiling |=
                    format::ImageFeature::COLOR_ATTACHMENT | format::ImageFeature::BLIT_DST;
                if can_linear {
                    props.linear_tiling |=
                        format::ImageFeature::COLOR_ATTACHMENT | format::ImageFeature::BLIT_DST;
                }
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_BLENDABLE != 0 {
                props.optimal_tiling |= format::ImageFeature::COLOR_ATTACHMENT_BLEND;
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_DEPTH_STENCIL != 0 {
                props.optimal_tiling |= format::ImageFeature::DEPTH_STENCIL_ATTACHMENT;
            }
            if support.OutFormatSupport & d3d11::D3D11_FORMAT_SUPPORT_SHADER_LOAD != 0 {
                //TODO: check d3d12::D3D12_FORMAT_SUPPORT2_UAV_TYPED_LOAD ?
                if can_buffer {
                    props.buffer_features |= format::BufferFeature::UNIFORM_TEXEL;
                }
            }

            let hr = unsafe {
                device.CheckFeatureSupport(
                    d3d11::D3D11_FEATURE_FORMAT_SUPPORT2,
                    &mut support_2 as *mut _ as *mut _,
                    mem::size_of::<d3d11::D3D11_FEATURE_DATA_FORMAT_SUPPORT2>() as UINT,
                )
            };
            if hr == winerror::S_OK {
                if support_2.OutFormatSupport2 & d3d11::D3D11_FORMAT_SUPPORT2_UAV_ATOMIC_ADD != 0 {
                    //TODO: other atomic flags?
                    if can_buffer {
                        props.buffer_features |= format::BufferFeature::STORAGE_TEXEL_ATOMIC;
                    }
                    if can_image {
                        props.optimal_tiling |= format::ImageFeature::STORAGE_ATOMIC;
                    }
                }
                if support_2.OutFormatSupport2 & d3d11::D3D11_FORMAT_SUPPORT2_UAV_TYPED_STORE != 0 {
                    if can_buffer {
                        props.buffer_features |= format::BufferFeature::STORAGE_TEXEL;
                    }
                    if can_image {
                        props.optimal_tiling |= format::ImageFeature::STORAGE;
                    }
                }
            }
        }

        //TODO: blits, linear tiling
    }

    format_properties
}

impl hal::Instance<Backend> for Instance {
    fn create(_: &str, _: u32) -> Result<Self, hal::UnsupportedBackend> {
        // TODO: get the latest factory we can find

        match dxgi::get_dxgi_factory() {
            Ok((library_dxgi, factory, dxgi_version)) => {
                info!("DXGI version: {:?}", dxgi_version);
                let library_d3d11 = Arc::new(
                    libloading::Library::new("d3d11.dll").map_err(|_| hal::UnsupportedBackend)?,
                );
                Ok(Instance {
                    factory,
                    dxgi_version,
                    library_d3d11,
                    library_dxgi,
                })
            }
            Err(hr) => {
                info!("Failed on factory creation: {:?}", hr);
                Err(hal::UnsupportedBackend)
            }
        }
    }

    fn enumerate_adapters(&self) -> Vec<adapter::Adapter<Backend>> {
        let mut adapters = Vec::new();
        let mut idx = 0;

        let func: libloading::Symbol<CreateFun> =
            match unsafe { self.library_d3d11.get(b"D3D11CreateDevice") } {
                Ok(func) => func,
                Err(e) => {
                    error!("Unable to get device creation function: {:?}", e);
                    return Vec::new();
                }
            };

        while let Ok((adapter, info)) =
            dxgi::get_adapter(idx, self.factory.as_raw(), self.dxgi_version)
        {
            idx += 1;

            use hal::memory::Properties;

            // TODO: move into function?
            let (device, feature_level) = {
                let feature_level = get_feature_level(&func, adapter.as_raw());

                let mut device = ptr::null_mut();
                let hr = unsafe {
                    func(
                        adapter.as_raw() as *mut _,
                        d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
                        ptr::null_mut(),
                        0,
                        [feature_level].as_ptr(),
                        1,
                        d3d11::D3D11_SDK_VERSION,
                        &mut device as *mut *mut _ as *mut *mut _,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                };

                if !winerror::SUCCEEDED(hr) {
                    continue;
                }

                (
                    unsafe { ComPtr::<d3d11::ID3D11Device>::from_raw(device) },
                    feature_level,
                )
            };

            let memory_properties = adapter::MemoryProperties {
                memory_types: vec![
                    adapter::MemoryType {
                        properties: Properties::DEVICE_LOCAL,
                        heap_index: 0,
                    },
                    adapter::MemoryType {
                        properties: Properties::CPU_VISIBLE
                            | Properties::COHERENT
                            | Properties::CPU_CACHED,
                        heap_index: 1,
                    },
                    adapter::MemoryType {
                        properties: Properties::CPU_VISIBLE | Properties::CPU_CACHED,
                        heap_index: 1,
                    },
                ],
                // TODO: would using *VideoMemory and *SystemMemory from
                //       DXGI_ADAPTER_DESC be too optimistic? :)
                memory_heaps: vec![!0, !0],
            };

            let limits = get_limits(feature_level);
            let features = get_features(device.clone(), feature_level);
            let format_properties = get_format_properties(device.clone());
            let hints = hal::Hints::BASE_VERTEX_INSTANCE_DRAWING;

            let physical_device = PhysicalDevice {
                adapter,
                library_d3d11: Arc::clone(&self.library_d3d11),
                features,
                hints,
                limits,
                memory_properties,
                format_properties,
            };

            info!("{:#?}", info);

            adapters.push(adapter::Adapter {
                info,
                physical_device,
                queue_families: vec![QueueFamily],
            });
        }

        adapters
    }

    unsafe fn create_surface(
        &self,
        has_handle: &impl raw_window_handle::HasRawWindowHandle,
    ) -> Result<Surface, hal::window::InitError> {
        match has_handle.raw_window_handle() {
            raw_window_handle::RawWindowHandle::Windows(handle) => {
                Ok(self.create_surface_from_hwnd(handle.hwnd))
            }
            _ => Err(hal::window::InitError::UnsupportedWindowHandle),
        }
    }

    unsafe fn destroy_surface(&self, _surface: Surface) {
        // TODO: Implement Surface cleanup
    }
}

pub struct PhysicalDevice {
    adapter: ComPtr<IDXGIAdapter>,
    library_d3d11: Arc<libloading::Library>,
    features: hal::Features,
    hints: hal::Hints,
    limits: hal::Limits,
    memory_properties: adapter::MemoryProperties,
    format_properties: [format::Properties; format::NUM_FORMATS],
}

impl fmt::Debug for PhysicalDevice {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("PhysicalDevice")
    }
}

unsafe impl Send for PhysicalDevice {}
unsafe impl Sync for PhysicalDevice {}

// TODO: does the adapter we get earlier matter for feature level?
fn get_feature_level(func: &CreateFun, adapter: *mut IDXGIAdapter) -> d3dcommon::D3D_FEATURE_LEVEL {
    let requested_feature_levels = [
        d3dcommon::D3D_FEATURE_LEVEL_11_1,
        d3dcommon::D3D_FEATURE_LEVEL_11_0,
        d3dcommon::D3D_FEATURE_LEVEL_10_1,
        d3dcommon::D3D_FEATURE_LEVEL_10_0,
        d3dcommon::D3D_FEATURE_LEVEL_9_3,
        d3dcommon::D3D_FEATURE_LEVEL_9_2,
        d3dcommon::D3D_FEATURE_LEVEL_9_1,
    ];

    let mut feature_level = d3dcommon::D3D_FEATURE_LEVEL_9_1;
    let hr = unsafe {
        func(
            adapter,
            d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
            ptr::null_mut(),
            0,
            requested_feature_levels[..].as_ptr(),
            requested_feature_levels.len() as _,
            d3d11::D3D11_SDK_VERSION,
            ptr::null_mut(),
            &mut feature_level as *mut _,
            ptr::null_mut(),
        )
    };

    if !winerror::SUCCEEDED(hr) {
        // if there is no 11.1 runtime installed, requesting
        // `D3D_FEATURE_LEVEL_11_1` will return E_INVALIDARG so we just retry
        // without that
        if hr == winerror::E_INVALIDARG {
            let hr = unsafe {
                func(
                    adapter,
                    d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
                    ptr::null_mut(),
                    0,
                    requested_feature_levels[1..].as_ptr(),
                    (requested_feature_levels.len() - 1) as _,
                    d3d11::D3D11_SDK_VERSION,
                    ptr::null_mut(),
                    &mut feature_level as *mut _,
                    ptr::null_mut(),
                )
            };

            if !winerror::SUCCEEDED(hr) {
                // TODO: device might not support any feature levels?
                unimplemented!();
            }
        }
    }

    feature_level
}

// TODO: PhysicalDevice
impl adapter::PhysicalDevice<Backend> for PhysicalDevice {
    unsafe fn open(
        &self,
        families: &[(&QueueFamily, &[queue::QueuePriority])],
        requested_features: hal::Features,
    ) -> Result<adapter::Gpu<Backend>, hal::device::CreationError> {
        let func: libloading::Symbol<CreateFun> =
            self.library_d3d11.get(b"D3D11CreateDevice").unwrap();

        let (device, cxt) = {
            if !self.features().contains(requested_features) {
                return Err(hal::device::CreationError::MissingFeature);
            }

            let feature_level = get_feature_level(&func, self.adapter.as_raw());
            let mut returned_level = d3dcommon::D3D_FEATURE_LEVEL_9_1;

            #[cfg(debug_assertions)]
            let create_flags = d3d11::D3D11_CREATE_DEVICE_DEBUG;
            #[cfg(not(debug_assertions))]
            let create_flags = 0;

            // TODO: request debug device only on debug config?
            let mut device: *mut d3d11::ID3D11Device = ptr::null_mut();
            let mut cxt = ptr::null_mut();
            let hr = func(
                self.adapter.as_raw() as *mut _,
                d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
                ptr::null_mut(),
                create_flags,
                [feature_level].as_ptr(),
                1,
                d3d11::D3D11_SDK_VERSION,
                &mut device as *mut *mut _ as *mut *mut _,
                &mut returned_level as *mut _,
                &mut cxt as *mut *mut _ as *mut *mut _,
            );

            // NOTE: returns error if adapter argument is non-null and driver
            // type is not unknown; or if debug device is requested but not
            // present
            if !winerror::SUCCEEDED(hr) {
                return Err(hal::device::CreationError::InitializationFailed);
            }

            info!("feature level={:x}=FL{}_{}", feature_level, feature_level >> 12, feature_level >> 8 & 0xF);

            (ComPtr::from_raw(device), ComPtr::from_raw(cxt))
        };

        let device1 = device.cast::<d3d11_1::ID3D11Device1>().ok();

        let device = device::Device::new(
            device,
            device1,
            cxt,
            requested_features,
            self.memory_properties.clone(),
        );

        // TODO: deferred context => 1 cxt/queue?
        let queue_groups = families
            .into_iter()
            .map(|&(_family, prio)| {
                assert_eq!(prio.len(), 1);
                let mut group = queue::QueueGroup::new(queue::QueueFamilyId(0));

                // TODO: multiple queues?
                let queue = CommandQueue {
                    context: device.context.clone(),
                };
                group.add_queue(queue);
                group
            })
            .collect();

        Ok(adapter::Gpu {
            device,
            queue_groups,
        })
    }

    fn format_properties(&self, fmt: Option<format::Format>) -> format::Properties {
        let idx = fmt.map(|fmt| fmt as usize).unwrap_or(0);
        self.format_properties[idx]
    }

    fn image_format_properties(
        &self,
        format: format::Format,
        dimensions: u8,
        tiling: image::Tiling,
        usage: image::Usage,
        view_caps: image::ViewCapabilities,
    ) -> Option<image::FormatProperties> {
        conv::map_format(format)?; //filter out unknown formats

        let supported_usage = {
            use hal::image::Usage as U;
            let format_props = &self.format_properties[format as usize];
            let props = match tiling {
                image::Tiling::Optimal => format_props.optimal_tiling,
                image::Tiling::Linear => format_props.linear_tiling,
            };
            let mut flags = U::empty();
            // Note: these checks would have been nicer if we had explicit BLIT usage
            if props.contains(format::ImageFeature::BLIT_SRC) {
                flags |= U::TRANSFER_SRC;
            }
            if props.contains(format::ImageFeature::BLIT_DST) {
                flags |= U::TRANSFER_DST;
            }
            if props.contains(format::ImageFeature::SAMPLED) {
                flags |= U::SAMPLED;
            }
            if props.contains(format::ImageFeature::STORAGE) {
                flags |= U::STORAGE;
            }
            if props.contains(format::ImageFeature::COLOR_ATTACHMENT) {
                flags |= U::COLOR_ATTACHMENT;
            }
            if props.contains(format::ImageFeature::DEPTH_STENCIL_ATTACHMENT) {
                flags |= U::DEPTH_STENCIL_ATTACHMENT;
            }
            flags
        };
        if !supported_usage.contains(usage) {
            return None;
        }

        let max_resource_size =
            (d3d11::D3D11_REQ_RESOURCE_SIZE_IN_MEGABYTES_EXPRESSION_A_TERM as usize) << 20;
        Some(match tiling {
            image::Tiling::Optimal => image::FormatProperties {
                max_extent: match dimensions {
                    1 => image::Extent {
                        width: d3d11::D3D11_REQ_TEXTURE1D_U_DIMENSION,
                        height: 1,
                        depth: 1,
                    },
                    2 => image::Extent {
                        width: d3d11::D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION,
                        height: d3d11::D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION,
                        depth: 1,
                    },
                    3 => image::Extent {
                        width: d3d11::D3D11_REQ_TEXTURE3D_U_V_OR_W_DIMENSION,
                        height: d3d11::D3D11_REQ_TEXTURE3D_U_V_OR_W_DIMENSION,
                        depth: d3d11::D3D11_REQ_TEXTURE3D_U_V_OR_W_DIMENSION,
                    },
                    _ => return None,
                },
                max_levels: d3d11::D3D11_REQ_MIP_LEVELS as _,
                max_layers: match dimensions {
                    1 => d3d11::D3D11_REQ_TEXTURE1D_ARRAY_AXIS_DIMENSION as _,
                    2 => d3d11::D3D11_REQ_TEXTURE2D_ARRAY_AXIS_DIMENSION as _,
                    _ => return None,
                },
                sample_count_mask: if dimensions == 2
                    && !view_caps.contains(image::ViewCapabilities::KIND_CUBE)
                    && (usage.contains(image::Usage::COLOR_ATTACHMENT)
                        | usage.contains(image::Usage::DEPTH_STENCIL_ATTACHMENT))
                {
                    0x3F //TODO: use D3D12_FEATURE_DATA_FORMAT_SUPPORT
                } else {
                    0x1
                },
                max_resource_size,
            },
            image::Tiling::Linear => image::FormatProperties {
                max_extent: match dimensions {
                    2 => image::Extent {
                        width: d3d11::D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION,
                        height: d3d11::D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION,
                        depth: 1,
                    },
                    _ => return None,
                },
                max_levels: 1,
                max_layers: 1,
                sample_count_mask: 0x1,
                max_resource_size,
            },
        })
    }

    fn memory_properties(&self) -> adapter::MemoryProperties {
        self.memory_properties.clone()
    }

    fn features(&self) -> hal::Features {
        self.features
    }

    fn hints(&self) -> hal::Hints {
        self.hints
    }

    fn limits(&self) -> Limits {
        self.limits
    }
}

struct Presentation {
    swapchain: ComPtr<IDXGISwapChain>,
    view: ComPtr<d3d11::ID3D11RenderTargetView>,
    format: format::Format,
    size: window::Extent2D,
    mode: window::PresentMode,
    is_init: bool,
}

pub struct Surface {
    pub(crate) factory: ComPtr<IDXGIFactory>,
    wnd_handle: HWND,
    presentation: Option<Presentation>,
}

impl fmt::Debug for Surface {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Surface")
    }
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl window::Surface<Backend> for Surface {
    fn supports_queue_family(&self, _queue_family: &QueueFamily) -> bool {
        true
    }

    fn capabilities(&self, _physical_device: &PhysicalDevice) -> window::SurfaceCapabilities {
        let current_extent = unsafe {
            let mut rect: RECT = mem::zeroed();
            assert_ne!(
                0,
                GetClientRect(self.wnd_handle as *mut _, &mut rect as *mut RECT)
            );
            Some(window::Extent2D {
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
            })
        };

        // TODO: flip swap effects require dx11.1/windows8
        // NOTE: some swap effects affect msaa capabilities..
        // TODO: _DISCARD swap effects can only have one image?
        window::SurfaceCapabilities {
            present_modes: window::PresentMode::IMMEDIATE
                | window::PresentMode::FIFO,
            composite_alpha_modes: window::CompositeAlphaMode::OPAQUE, //TODO
            image_count: 1..=16,                                       // TODO:
            current_extent,
            extents: window::Extent2D {
                width: 16,
                height: 16,
            }..=window::Extent2D {
                width: 4096,
                height: 4096,
            },
            max_image_layers: 1,
            usage: image::Usage::COLOR_ATTACHMENT,
        }
    }

    fn supported_formats(&self, _physical_device: &PhysicalDevice) -> Option<Vec<format::Format>> {
        Some(vec![
            format::Format::Bgra8Srgb,
            format::Format::Bgra8Unorm,
            format::Format::Rgba8Srgb,
            format::Format::Rgba8Unorm,
            format::Format::A2b10g10r10Unorm,
            format::Format::Rgba16Sfloat,
        ])
    }
}

impl window::PresentationSurface<Backend> for Surface {
    type SwapchainImage = ImageView;

    unsafe fn configure_swapchain(
        &mut self,
        device: &device::Device,
        config: window::SwapchainConfig,
    ) -> Result<(), window::CreationError> {
        assert!(image::Usage::COLOR_ATTACHMENT.contains(config.image_usage));

        let swapchain = match self.presentation.take() {
            Some(present) => {
                if present.format == config.format && present.size == config.extent {
                    self.presentation = Some(present);
                    return Ok(());
                }
                let non_srgb_format = conv::map_format_nosrgb(config.format).unwrap();
                drop(present.view);
                let result = present.swapchain.ResizeBuffers(
                    config.image_count,
                    config.extent.width,
                    config.extent.height,
                    non_srgb_format,
                    0,
                );
                if result != winerror::S_OK {
                    error!("ResizeBuffers failed with 0x{:x}", result as u32);
                    return Err(window::CreationError::WindowInUse(hal::device::WindowInUse));
                }
                present.swapchain
            }
            None => {
                let (swapchain, _) =
                    device.create_swapchain_impl(&config, self.wnd_handle, self.factory.clone())?;
                swapchain
            }
        };

        let mut resource: *mut d3d11::ID3D11Resource = ptr::null_mut();
        assert_eq!(
            winerror::S_OK,
            swapchain.GetBuffer(
                0 as _,
                &d3d11::ID3D11Resource::uuidof(),
                &mut resource as *mut *mut _ as *mut *mut _,
            )
        );
        set_debug_name(&*resource, "Swapchain Image");

        let kind = image::Kind::D2(config.extent.width, config.extent.height, 1, 1);
        let format = conv::map_format(config.format).unwrap();
        let decomposed = conv::DecomposedDxgiFormat::from_dxgi_format(format);

        let view_info = ViewInfo {
            resource,
            kind,
            caps: image::ViewCapabilities::empty(),
            view_kind: image::ViewKind::D2,
            format: decomposed.rtv.unwrap(),
            levels: 0..1,
            layers: 0..1,
        };
        let view = device.view_image_as_render_target(&view_info).unwrap();
        set_debug_name(&view, "Swapchain Image View");

        (*resource).Release();

        self.presentation = Some(Presentation {
            swapchain,
            view,
            format: config.format,
            size: config.extent,
            mode: config.present_mode,
            is_init: true,
        });
        Ok(())
    }

    unsafe fn unconfigure_swapchain(&mut self, _device: &device::Device) {
        self.presentation = None;
    }

    unsafe fn acquire_image(
        &mut self,
        _timeout_ns: u64, //TODO: use the timeout
    ) -> Result<(ImageView, Option<window::Suboptimal>), window::AcquireError> {
        let present = self.presentation.as_ref().unwrap();
        let image_view = ImageView {
            subresource: d3d11::D3D11CalcSubresource(0, 0, 1),
            format: present.format,
            rtv_handle: Some(present.view.as_raw()),
            dsv_handle: None,
            srv_handle: None,
            uav_handle: None,
            rodsv_handle: None,
            owned: false,
        };
        Ok((image_view, None))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct QueueFamily;

impl queue::QueueFamily for QueueFamily {
    fn queue_type(&self) -> queue::QueueType {
        queue::QueueType::General
    }
    fn max_queues(&self) -> usize {
        1
    }
    fn id(&self) -> queue::QueueFamilyId {
        queue::QueueFamilyId(0)
    }
}

#[derive(Clone)]
pub struct CommandQueue {
    context: ComPtr<d3d11::ID3D11DeviceContext>,
}

impl fmt::Debug for CommandQueue {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("CommandQueue")
    }
}

unsafe impl Send for CommandQueue {}
unsafe impl Sync for CommandQueue {}

impl queue::CommandQueue<Backend> for CommandQueue {
    unsafe fn submit<'a, T, Ic, S, Iw, Is>(
        &mut self,
        submission: queue::Submission<Ic, Iw, Is>,
        fence: Option<&Fence>,
    ) where
        T: 'a + Borrow<CommandBuffer>,
        Ic: IntoIterator<Item = &'a T>,
        S: 'a + Borrow<Semaphore>,
        Iw: IntoIterator<Item = (&'a S, pso::PipelineStage)>,
        Is: IntoIterator<Item = &'a S>,
    {
        let _scope = debug_scope!(&self.context, "Submit(fence={:?})", fence);
        for cmd_buf in submission.command_buffers {
            let cmd_buf = cmd_buf.borrow();

            let _scope = debug_scope!(
                &self.context,
                "CommandBuffer ({}/{})",
                cmd_buf.flush_coherent_memory.len(),
                cmd_buf.invalidate_coherent_memory.len()
            );

            {
                let _scope = debug_scope!(&self.context, "Pre-Exec: Flush");
                for sync in &cmd_buf.flush_coherent_memory {
                    sync.do_flush(&self.context);
                }
            }
            self.context
                .ExecuteCommandList(cmd_buf.as_raw_list().as_raw(), FALSE);
            {
                let _scope = debug_scope!(&self.context, "Post-Exec: Invalidate");
                for sync in &cmd_buf.invalidate_coherent_memory {
                    sync.do_invalidate(&self.context);
                }
            }
        }

        if let Some(fence) = fence {
            *fence.mutex.lock() = true;
            fence.condvar.notify_all();
        }
    }

    unsafe fn present(
        &mut self,
        surface: &mut Surface,
        _image: ImageView,
        _wait_semaphore: Option<&Semaphore>,
    ) -> Result<Option<window::Suboptimal>, window::PresentError> {
        let mut presentation = surface.presentation.as_mut().unwrap();
        let (interval, flags) = match presentation.mode {
            window::PresentMode::IMMEDIATE => (0, 0),
            //Note: this ends up not presenting anything for some reason
            //window::PresentMode::MAILBOX if !presentation.is_init => (1, DXGI_PRESENT_DO_NOT_SEQUENCE),
            window::PresentMode::FIFO => (1, 0),
            _ => (0, 0),
        };
        presentation.is_init = false;
        presentation.swapchain.Present(interval, flags);
        Ok(None)
    }

    fn wait_idle(&self) -> Result<(), hal::device::OutOfMemory> {
        // unimplemented!()
        Ok(())
    }
}

#[derive(Debug)]
pub struct AttachmentClear {
    subpass_id: Option<pass::SubpassId>,
    attachment_id: usize,
    raw: command::AttachmentClear,
}

#[derive(Debug)]
pub struct RenderPassCache {
    pub render_pass: RenderPass,
    pub framebuffer: Framebuffer,
    pub attachment_clear_values: Vec<AttachmentClear>,
    pub target_rect: pso::Rect,
    pub current_subpass: pass::SubpassId,
}

impl RenderPassCache {
    pub fn start_subpass(
        &mut self,
        internal: &internal::Internal,
        context: &ComPtr<d3d11::ID3D11DeviceContext>,
        cache: &mut CommandBufferState,
    ) {
        let attachments = self
            .attachment_clear_values
            .iter()
            .filter(|clear| clear.subpass_id == Some(self.current_subpass))
            .map(|clear| clear.raw);

        cache.dirty_flag.insert(
            DirtyStateFlag::GRAPHICS_PIPELINE
                | DirtyStateFlag::DEPTH_STENCIL_STATE
                | DirtyStateFlag::PIPELINE_PS
                | DirtyStateFlag::VIEWPORTS
                | DirtyStateFlag::RENDER_TARGETS_AND_UAVS,
        );
        internal.clear_attachments(
            context,
            attachments,
            &[pso::ClearRect {
                rect: self.target_rect,
                layers: 0..1,
            }],
            &self,
        );

        let subpass = &self.render_pass.subpasses[self.current_subpass as usize];
        let color_views = subpass
            .color_attachments
            .iter()
            .map(|&(id, _)| {
                self.framebuffer.attachments[id]
                    .rtv_handle
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let (ds_view, rods_view) = match subpass.depth_stencil_attachment {
            Some((id, _)) => {
                let attachment = &self.framebuffer.attachments[id];
                let ds_view = attachment
                    .dsv_handle
                    .unwrap();

                let rods_view = attachment
                    .rodsv_handle
                    .unwrap();

                (Some(ds_view), Some(rods_view))
            },
            None => (None, None),
        };

        cache.set_render_targets(&color_views, ds_view, rods_view);
        cache.bind(context);
    }

    fn resolve_msaa(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        let subpass: &SubpassDesc = &self.render_pass.subpasses[self.current_subpass as usize];

        for (&(color_id, _), &(resolve_id, _)) in subpass.color_attachments.iter().zip(subpass.resolve_attachments.iter()) {
            if color_id == pass::ATTACHMENT_UNUSED || resolve_id == pass::ATTACHMENT_UNUSED {
                continue;
            }

            let color_framebuffer = &self.framebuffer.attachments[color_id];
            let resolve_framebuffer = &self.framebuffer.attachments[resolve_id];

            let mut color_resource: *mut d3d11::ID3D11Resource = ptr::null_mut();
            let mut resolve_resource: *mut d3d11::ID3D11Resource = ptr::null_mut();

            unsafe {
                (&*color_framebuffer.rtv_handle.expect("Framebuffer must have COLOR_ATTACHMENT usage")).GetResource(&mut color_resource as *mut *mut _);
                (&*resolve_framebuffer.rtv_handle.expect("Resolve texture must have COLOR_ATTACHMENT usage")).GetResource(&mut resolve_resource as *mut *mut _);

                context.ResolveSubresource(
                    resolve_resource,
                    resolve_framebuffer.subresource,
                    color_resource,
                    color_framebuffer.subresource,
                    conv::map_format(color_framebuffer.format).unwrap()
                );

                (&*color_resource).Release();
                (&*resolve_resource).Release();
            }
        }
    }

    pub fn next_subpass(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        self.resolve_msaa(context);
        self.current_subpass += 1;
    }
}

bitflags! {
    struct DirtyStateFlag : u32 {
        const RENDER_TARGETS_AND_UAVS = (1 << 1);
        const VERTEX_BUFFERS = (1 << 2);
        const GRAPHICS_PIPELINE = (1 << 3);
        const PIPELINE_GS = (1 << 4);
        const PIPELINE_HS = (1 << 5);
        const PIPELINE_DS = (1 << 6);
        const PIPELINE_PS = (1 << 7);
        const VIEWPORTS = (1 << 8);
        const BLEND_STATE = (1 << 9);
        const DEPTH_STENCIL_STATE = (1 << 10);
    }
}

pub struct CommandBufferState {
    dirty_flag: DirtyStateFlag,

    render_target_len: u32,
    render_targets: [*mut d3d11::ID3D11RenderTargetView; 8],
    uav_len: u32,
    uavs: [*mut d3d11::ID3D11UnorderedAccessView; d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT as _],
    depth_target: Option<*mut d3d11::ID3D11DepthStencilView>,
    readonly_depth_target: Option<*mut d3d11::ID3D11DepthStencilView>,
    depth_target_read_only: bool,
    graphics_pipeline: Option<GraphicsPipeline>,

    // a bitmask that keeps track of what vertex buffer bindings have been "bound" into
    // our vec
    bound_bindings: u32,
    // a bitmask that hold the required binding slots to be bound for the currently
    // bound pipeline
    required_bindings: Option<u32>,
    // the highest binding number in currently bound pipeline
    max_bindings: Option<u32>,
    viewports: Vec<d3d11::D3D11_VIEWPORT>,
    vertex_buffers: Vec<*mut d3d11::ID3D11Buffer>,
    vertex_offsets: Vec<u32>,
    vertex_strides: Vec<u32>,
    blend_factor: Option<[f32; 4]>,
    // we can only support one face (rather, both faces must have the same value)
    stencil_ref: Option<pso::StencilValue>,
    stencil_read_mask: Option<pso::StencilValue>,
    stencil_write_mask: Option<pso::StencilValue>,
    current_blend: Option<*mut d3d11::ID3D11BlendState>,
}

impl fmt::Debug for CommandBufferState {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("CommandBufferState")
    }
}

impl CommandBufferState {
    fn new() -> Self {
        CommandBufferState {
            dirty_flag: DirtyStateFlag::empty(),
            render_target_len: 0,
            render_targets: [ptr::null_mut(); 8],
            uav_len: 0,
            uavs: [ptr::null_mut(); 8],
            depth_target: None,
            readonly_depth_target: None,
            depth_target_read_only: false,
            graphics_pipeline: None,
            bound_bindings: 0,
            required_bindings: None,
            max_bindings: None,
            viewports: Vec::new(),
            vertex_buffers: Vec::new(),
            vertex_offsets: Vec::new(),
            vertex_strides: Vec::new(),
            blend_factor: None,
            stencil_ref: None,
            stencil_read_mask: None,
            stencil_write_mask: None,
            current_blend: None,
        }
    }

    fn clear(&mut self) {
        self.render_target_len = 0;
        self.uav_len = 0;
        self.depth_target = None;
        self.readonly_depth_target = None;
        self.depth_target_read_only = false;
        self.graphics_pipeline = None;
        self.bound_bindings = 0;
        self.required_bindings = None;
        self.max_bindings = None;
        self.viewports.clear();
        self.vertex_buffers.clear();
        self.vertex_offsets.clear();
        self.vertex_strides.clear();
        self.blend_factor = None;
        self.stencil_ref = None;
        self.stencil_read_mask = None;
        self.stencil_write_mask = None;
        self.current_blend = None;
    }

    pub fn set_vertex_buffer(
        &mut self,
        index: usize,
        offset: u32,
        buffer: *mut d3d11::ID3D11Buffer,
    ) {
        self.bound_bindings |= 1 << index as u32;

        if index >= self.vertex_buffers.len() {
            self.vertex_buffers.push(buffer);
            self.vertex_offsets.push(offset);
        } else {
            self.vertex_buffers[index] = buffer;
            self.vertex_offsets[index] = offset;
        }

        self.dirty_flag.insert(DirtyStateFlag::VERTEX_BUFFERS);
    }

    pub fn bind_vertex_buffers(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if !self.dirty_flag.contains(DirtyStateFlag::VERTEX_BUFFERS) {
            return;
        }

        if let Some(binding_count) = self.max_bindings {
            if self.vertex_buffers.len() >= binding_count as usize
                && self.vertex_strides.len() >= binding_count as usize
            {
                unsafe {
                    context.IASetVertexBuffers(
                        0,
                        binding_count,
                        self.vertex_buffers.as_ptr(),
                        self.vertex_strides.as_ptr(),
                        self.vertex_offsets.as_ptr(),
                    );
                }

                self.dirty_flag.remove(DirtyStateFlag::VERTEX_BUFFERS);
            }
        }
    }

    pub fn set_viewports(&mut self, viewports: &[d3d11::D3D11_VIEWPORT]) {
        self.viewports.clear();
        self.viewports.extend(viewports);

        self.dirty_flag.insert(DirtyStateFlag::VIEWPORTS);
    }

    pub fn bind_viewports(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if !self.dirty_flag.contains(DirtyStateFlag::VIEWPORTS) {
            return;
        }

        if let Some(ref pipeline) = self.graphics_pipeline {
            if let Some(ref viewport) = pipeline.baked_states.viewport {
                unsafe {
                    context.RSSetViewports(1, [conv::map_viewport(&viewport)].as_ptr());
                }
            } else {
                unsafe {
                    context.RSSetViewports(self.viewports.len() as u32, self.viewports.as_ptr());
                }
            }
        } else {
            unsafe {
                context.RSSetViewports(self.viewports.len() as u32, self.viewports.as_ptr());
            }
        }

        self.dirty_flag.remove(DirtyStateFlag::VIEWPORTS);
    }

    pub fn set_render_targets(
        &mut self,
        render_targets: &[*mut d3d11::ID3D11RenderTargetView],
        depth_target: Option<*mut d3d11::ID3D11DepthStencilView>,
        readonly_depth_target: Option<*mut d3d11::ID3D11DepthStencilView>
    ) {
        for (idx, &rt) in render_targets.iter().enumerate() {
            self.render_targets[idx] = rt;
        }

        self.render_target_len = render_targets.len() as u32;
        self.depth_target = depth_target;
        self.readonly_depth_target = readonly_depth_target;

        self.dirty_flag.insert(DirtyStateFlag::RENDER_TARGETS_AND_UAVS);
    }

    pub fn bind_render_targets(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if !self.dirty_flag.contains(DirtyStateFlag::RENDER_TARGETS_AND_UAVS) {
            return;
        }

        let depth_target = if self.depth_target_read_only {
            self.readonly_depth_target
        } else {
            self.depth_target
        }.unwrap_or(ptr::null_mut());

        let uav_start_index = d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT - self.uav_len;

        unsafe {
            if self.uav_len > 0 {
                context.OMSetRenderTargetsAndUnorderedAccessViews(
                    self.render_target_len,
                    self.render_targets.as_ptr(),
                    depth_target,
                    uav_start_index,
                    self.uav_len,
                    &self.uavs[uav_start_index as usize] as *const *mut _,
                    ptr::null(),
                )
            } else {
                context.OMSetRenderTargets(
                    self.render_target_len,
                    self.render_targets.as_ptr(),
                    depth_target,
                )
            };
        }

        self.dirty_flag.remove(DirtyStateFlag::RENDER_TARGETS_AND_UAVS);
    }

    pub fn set_blend_factor(&mut self, factor: [f32; 4]) {
        self.blend_factor = Some(factor);

        self.dirty_flag.insert(DirtyStateFlag::BLEND_STATE);
    }

    pub fn bind_blend_state(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if let Some(blend) = self.current_blend {
            let blend_color = if let Some(ref pipeline) = self.graphics_pipeline {
                pipeline
                    .baked_states
                    .blend_color
                    .or(self.blend_factor)
                    .unwrap_or([0f32; 4])
            } else {
                self.blend_factor.unwrap_or([0f32; 4])
            };

            // TODO: MSAA
            unsafe {
                context.OMSetBlendState(blend, &blend_color, !0);
            }

            self.dirty_flag.remove(DirtyStateFlag::BLEND_STATE);
        }
    }

    pub fn set_stencil_ref(&mut self, value: pso::StencilValue) {
        self.stencil_ref = Some(value);
        self.dirty_flag.insert(DirtyStateFlag::DEPTH_STENCIL_STATE);
    }

    pub fn bind_depth_stencil_state(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if !self.dirty_flag.contains(DirtyStateFlag::DEPTH_STENCIL_STATE) {
            return;
        }

        let pipeline = match self.graphics_pipeline {
            Some(ref pipeline) => pipeline,
            None => return,
        };

        if let Some(ref state) = pipeline.depth_stencil_state {
            let stencil_ref = state.stencil_ref.static_or(self.stencil_ref.unwrap_or(0));

            unsafe {
                context.OMSetDepthStencilState(state.raw.as_raw(), stencil_ref);
            }
        }

        self.dirty_flag.remove(DirtyStateFlag::DEPTH_STENCIL_STATE)
    }

    pub fn set_graphics_pipeline(&mut self, pipeline: GraphicsPipeline) {
        let prev = self.graphics_pipeline.take();

        let mut prev_has_ps = false;
        let mut prev_has_gs = false;
        let mut prev_has_ds = false;
        let mut prev_has_hs = false;
        if let Some(p) = prev {
            prev_has_ps = p.ps.is_some();
            prev_has_gs = p.gs.is_some();
            prev_has_ds = p.ds.is_some();
            prev_has_hs = p.hs.is_some();
        }

        if prev_has_ps || pipeline.ps.is_some() {
            self.dirty_flag.insert(DirtyStateFlag::PIPELINE_PS);
        }
        if prev_has_gs || pipeline.gs.is_some() {
            self.dirty_flag.insert(DirtyStateFlag::PIPELINE_GS);
        }
        if prev_has_ds || pipeline.ds.is_some() {
            self.dirty_flag.insert(DirtyStateFlag::PIPELINE_DS);
        }
        if prev_has_hs || pipeline.hs.is_some() {
            self.dirty_flag.insert(DirtyStateFlag::PIPELINE_HS);
        }

        // If we don't have depth stencil state, we use the old value, so we don't bother changing anything.
        let depth_target_read_only =
            pipeline
                .depth_stencil_state
                .as_ref()
                .map_or(self.depth_target_read_only, |ds| ds.read_only);

        if self.depth_target_read_only != depth_target_read_only {
            self.depth_target_read_only = depth_target_read_only;
            self.dirty_flag.insert(DirtyStateFlag::RENDER_TARGETS_AND_UAVS);
        }

        self.dirty_flag.insert(DirtyStateFlag::GRAPHICS_PIPELINE | DirtyStateFlag::DEPTH_STENCIL_STATE);

        self.graphics_pipeline = Some(pipeline);
    }

    pub fn bind_graphics_pipeline(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        if !self.dirty_flag.contains(DirtyStateFlag::GRAPHICS_PIPELINE) {
            return;
        }

        if let Some(ref pipeline) = self.graphics_pipeline {
            self.vertex_strides.clear();
            self.vertex_strides.extend(&pipeline.strides);

            self.required_bindings = Some(pipeline.required_bindings);
            self.max_bindings = Some(pipeline.max_vertex_bindings);
        };

        self.bind_vertex_buffers(context);

        if let Some(ref pipeline) = self.graphics_pipeline {
            unsafe {
                context.IASetPrimitiveTopology(pipeline.topology);
                context.IASetInputLayout(pipeline.input_layout.as_raw());

                context.VSSetShader(pipeline.vs.as_raw(), ptr::null_mut(), 0);

                if self.dirty_flag.contains(DirtyStateFlag::PIPELINE_PS) {
                    let ps = pipeline.ps.as_ref().map_or(ptr::null_mut(), |ps| ps.as_raw());
                    context.PSSetShader(ps, ptr::null_mut(), 0);

                    self.dirty_flag.remove(DirtyStateFlag::PIPELINE_PS)
                }

                if self.dirty_flag.contains(DirtyStateFlag::PIPELINE_GS) {
                    let gs = pipeline.gs.as_ref().map_or(ptr::null_mut(), |gs| gs.as_raw());
                    context.GSSetShader(gs, ptr::null_mut(), 0);

                    self.dirty_flag.remove(DirtyStateFlag::PIPELINE_GS)
                }

                if self.dirty_flag.contains(DirtyStateFlag::PIPELINE_HS) {
                    let hs = pipeline.hs.as_ref().map_or(ptr::null_mut(), |hs| hs.as_raw());
                    context.HSSetShader(hs, ptr::null_mut(), 0);

                    self.dirty_flag.remove(DirtyStateFlag::PIPELINE_HS)
                }

                if self.dirty_flag.contains(DirtyStateFlag::PIPELINE_DS) {
                    let ds = pipeline.ds.as_ref().map_or(ptr::null_mut(), |ds| ds.as_raw());
                    context.DSSetShader(ds, ptr::null_mut(), 0);

                    self.dirty_flag.remove(DirtyStateFlag::PIPELINE_DS)
                }

                context.RSSetState(pipeline.rasterizer_state.as_raw());
                if let Some(ref viewport) = pipeline.baked_states.viewport {
                    context.RSSetViewports(1, [conv::map_viewport(&viewport)].as_ptr());
                }
                if let Some(ref scissor) = pipeline.baked_states.scissor {
                    context.RSSetScissorRects(1, [conv::map_rect(&scissor)].as_ptr());
                }

                self.current_blend = Some(pipeline.blend_state.as_raw());
            }
        };

        self.bind_blend_state(context);
        self.bind_depth_stencil_state(context);

        self.dirty_flag.remove(DirtyStateFlag::GRAPHICS_PIPELINE);
    }

    pub fn bind(&mut self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        self.bind_render_targets(context);
        self.bind_graphics_pipeline(context);
        self.bind_vertex_buffers(context);
        self.bind_viewports(context);
    }
}

type PerConstantBufferVec<T> = ArrayVec<[T; d3d11::D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT as _]>;

fn generate_graphics_dynamic_constant_buffer_offsets<'a>(
    bindings: impl IntoIterator<Item = &'a pso::DescriptorSetLayoutBinding>,
    offset_iter: &mut impl Iterator<Item = u32>,
    context1_some: bool,
) -> (PerConstantBufferVec<UINT>, PerConstantBufferVec<UINT>) {
    let mut vs_offsets = ArrayVec::new();
    let mut fs_offsets = ArrayVec::new();

    let mut exists_dynamic_constant_buffer = false;

    for binding in bindings.into_iter() {
        match binding.ty {
            pso::DescriptorType::Buffer {
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: true
                },
                ty: pso::BufferDescriptorType::Uniform,
            } => {
                let offset = offset_iter.next().unwrap();

                if binding.stage_flags.contains(pso::ShaderStageFlags::VERTEX) {
                    vs_offsets.push(offset / 16)
                };

                if binding.stage_flags.contains(pso::ShaderStageFlags::FRAGMENT) {
                    fs_offsets.push(offset / 16)
                };
                exists_dynamic_constant_buffer = true;
            }
            pso::DescriptorType::Buffer {
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: false
                },
                ty: pso::BufferDescriptorType::Uniform,
            } => {
                if binding.stage_flags.contains(pso::ShaderStageFlags::VERTEX) {
                    vs_offsets.push(0)
                };

                if binding.stage_flags.contains(pso::ShaderStageFlags::FRAGMENT) {
                    fs_offsets.push(0)
                };
            }
            pso::DescriptorType::Buffer {
                ty: pso::BufferDescriptorType::Storage { .. },
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: true
                },
            } => {
                // TODO: Storage buffer offsets require new buffer views with correct sizes.
                //       Might also require D3D11_BUFFEREX_SRV to act like RBA is happening.
                let _ = offset_iter.next().unwrap();
                warn!("Dynamic offsets into storage buffers are currently unsupported on DX11.");
            }
            _ => {}
        }
    }

    if exists_dynamic_constant_buffer && !context1_some {
        warn!("D3D11.1 runtime required for dynamic offsets into constant buffers. Offsets will be ignored.");
    }

    (vs_offsets, fs_offsets)
}

fn generate_compute_dynamic_constant_buffer_offsets<'a>(
    bindings: impl IntoIterator<Item = &'a pso::DescriptorSetLayoutBinding>,
    offset_iter: &mut impl Iterator<Item = u32>,
    context1_some: bool,
) -> PerConstantBufferVec<UINT> {
    let mut cs_offsets = ArrayVec::new();

    let mut exists_dynamic_constant_buffer = false;

    for binding in bindings.into_iter() {
        match binding.ty {
            pso::DescriptorType::Buffer {
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: true
                },
                ty: pso::BufferDescriptorType::Uniform,
            } => {
                let offset = offset_iter.next().unwrap();

                if binding.stage_flags.contains(pso::ShaderStageFlags::COMPUTE) {
                    cs_offsets.push(offset / 16)
                };

                exists_dynamic_constant_buffer = true;
            }
            pso::DescriptorType::Buffer {
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: false
                },
                ty: pso::BufferDescriptorType::Uniform,
            } => {
                if binding.stage_flags.contains(pso::ShaderStageFlags::COMPUTE) {
                    cs_offsets.push(0)
                };
            }
            pso::DescriptorType::Buffer {
                ty: pso::BufferDescriptorType::Storage { .. },
                format: pso::BufferDescriptorFormat::Structured {
                    dynamic_offset: true
                },
            } => {
                // TODO: Storage buffer offsets require new buffer views with correct sizes.
                //       Might also require D3D11_BUFFEREX_SRV to act like RBA is happening.
                let _ = offset_iter.next().unwrap();
                warn!("Dynamic offsets into storage buffers are currently unsupported on DX11.");
            }
            _ => {}
        }
    }

    if exists_dynamic_constant_buffer && !context1_some {
        warn!("D3D11.1 runtime required for dynamic offsets into constant buffers. Offsets will be ignored.");
    }

    cs_offsets
}

pub struct CommandBuffer {
    internal: Arc<internal::Internal>,
    context: ComPtr<d3d11::ID3D11DeviceContext>,
    context1: Option<ComPtr<d3d11_1::ID3D11DeviceContext1>>,
    list: RefCell<Option<ComPtr<d3d11::ID3D11CommandList>>>,

    // since coherent memory needs to be synchronized at submission, we need to gather up all
    // coherent resources that are used in the command buffer and flush/invalidate them accordingly
    // before executing.
    flush_coherent_memory: Vec<MemoryFlush>,
    invalidate_coherent_memory: Vec<MemoryInvalidate>,

    // holds information about the active render pass
    render_pass_cache: Option<RenderPassCache>,

    // Have to update entire push constant buffer at once, keep whole buffer data local.
    push_constant_data: [u32; MAX_PUSH_CONSTANT_SIZE / 4],
    push_constant_buffer: ComPtr<d3d11::ID3D11Buffer>,

    cache: CommandBufferState,

    one_time_submit: bool,

    debug_name: Option<String>,
    debug_scopes: Vec<Option<debug::DebugScope>>,
}

impl fmt::Debug for CommandBuffer {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("CommandBuffer")
    }
}

unsafe impl Send for CommandBuffer {}
unsafe impl Sync for CommandBuffer {}

impl CommandBuffer {
    fn create_deferred(
        device: &d3d11::ID3D11Device,
        device1: Option<&d3d11_1::ID3D11Device1>,
        internal: Arc<internal::Internal>,
    ) -> Self {
        let (context, context1) = if let Some(device1) = device1 {
            let mut context1: *mut d3d11_1::ID3D11DeviceContext1 = ptr::null_mut();
            let hr =
                unsafe { device1.CreateDeferredContext1(0, &mut context1 as *mut *mut _) };
            assert_eq!(hr, winerror::S_OK);

            let context1 = unsafe { ComPtr::from_raw(context1) };
            let context = context1.cast::<d3d11::ID3D11DeviceContext>().unwrap();

            (context, Some(context1))
        } else {
            let mut context: *mut d3d11::ID3D11DeviceContext = ptr::null_mut();
            let hr =
                unsafe { device.CreateDeferredContext(0, &mut context as *mut *mut _) };
            assert_eq!(hr, winerror::S_OK);

            let context = unsafe { ComPtr::from_raw(context) };

            (context, None)
        };

        let push_constant_buffer = {
            let desc = d3d11::D3D11_BUFFER_DESC {
                ByteWidth: MAX_PUSH_CONSTANT_SIZE as _,
                Usage: d3d11::D3D11_USAGE_DEFAULT,
                BindFlags: d3d11::D3D11_BIND_CONSTANT_BUFFER,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                StructureByteStride: 0,
            };

            let mut buffer: *mut d3d11::ID3D11Buffer = ptr::null_mut();
            let hr = unsafe {
                device.CreateBuffer(&desc as *const _, ptr::null_mut(), &mut buffer as *mut _)
            };

            assert_eq!(hr, winerror::S_OK);

            unsafe { ComPtr::from_raw(buffer) }
        };

        let push_constant_data = [0_u32; 64];

        CommandBuffer {
            internal,
            context,
            context1,
            list: RefCell::new(None),
            flush_coherent_memory: Vec::new(),
            invalidate_coherent_memory: Vec::new(),
            render_pass_cache: None,
            push_constant_data,
            push_constant_buffer,
            cache: CommandBufferState::new(),
            one_time_submit: false,
            debug_name: None,
            debug_scopes: Vec::new(),
        }
    }

    fn as_raw_list(&self) -> ComPtr<d3d11::ID3D11CommandList> {
        if self.one_time_submit {
            self.list.replace(None).unwrap()
        } else {
            self.list.borrow().clone().unwrap()
        }
    }

    fn defer_coherent_flush(&mut self, buffer: &Buffer) {
        if !self
            .flush_coherent_memory
            .iter()
            .any(|m| m.buffer == buffer.internal.raw)
        {
            self.flush_coherent_memory.push(MemoryFlush {
                host_memory: buffer.memory_ptr,
                sync_range: SyncRange::Whole,
                buffer: buffer.internal.raw,
            });
        }
    }

    fn defer_coherent_invalidate(&mut self, buffer: &Buffer) {
        if !self
            .invalidate_coherent_memory
            .iter()
            .any(|m| m.buffer == buffer.internal.raw)
        {
            self.invalidate_coherent_memory.push(MemoryInvalidate {
                working_buffer: Some(self.internal.working_buffer.clone()),
                working_buffer_size: self.internal.working_buffer_size,
                host_memory: buffer.memory_ptr,
                host_sync_range: buffer.bound_range.clone(),
                buffer_sync_range: buffer.bound_range.clone(),
                buffer: buffer.internal.raw,
            });
        }
    }

    fn reset(&mut self) {
        self.flush_coherent_memory.clear();
        self.invalidate_coherent_memory.clear();
        self.render_pass_cache = None;
        self.cache.clear();
        self.debug_scopes.clear();
    }
}

impl command::CommandBuffer<Backend> for CommandBuffer {
    unsafe fn begin(
        &mut self,
        flags: command::CommandBufferFlags,
        _info: command::CommandBufferInheritanceInfo<Backend>,
    ) {
        self.one_time_submit = flags.contains(command::CommandBufferFlags::ONE_TIME_SUBMIT);
        self.reset();

        // Push constants are at the top register to allow them to be bound only once.
        let raw_push_constant_buffer = self.push_constant_buffer.as_raw();
        self.context.VSSetConstantBuffers(
            d3d11::D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT - 1,
            1,
            &raw_push_constant_buffer as *const _
        );
        self.context.PSSetConstantBuffers(
            d3d11::D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT - 1,
            1,
            &raw_push_constant_buffer as *const _
        );
        self.context.CSSetConstantBuffers(
            d3d11::D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT - 1,
            1,
            &raw_push_constant_buffer as *const _
        );
    }

    unsafe fn finish(&mut self) {
        let mut list: *mut d3d11::ID3D11CommandList = ptr::null_mut();
        let hr = self
            .context
            .FinishCommandList(FALSE, &mut list as *mut *mut _);
        assert_eq!(hr, winerror::S_OK);

        if let Some(ref name) = self.debug_name {
            set_debug_name(&*list, name);
        }

        self.list.replace(Some(ComPtr::from_raw(list)));
    }

    unsafe fn reset(&mut self, _release_resources: bool) {
        self.reset();
    }

    unsafe fn begin_render_pass<T>(
        &mut self,
        render_pass: &RenderPass,
        framebuffer: &Framebuffer,
        target_rect: pso::Rect,
        clear_values: T,
        _first_subpass: command::SubpassContents,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ClearValue>,
    {
        use pass::AttachmentLoadOp as Alo;

        let mut clear_iter = clear_values.into_iter();
        let mut attachment_clears = Vec::new();

        for (idx, attachment) in render_pass.attachments.iter().enumerate() {
            //let attachment = render_pass.attachments[attachment_ref];
            let format = attachment.format.unwrap();

            let subpass_id = render_pass
                .subpasses
                .iter()
                .position(|sp| sp.is_using(idx))
                .map(|i| i as pass::SubpassId);

            if attachment.has_clears() {
                let value = *clear_iter.next().unwrap().borrow();

                match (attachment.ops.load, attachment.stencil_ops.load) {
                    (Alo::Clear, Alo::Clear) if format.is_depth() => {
                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::DepthStencil {
                                depth: Some(value.depth_stencil.depth),
                                stencil: Some(value.depth_stencil.stencil),
                            },
                        });
                    }
                    (Alo::Clear, Alo::Clear) => {
                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::Color {
                                index: idx,
                                value: value.color,
                            },
                        });

                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::DepthStencil {
                                depth: None,
                                stencil: Some(value.depth_stencil.stencil),
                            },
                        });
                    }
                    (Alo::Clear, _) if format.is_depth() => {
                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::DepthStencil {
                                depth: Some(value.depth_stencil.depth),
                                stencil: None,
                            },
                        });
                    }
                    (Alo::Clear, _) => {
                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::Color {
                                index: idx,
                                value: value.color,
                            },
                        });
                    }
                    (_, Alo::Clear) => {
                        attachment_clears.push(AttachmentClear {
                            subpass_id,
                            attachment_id: idx,
                            raw: command::AttachmentClear::DepthStencil {
                                depth: None,
                                stencil: Some(value.depth_stencil.stencil),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        self.render_pass_cache = Some(RenderPassCache {
            render_pass: render_pass.clone(),
            framebuffer: framebuffer.clone(),
            attachment_clear_values: attachment_clears,
            target_rect,
            current_subpass: 0,
        });

        if let Some(ref mut current_render_pass) = self.render_pass_cache {
            current_render_pass.start_subpass(&self.internal, &self.context, &mut self.cache);
        }
    }

    unsafe fn next_subpass(&mut self, _contents: command::SubpassContents) {
        if let Some(ref mut current_render_pass) = self.render_pass_cache {
            current_render_pass.next_subpass(&self.context);
            current_render_pass.start_subpass(&self.internal, &self.context, &mut self.cache);
        }
    }

    unsafe fn end_render_pass(&mut self) {
        if let Some(ref mut current_render_pass) = self.render_pass_cache {
            current_render_pass.resolve_msaa(&self.context);
        }

        self.context
            .OMSetRenderTargets(8, [ptr::null_mut(); 8].as_ptr(), ptr::null_mut());

        self.render_pass_cache = None;
    }

    unsafe fn pipeline_barrier<'a, T>(
        &mut self,
        _stages: Range<pso::PipelineStage>,
        _dependencies: memory::Dependencies,
        _barriers: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<memory::Barrier<'a, Backend>>,
    {
        // TODO: should we track and assert on resource states?
        // unimplemented!()
    }

    unsafe fn clear_image<T>(
        &mut self,
        image: &Image,
        _: image::Layout,
        value: command::ClearValue,
        subresource_ranges: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<image::SubresourceRange>,
    {
        for range in subresource_ranges {
            let range = range.borrow();
            let num_levels = range.resolve_level_count(image.mip_levels);
            let num_layers = range.resolve_layer_count(image.kind.num_layers());

            let mut depth_stencil_flags = 0;
            if range.aspects.contains(format::Aspects::DEPTH) {
                depth_stencil_flags |= d3d11::D3D11_CLEAR_DEPTH;
            }
            if range.aspects.contains(format::Aspects::STENCIL) {
                depth_stencil_flags |= d3d11::D3D11_CLEAR_STENCIL;
            }

            // TODO: clear Int/Uint depending on format
            for rel_layer in 0..num_layers {
                for rel_level in 0..num_levels {
                    let level = range.level_start + rel_level;
                    let layer = range.layer_start + rel_layer;
                    if range.aspects.contains(format::Aspects::COLOR) {
                        self.context.ClearRenderTargetView(
                            image.get_rtv(level, layer).unwrap().as_raw(),
                            &value.color.float32,
                        );
                    } else {
                        self.context.ClearDepthStencilView(
                            image.get_dsv(level, layer).unwrap().as_raw(),
                            depth_stencil_flags,
                            value.depth_stencil.depth,
                            value.depth_stencil.stencil as _,
                        );
                    }
                }
            }
        }
    }

    unsafe fn clear_attachments<T, U>(&mut self, clears: T, rects: U)
    where
        T: IntoIterator,
        T::Item: Borrow<command::AttachmentClear>,
        U: IntoIterator,
        U::Item: Borrow<pso::ClearRect>,
    {
        if let Some(ref pass) = self.render_pass_cache {
            self.cache.dirty_flag.insert(
                DirtyStateFlag::GRAPHICS_PIPELINE
                    | DirtyStateFlag::DEPTH_STENCIL_STATE
                    | DirtyStateFlag::PIPELINE_PS
                    | DirtyStateFlag::VIEWPORTS
                    | DirtyStateFlag::RENDER_TARGETS_AND_UAVS,
            );
            self.internal
                .clear_attachments(&self.context, clears, rects, pass);
            self.cache.bind(&self.context);
        } else {
            panic!("`clear_attachments` can only be called inside a renderpass")
        }
    }

    unsafe fn resolve_image<T>(
        &mut self,
        _src: &Image,
        _src_layout: image::Layout,
        _dst: &Image,
        _dst_layout: image::Layout,
        _regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ImageResolve>,
    {
        unimplemented!()
    }

    unsafe fn blit_image<T>(
        &mut self,
        src: &Image,
        _src_layout: image::Layout,
        dst: &Image,
        _dst_layout: image::Layout,
        filter: image::Filter,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ImageBlit>,
    {
        self.cache
            .dirty_flag
            .insert(DirtyStateFlag::GRAPHICS_PIPELINE | DirtyStateFlag::PIPELINE_PS);

        self.internal
            .blit_2d_image(&self.context, src, dst, filter, regions);

        self.cache.bind(&self.context);
    }

    unsafe fn bind_index_buffer(&mut self, ibv: buffer::IndexBufferView<Backend>) {
        self.context.IASetIndexBuffer(
            ibv.buffer.internal.raw,
            conv::map_index_type(ibv.index_type),
            ibv.range.offset as u32,
        );
    }

    unsafe fn bind_vertex_buffers<I, T>(&mut self, first_binding: pso::BufferIndex, buffers: I)
    where
        I: IntoIterator<Item = (T, buffer::SubRange)>,
        T: Borrow<Buffer>,
    {
        for (i, (buf, sub)) in buffers.into_iter().enumerate() {
            let idx = i + first_binding as usize;
            let buf = buf.borrow();

            if buf.is_coherent {
                self.defer_coherent_flush(buf);
            }

            self.cache
                .set_vertex_buffer(idx, sub.offset as u32, buf.internal.raw);
        }

        self.cache.bind_vertex_buffers(&self.context);
    }

    unsafe fn set_viewports<T>(&mut self, _first_viewport: u32, viewports: T)
    where
        T: IntoIterator,
        T::Item: Borrow<pso::Viewport>,
    {
        let viewports = viewports
            .into_iter()
            .map(|v| {
                let v = v.borrow();
                conv::map_viewport(v)
            })
            .collect::<Vec<_>>();

        // TODO: DX only lets us set all VPs at once, so cache in slice?
        self.cache.set_viewports(&viewports);
        self.cache.bind_viewports(&self.context);
    }

    unsafe fn set_scissors<T>(&mut self, _first_scissor: u32, scissors: T)
    where
        T: IntoIterator,
        T::Item: Borrow<pso::Rect>,
    {
        let scissors = scissors
            .into_iter()
            .map(|s| {
                let s = s.borrow();
                conv::map_rect(s)
            })
            .collect::<Vec<_>>();

        // TODO: same as for viewports
        self.context
            .RSSetScissorRects(scissors.len() as _, scissors.as_ptr());
    }

    unsafe fn set_blend_constants(&mut self, color: pso::ColorValue) {
        self.cache.set_blend_factor(color);
        self.cache.bind_blend_state(&self.context);
    }

    unsafe fn set_stencil_reference(&mut self, _faces: pso::Face, value: pso::StencilValue) {
        self.cache.set_stencil_ref(value);
        self.cache.bind_depth_stencil_state(&self.context);
    }

    unsafe fn set_stencil_read_mask(&mut self, _faces: pso::Face, value: pso::StencilValue) {
        self.cache.stencil_read_mask = Some(value);
    }

    unsafe fn set_stencil_write_mask(&mut self, _faces: pso::Face, value: pso::StencilValue) {
        self.cache.stencil_write_mask = Some(value);
    }

    unsafe fn set_depth_bounds(&mut self, _bounds: Range<f32>) {
        unimplemented!()
    }

    unsafe fn set_line_width(&mut self, width: f32) {
        validate_line_width(width);
    }

    unsafe fn set_depth_bias(&mut self, _depth_bias: pso::DepthBias) {
        // TODO:
        // unimplemented!()
    }

    unsafe fn bind_graphics_pipeline(&mut self, pipeline: &GraphicsPipeline) {
        self.cache.set_graphics_pipeline(pipeline.clone());
        self.cache.bind(&self.context);
    }

    unsafe fn bind_graphics_descriptor_sets<'a, I, J>(
        &mut self,
        layout: &PipelineLayout,
        first_set: usize,
        sets: I,
        offsets: J,
    ) where
        I: IntoIterator,
        I::Item: Borrow<DescriptorSet>,
        J: IntoIterator,
        J::Item: Borrow<command::DescriptorSetOffset>,
    {
        let _scope = debug_scope!(&self.context, "BindGraphicsDescriptorSets");

        // TODO: find a better solution to invalidating old bindings..
        let nulls = [ptr::null_mut(); d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT as usize];
        self.context.CSSetUnorderedAccessViews(
            0,
            d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT,
            nulls.as_ptr(),
            ptr::null_mut(),
        );

        let mut offset_iter = offsets.into_iter().map(|o: J::Item| *o.borrow());

        for (set, info) in sets.into_iter().zip(&layout.sets[first_set..]) {
            let set: &DescriptorSet = set.borrow();

            {
                let coherent_buffers = set.coherent_buffers.lock();
                for sync in coherent_buffers.flush_coherent_buffers.borrow().iter() {
                    // TODO: merge sync range if a flush already exists
                    if !self
                        .flush_coherent_memory
                        .iter()
                        .any(|m| m.buffer == sync.device_buffer)
                    {
                        self.flush_coherent_memory.push(MemoryFlush {
                            host_memory: sync.host_ptr,
                            sync_range: sync.range.clone(),
                            buffer: sync.device_buffer,
                        });
                    }
                }

                for sync in coherent_buffers.invalidate_coherent_buffers.borrow().iter() {
                    if !self
                        .invalidate_coherent_memory
                        .iter()
                        .any(|m| m.buffer == sync.device_buffer)
                    {
                        self.invalidate_coherent_memory.push(MemoryInvalidate {
                            working_buffer: Some(self.internal.working_buffer.clone()),
                            working_buffer_size: self.internal.working_buffer_size,
                            host_memory: sync.host_ptr,
                            host_sync_range: sync.range.clone(),
                            buffer_sync_range: sync.range.clone(),
                            buffer: sync.device_buffer,
                        });
                    }
                }
            }

            let (vs_offsets, fs_offsets) = generate_graphics_dynamic_constant_buffer_offsets(
                &*set.layout.bindings,
                &mut offset_iter,
                self.context1.is_some()
            );

            if let Some(rd) = info.registers.vs.c.as_some() {
                let start_slot = rd.res_index as u32;
                let num_buffers = rd.count as u32;
                let constant_buffers = set.handles.offset(rd.pool_offset as isize);
                if let Some(ref context1) = self.context1 {
                    // This call with offsets won't work right with command list emulation
                    // unless we reset the first and last constant buffers to null.
                    if self.internal.command_list_emulation {
                        let null_cbuf = [ptr::null_mut::<d3d11::ID3D11Buffer>()];
                        context1.VSSetConstantBuffers(start_slot, 1, &null_cbuf as *const _);
                        if num_buffers > 1 {
                            context1.VSSetConstantBuffers(start_slot + num_buffers - 1, 1, &null_cbuf as *const _);
                        }
                    }

                    // TODO: This should be the actual buffer length for RBA purposes,
                    //       but that information isn't easily accessible here.
                    context1.VSSetConstantBuffers1(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _,
                        vs_offsets.as_ptr(),
                        self.internal.constant_buffer_count_buffer.as_ptr(),
                    );
                } else {
                    self.context.VSSetConstantBuffers(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _
                    );
                }
            }
            if let Some(rd) = info.registers.vs.t.as_some() {
                self.context.VSSetShaderResources(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }
            if let Some(rd) = info.registers.vs.s.as_some() {
                self.context.VSSetSamplers(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }

            if let Some(rd) = info.registers.ps.c.as_some() {
                let start_slot = rd.res_index as u32;
                let num_buffers = rd.count as u32;
                let constant_buffers = set.handles.offset(rd.pool_offset as isize);
                if let Some(ref context1) = self.context1 {
                    // This call with offsets won't work right with command list emulation
                    // unless we reset the first and last constant buffers to null.
                    if self.internal.command_list_emulation {
                        let null_cbuf = [ptr::null_mut::<d3d11::ID3D11Buffer>()];
                        context1.PSSetConstantBuffers(start_slot, 1, &null_cbuf as *const _);
                        if num_buffers > 1 {
                            context1.PSSetConstantBuffers(start_slot + num_buffers - 1, 1, &null_cbuf as *const _);
                        }
                    }

                    context1.PSSetConstantBuffers1(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _,
                        fs_offsets.as_ptr(),
                        self.internal.constant_buffer_count_buffer.as_ptr(),
                    );
                } else {
                    self.context.PSSetConstantBuffers(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _
                    );
                }
            }
            if let Some(rd) = info.registers.ps.t.as_some() {
                self.context.PSSetShaderResources(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }
            if let Some(rd) = info.registers.ps.s.as_some() {
                self.context.PSSetSamplers(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }

            // UAVs going to the graphics pipeline are always treated as pixel shader bindings.
            if let Some(rd) = info.registers.ps.u.as_some() {
            	// We bind UAVs in inverse order from the top to prevent invalidation
            	// when the render target count changes.
                for idx in (0..(rd.count)).rev() {
                    let ptr = (*set.handles.offset(rd.pool_offset as isize + idx as isize)).0;
                    let uav_register = d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT - 1 - rd.res_index as u32 - idx as u32;
                    self.cache.uavs[uav_register as usize] = ptr as *mut _;
                }
                self.cache.uav_len = (rd.res_index + rd.count) as u32;
                self.cache.dirty_flag.insert(DirtyStateFlag::RENDER_TARGETS_AND_UAVS);
            }
        }

        self.cache.bind_render_targets(&self.context);
    }

    unsafe fn bind_compute_pipeline(&mut self, pipeline: &ComputePipeline) {
        self.context
            .CSSetShader(pipeline.cs.as_raw(), ptr::null_mut(), 0);
    }

    unsafe fn bind_compute_descriptor_sets<I, J>(
        &mut self,
        layout: &PipelineLayout,
        first_set: usize,
        sets: I,
        offsets: J,
    ) where
        I: IntoIterator,
        I::Item: Borrow<DescriptorSet>,
        J: IntoIterator,
        J::Item: Borrow<command::DescriptorSetOffset>,
    {
        let _scope = debug_scope!(&self.context, "BindComputeDescriptorSets");

        let nulls = [ptr::null_mut(); d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT as usize];
        self.context.CSSetUnorderedAccessViews(
            0,
            d3d11::D3D11_PS_CS_UAV_REGISTER_COUNT,
            nulls.as_ptr(),
            ptr::null_mut(),
        );

        let mut offset_iter = offsets.into_iter().map(|o: J::Item| *o.borrow());

        for (set, info) in sets.into_iter().zip(&layout.sets[first_set..]) {
            let set: &DescriptorSet = set.borrow();

            {
                let coherent_buffers = set.coherent_buffers.lock();
                for sync in coherent_buffers.flush_coherent_buffers.borrow().iter() {
                    if !self
                        .flush_coherent_memory
                        .iter()
                        .any(|m| m.buffer == sync.device_buffer)
                    {
                        self.flush_coherent_memory.push(MemoryFlush {
                            host_memory: sync.host_ptr,
                            sync_range: sync.range.clone(),
                            buffer: sync.device_buffer,
                        });
                    }
                }

                for sync in coherent_buffers.invalidate_coherent_buffers.borrow().iter() {
                    if !self
                        .invalidate_coherent_memory
                        .iter()
                        .any(|m| m.buffer == sync.device_buffer)
                    {
                        self.invalidate_coherent_memory.push(MemoryInvalidate {
                            working_buffer: Some(self.internal.working_buffer.clone()),
                            working_buffer_size: self.internal.working_buffer_size,
                            host_memory: sync.host_ptr,
                            host_sync_range: sync.range.clone(),
                            buffer_sync_range: sync.range.clone(),
                            buffer: sync.device_buffer,
                        });
                    }
                }
            }

            let cs_offsets = generate_compute_dynamic_constant_buffer_offsets(
                &*set.layout.bindings,
                &mut offset_iter,
                self.context1.is_some()
            );

            if let Some(rd) = info.registers.cs.c.as_some() {
                let start_slot = rd.res_index as u32;
                let num_buffers = rd.count as u32;
                let constant_buffers = set.handles.offset(rd.pool_offset as isize);
                if let Some(ref context1) = self.context1 {
                    // This call with offsets won't work right with command list emulation
                    // unless we reset the first and last constant buffers to null.
                    if self.internal.command_list_emulation {
                        let null_cbuf = [ptr::null_mut::<d3d11::ID3D11Buffer>()];
                        context1.CSSetConstantBuffers(start_slot, 1, &null_cbuf as *const _);
                        if num_buffers > 1 {
                            context1.CSSetConstantBuffers(start_slot + num_buffers - 1, 1, &null_cbuf as *const _);
                        }
                    }

                    // TODO: This should be the actual buffer length for RBA purposes,
                    //       but that information isn't easily accessible here.
                    context1.CSSetConstantBuffers1(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _,
                        cs_offsets.as_ptr(),
                        self.internal.constant_buffer_count_buffer.as_ptr(),
                    );
                } else {
                    self.context.CSSetConstantBuffers(
                        start_slot,
                        num_buffers,
                        constant_buffers as *const *mut _
                    );
                }
            }
            if let Some(rd) = info.registers.cs.t.as_some() {
                self.context.CSSetShaderResources(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }
            if let Some(rd) = info.registers.cs.u.as_some() {
                self.context.CSSetUnorderedAccessViews(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                    ptr::null_mut(),
                );
            }
            if let Some(rd) = info.registers.cs.s.as_some() {
                self.context.CSSetSamplers(
                    rd.res_index as u32,
                    rd.count as u32,
                    set.handles.offset(rd.pool_offset as isize) as *const *mut _,
                );
            }
        }
    }

    unsafe fn dispatch(&mut self, count: WorkGroupCount) {
        self.context.Dispatch(count[0], count[1], count[2]);
    }

    unsafe fn dispatch_indirect(&mut self, _buffer: &Buffer, _offset: buffer::Offset) {
        unimplemented!()
    }

    unsafe fn fill_buffer(&mut self, _buffer: &Buffer, _sub: buffer::SubRange, _data: u32) {
        unimplemented!()
    }

    unsafe fn update_buffer(&mut self, _buffer: &Buffer, _offset: buffer::Offset, _data: &[u8]) {
        unimplemented!()
    }

    unsafe fn copy_buffer<T>(&mut self, src: &Buffer, dst: &Buffer, regions: T)
    where
        T: IntoIterator,
        T::Item: Borrow<command::BufferCopy>,
    {
        if src.is_coherent {
            self.defer_coherent_flush(src);
        }

        for region in regions.into_iter() {
            let info = region.borrow();
            let dst_box = d3d11::D3D11_BOX {
                left: info.src as _,
                top: 0,
                front: 0,
                right: (info.src + info.size) as _,
                bottom: 1,
                back: 1,
            };

            self.context.CopySubresourceRegion(
                dst.internal.raw as _,
                0,
                info.dst as _,
                0,
                0,
                src.internal.raw as _,
                0,
                &dst_box,
            );

            if let Some(disjoint_cb) = dst.internal.disjoint_cb {
                self.context.CopySubresourceRegion(
                    disjoint_cb as _,
                    0,
                    info.dst as _,
                    0,
                    0,
                    src.internal.raw as _,
                    0,
                    &dst_box,
                );
            }
        }
    }

    unsafe fn copy_image<T>(
        &mut self,
        src: &Image,
        _: image::Layout,
        dst: &Image,
        _: image::Layout,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::ImageCopy>,
    {
        self.internal
            .copy_image_2d(&self.context, src, dst, regions);
    }

    unsafe fn copy_buffer_to_image<T>(
        &mut self,
        buffer: &Buffer,
        image: &Image,
        _: image::Layout,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::BufferImageCopy>,
    {
        if buffer.is_coherent {
            self.defer_coherent_flush(buffer);
        }

        self.internal
            .copy_buffer_to_image(&self.context, buffer, image, regions);
    }

    unsafe fn copy_image_to_buffer<T>(
        &mut self,
        image: &Image,
        _: image::Layout,
        buffer: &Buffer,
        regions: T,
    ) where
        T: IntoIterator,
        T::Item: Borrow<command::BufferImageCopy>,
    {
        if buffer.is_coherent {
            self.defer_coherent_invalidate(buffer);
        }

        self.internal
            .copy_image_to_buffer(&self.context, image, buffer, regions);
    }

    unsafe fn draw(&mut self, vertices: Range<VertexCount>, instances: Range<InstanceCount>) {
        self.context.DrawInstanced(
            vertices.end - vertices.start,
            instances.end - instances.start,
            vertices.start,
            instances.start,
        );
    }

    unsafe fn draw_indexed(
        &mut self,
        indices: Range<IndexCount>,
        base_vertex: VertexOffset,
        instances: Range<InstanceCount>,
    ) {
        self.context.DrawIndexedInstanced(
            indices.end - indices.start,
            instances.end - instances.start,
            indices.start,
            base_vertex,
            instances.start,
        );
    }

    unsafe fn draw_indirect(
        &mut self,
        buffer: &Buffer,
        offset: buffer::Offset,
        draw_count: DrawCount,
        _stride: u32,
    ) {
        assert_eq!(draw_count, 1, "DX11 doesn't support MULTI_DRAW_INDIRECT");
        self.context.DrawInstancedIndirect(
            buffer.internal.raw,
            offset as _,
        );
    }

    unsafe fn draw_indexed_indirect(
        &mut self,
        buffer: &Buffer,
        offset: buffer::Offset,
        draw_count: DrawCount,
        _stride: u32,
    ) {
        assert_eq!(draw_count, 1, "DX11 doesn't support MULTI_DRAW_INDIRECT");
        self.context.DrawIndexedInstancedIndirect(
            buffer.internal.raw,
            offset as _,
        );
    }

    unsafe fn draw_indirect_count(
        &mut self,
        _buffer: &Buffer,
        _offset: buffer::Offset,
        _count_buffer: &Buffer,
        _count_buffer_offset: buffer::Offset,
        _max_draw_count: u32,
        _stride: u32,
    ) {
        panic!("DX11 doesn't support DRAW_INDIRECT_COUNT")
    }

    unsafe fn draw_indexed_indirect_count(
        &mut self,
        _buffer: &Buffer,
        _offset: buffer::Offset,
        _count_buffer: &Buffer,
        _count_buffer_offset: buffer::Offset,
        _max_draw_count: u32,
        _stride: u32,
    ) {
        panic!("DX11 doesn't support DRAW_INDIRECT_COUNT")
    }

    unsafe fn draw_mesh_tasks(&mut self, _: TaskCount, _: TaskCount) {
        panic!("DX11 doesn't support MESH_SHADERS")
    }

    unsafe fn draw_mesh_tasks_indirect(
        &mut self,
        _: &Buffer,
        _: buffer::Offset,
        _: hal::DrawCount,
        _: u32,
    ) {
        panic!("DX11 doesn't support MESH_SHADERS")
    }

    unsafe fn draw_mesh_tasks_indirect_count(
        &mut self,
        _: &Buffer,
        _: buffer::Offset,
        _: &Buffer,
        _: buffer::Offset,
        _: hal::DrawCount,
        _: u32,
    ) {
        panic!("DX11 doesn't support MESH_SHADERS")
    }

    unsafe fn set_event(&mut self, _: &(), _: pso::PipelineStage) {
        unimplemented!()
    }

    unsafe fn reset_event(&mut self, _: &(), _: pso::PipelineStage) {
        unimplemented!()
    }

    unsafe fn wait_events<'a, I, J>(&mut self, _: I, _: Range<pso::PipelineStage>, _: J)
    where
        I: IntoIterator,
        I::Item: Borrow<()>,
        J: IntoIterator,
        J::Item: Borrow<memory::Barrier<'a, Backend>>,
    {
        unimplemented!()
    }

    unsafe fn begin_query(&mut self, _query: query::Query<Backend>, _flags: query::ControlFlags) {
        unimplemented!()
    }

    unsafe fn end_query(&mut self, _query: query::Query<Backend>) {
        unimplemented!()
    }

    unsafe fn reset_query_pool(&mut self, _pool: &QueryPool, _queries: Range<query::Id>) {
        unimplemented!()
    }

    unsafe fn copy_query_pool_results(
        &mut self,
        _pool: &QueryPool,
        _queries: Range<query::Id>,
        _buffer: &Buffer,
        _offset: buffer::Offset,
        _stride: buffer::Offset,
        _flags: query::ResultFlags,
    ) {
        unimplemented!()
    }

    unsafe fn write_timestamp(&mut self, _: pso::PipelineStage, _query: query::Query<Backend>) {
        unimplemented!()
    }

    unsafe fn push_graphics_constants(
        &mut self,
        _layout: &PipelineLayout,
        _stages: pso::ShaderStageFlags,
        offset: u32,
        constants: &[u32],
    ) {
        let start = (offset / 4) as usize;
        let end = start + constants.len();

        self.push_constant_data[start..end].copy_from_slice(constants);

        self.context.UpdateSubresource(
            self.push_constant_buffer.as_raw() as *mut _,
            0,
            ptr::null(),
            self.push_constant_data.as_ptr() as *const _,
            MAX_PUSH_CONSTANT_SIZE as _,
            1,
        );
    }

    unsafe fn push_compute_constants(
        &mut self,
        _layout: &PipelineLayout,
        offset: u32,
        constants: &[u32],
    ) {
        let start = (offset / 4) as usize;
        let end = start + constants.len();

        self.push_constant_data[start..end].copy_from_slice(constants);

        self.context.UpdateSubresource(
            self.push_constant_buffer.as_raw() as *mut _,
            0,
            ptr::null(),
            self.push_constant_data.as_ptr() as *const _,
            MAX_PUSH_CONSTANT_SIZE as _,
            1,
        );
    }

    unsafe fn execute_commands<'a, T, I>(&mut self, _buffers: I)
    where
        T: 'a + Borrow<CommandBuffer>,
        I: IntoIterator<Item = &'a T>,
    {
        unimplemented!()
    }

    unsafe fn insert_debug_marker(&mut self, name: &str, _color: u32) {
        debug::debug_marker(&self.context, &format!("{}", name))
    }
    unsafe fn begin_debug_marker(&mut self, _name: &str, _color: u32) {
        // TODO: This causes everything after this to be part of this scope, why?
        // self.debug_scopes.push(debug::DebugScope::with_name(&self.context, format_args!("{}", name)))
    }
    unsafe fn end_debug_marker(&mut self) {
        // self.debug_scopes.pop();
    }
}

#[derive(Clone, Debug)]
enum SyncRange {
    Whole,
    Partial(Range<u64>),
}

#[derive(Debug)]
pub struct MemoryFlush {
    host_memory: *const u8,
    sync_range: SyncRange,
    buffer: *mut d3d11::ID3D11Buffer,
}

pub struct MemoryInvalidate {
    working_buffer: Option<ComPtr<d3d11::ID3D11Buffer>>,
    working_buffer_size: u64,
    host_memory: *mut u8,
    host_sync_range: Range<u64>,
    buffer_sync_range: Range<u64>,
    buffer: *mut d3d11::ID3D11Buffer,
}

impl fmt::Debug for MemoryInvalidate {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("MemoryInvalidate")
    }
}

fn intersection(a: &Range<u64>, b: &Range<u64>) -> Option<Range<u64>> {
    let r = a.start.max(b.start)..a.end.min(b.end);
    if r.start < r.end {
        Some(r)
    } else {
        None
    }
}

impl MemoryFlush {
    fn do_flush(&self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        let src = self.host_memory;

        debug_marker!(context, "Flush({:?})", self.sync_range);
        let region = match self.sync_range {
            SyncRange::Partial(ref range) if range.start < range.end => Some(d3d11::D3D11_BOX {
                left: range.start as u32,
                top: 0,
                front: 0,
                right: range.end as u32,
                bottom: 1,
                back: 1,
            }),
            _ => None,
        };

        unsafe {
            context.UpdateSubresource(
                self.buffer as _,
                0,
                region.as_ref().map_or(ptr::null(), |r| r),
                src as _,
                0,
                0,
            );
        }
    }
}

impl MemoryInvalidate {
    fn download(
        &self,
        context: &ComPtr<d3d11::ID3D11DeviceContext>,
        buffer: *mut d3d11::ID3D11Buffer,
        host_range: Range<u64>,
        buffer_range: Range<u64>
    ) {
        // Range<u64> doesn't impl `len` for some bizzare reason relating to underflow
        debug_assert_eq!(host_range.end - host_range.start, buffer_range.end - buffer_range.start);

        unsafe {
            context.CopySubresourceRegion(
                self.working_buffer.clone().unwrap().as_raw() as _,
                0,
                0,
                0,
                0,
                buffer as _,
                0,
                &d3d11::D3D11_BOX {
                    left: buffer_range.start as _,
                    top: 0,
                    front: 0,
                    right: buffer_range.end as _,
                    bottom: 1,
                    back: 1,
                },
            );

            // copy over to our vec
            let dst = self.host_memory.offset(host_range.start as isize);
            let src = self.map(&context);
            ptr::copy(src, dst, (host_range.end - host_range.start) as usize);
            self.unmap(&context);
        }
    }

    fn do_invalidate(&self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        let stride = self.working_buffer_size;
        let len = self.host_sync_range.end - self.host_sync_range.start;
        let chunks = len / stride;
        let remainder = len % stride;

        // we split up the copies into chunks the size of our working buffer
        for i in 0..chunks {
            let host_offset = self.host_sync_range.start + i * stride;
            let host_range = host_offset..(host_offset + stride);
            let buffer_offset = self.buffer_sync_range.start + i * stride;
            let buffer_range = buffer_offset..(buffer_offset + stride);

            self.download(context, self.buffer, host_range, buffer_range);
        }

        if remainder != 0 {
            let host_offset = self.host_sync_range.start + chunks * stride;
            let host_range = host_offset..self.host_sync_range.end;
            let buffer_offset = self.buffer_sync_range.start + chunks * stride;
            let buffer_range = buffer_offset..self.buffer_sync_range.end;

            debug_assert!(host_range.end - host_range.start <= stride);
            debug_assert!(buffer_range.end - buffer_range.start <= stride);

            self.download(context, self.buffer, host_range, buffer_range);
        }
    }

    fn map(&self, context: &ComPtr<d3d11::ID3D11DeviceContext>) -> *mut u8 {
        assert_eq!(self.working_buffer.is_some(), true);

        unsafe {
            let mut map = mem::zeroed();
            let hr = context.Map(
                self.working_buffer.clone().unwrap().as_raw() as _,
                0,
                d3d11::D3D11_MAP_READ,
                0,
                &mut map,
            );

            assert_eq!(hr, winerror::S_OK);

            map.pData as _
        }
    }

    fn unmap(&self, context: &ComPtr<d3d11::ID3D11DeviceContext>) {
        unsafe {
            context.Unmap(self.working_buffer.clone().unwrap().as_raw() as _, 0);
        }
    }
}

type LocalResourceArena<T> = thunderdome::Arena<(Range<u64>, T)>;

// Since we dont have any heaps to work with directly, Beverytime we bind a
// buffer/image to memory we allocate a dx11 resource and assign it a range.
//
// `HOST_VISIBLE` memory gets a `Vec<u8>` which covers the entire memory
// range. This forces us to only expose non-coherent memory, as this
// abstraction acts as a "cache" since the "staging buffer" vec is disjoint
// from all the dx11 resources we store in the struct.
pub struct Memory {
    properties: memory::Properties,
    size: u64,

    // pointer to staging memory, if it's HOST_VISIBLE
    host_ptr: *mut u8,

    // list of all buffers bound to this memory
    local_buffers: Arc<RwLock<LocalResourceArena<InternalBuffer>>>,
}

impl fmt::Debug for Memory {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Memory")
    }
}

unsafe impl Send for Memory {}
unsafe impl Sync for Memory {}

impl Memory {
    pub fn resolve(&self, segment: &memory::Segment) -> Range<u64> {
        segment.offset..segment.size.map_or(self.size, |s| segment.offset + s)
    }

    pub fn bind_buffer(&self, range: Range<u64>, buffer: InternalBuffer) -> thunderdome::Index {
        let mut local_buffers = self.local_buffers.write();
        local_buffers.insert((range, buffer))
    }

    pub fn flush(&self, context: &ComPtr<d3d11::ID3D11DeviceContext>, range: Range<u64>) {
        use buffer::Usage;

        for (_, &(ref buffer_range, ref buffer)) in self.local_buffers.read().iter() {
            let range = match intersection(&range, &buffer_range) {
                Some(r) => r,
                None => continue,
            };
            // we need to handle 3 cases for updating buffers:
            //
            //   1. if our buffer was created as a `UNIFORM` buffer *and* other usage flags, we
            //      also have a disjoint buffer which only has `D3D11_BIND_CONSTANT_BUFFER` due
            //      to DX11 limitation. we then need to update both the original buffer and the
            //      disjoint one with the *whole* range
            //
            //   2. if our buffer was created with *only* `UNIFORM` usage we need to upload
            //      the whole range
            //
            //   3. the general case, without any `UNIFORM` usage has no restrictions on
            //      partial updates, so we upload the specified range
            //
            if let Some(disjoint) = buffer.disjoint_cb {
                MemoryFlush {
                    host_memory: unsafe { self.host_ptr.offset(buffer_range.start as _) },
                    sync_range: SyncRange::Whole,
                    buffer: disjoint,
                }
                .do_flush(&context);
            }

            let mem_flush = if buffer.usage == Usage::UNIFORM {
                MemoryFlush {
                    host_memory: unsafe { self.host_ptr.offset(buffer_range.start as _) },
                    sync_range: SyncRange::Whole,
                    buffer: buffer.raw,
                }
            } else {
                let local_start = range.start - buffer_range.start;
                let local_end = range.end - buffer_range.start;

                MemoryFlush {
                    host_memory: unsafe { self.host_ptr.offset(range.start as _) },
                    sync_range: SyncRange::Partial(local_start..local_end),
                    buffer: buffer.raw,
                }
            };

            mem_flush.do_flush(&context)
        }
    }

    pub fn invalidate(
        &self,
        context: &ComPtr<d3d11::ID3D11DeviceContext>,
        range: Range<u64>,
        working_buffer: ComPtr<d3d11::ID3D11Buffer>,
        working_buffer_size: u64,
    ) {
        for (_, &(ref buffer_range, ref buffer)) in self.local_buffers.read().iter() {
            if let Some(range) = intersection(&range, &buffer_range) {
                let buffer_start_offset = range.start - buffer_range.start;
                let buffer_end_offset = range.end - buffer_range.start;

                let buffer_sync_range = buffer_start_offset..buffer_end_offset;

                MemoryInvalidate {
                    working_buffer: Some(working_buffer.clone()),
                    working_buffer_size,
                    host_memory: self.host_ptr,
                    host_sync_range: range.clone(),
                    buffer_sync_range: buffer_sync_range,
                    buffer: buffer.raw,
                }
                .do_invalidate(&context);
            }
        }
    }
}

#[derive(Debug)]
pub struct CommandPool {
    device: ComPtr<d3d11::ID3D11Device>,
    device1: Option<ComPtr<d3d11_1::ID3D11Device1>>,
    internal: Arc<internal::Internal>,
}

unsafe impl Send for CommandPool {}
unsafe impl Sync for CommandPool {}

impl hal::pool::CommandPool<Backend> for CommandPool {
    unsafe fn reset(&mut self, _release_resources: bool) {
        //unimplemented!()
    }

    unsafe fn allocate_one(&mut self, _level: command::Level) -> CommandBuffer {
        CommandBuffer::create_deferred(&self.device, self.device1.as_deref(), Arc::clone(&self.internal))
    }

    unsafe fn free<I>(&mut self, _cbufs: I)
    where
        I: IntoIterator<Item = CommandBuffer>,
    {
        // TODO:
        // unimplemented!()
    }
}

/// Similarily to dx12 backend, we can handle either precompiled dxbc or spirv
pub enum ShaderModule {
    Dxbc(Vec<u8>),
    Spirv(Vec<u32>),
}

// TODO: temporary
impl fmt::Debug for ShaderModule {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "{}", "ShaderModule { ... }")
    }
}

unsafe impl Send for ShaderModule {}
unsafe impl Sync for ShaderModule {}

#[derive(Clone, Debug)]
pub struct SubpassDesc {
    pub color_attachments: Vec<pass::AttachmentRef>,
    pub depth_stencil_attachment: Option<pass::AttachmentRef>,
    pub input_attachments: Vec<pass::AttachmentRef>,
    pub resolve_attachments: Vec<pass::AttachmentRef>,
}

impl SubpassDesc {
    pub(crate) fn is_using(&self, at_id: pass::AttachmentId) -> bool {
        self.color_attachments
            .iter()
            .chain(self.depth_stencil_attachment.iter())
            .chain(self.input_attachments.iter())
            .chain(self.resolve_attachments.iter())
            .any(|&(id, _)| id == at_id)
    }
}

#[derive(Clone, Debug)]
pub struct RenderPass {
    pub attachments: Vec<pass::Attachment>,
    pub subpasses: Vec<SubpassDesc>,
}

#[derive(Clone, Debug)]
pub struct Framebuffer {
    attachments: Vec<ImageView>,
    layers: image::Layer,
}

#[derive(Clone, Debug)]
pub struct InternalBuffer {
    raw: *mut d3d11::ID3D11Buffer,
    // TODO: need to sync between `raw` and `disjoint_cb`, same way as we do with
    // `MemoryFlush/Invalidate`
    disjoint_cb: Option<*mut d3d11::ID3D11Buffer>, // if unbound this buffer might be null.
    srv: Option<*mut d3d11::ID3D11ShaderResourceView>,
    uav: Option<*mut d3d11::ID3D11UnorderedAccessView>,
    usage: buffer::Usage,
    debug_name: Option<String>,
}

impl InternalBuffer {
    unsafe fn release_resources(&mut self) {
        (&*self.raw).Release();
        self.raw = ptr::null_mut();
        self.disjoint_cb.take().map(|cb| (&*cb).Release());
        self.uav.take().map(|uav| (&*uav).Release());
        self.srv.take().map(|srv| (&*srv).Release());
        self.usage = buffer::Usage::empty();
        self.debug_name = None;
    }
}

pub struct Buffer {
    internal: InternalBuffer,
    is_coherent: bool,
    memory_ptr: *mut u8,     // null if unbound or non-cpu-visible
    bound_range: Range<u64>, // 0 if unbound
    /// Handle to the Memory arena storing this buffer.
    local_memory_arena: Weak<RwLock<LocalResourceArena<InternalBuffer>>>,
    /// Index into the above memory arena.
    ///
    /// Once memory is bound to a buffer, this should never be None.
    memory_index: Option<thunderdome::Index>,
    requirements: memory::Requirements,
    bind: d3d11::D3D11_BIND_FLAG,
}

impl fmt::Debug for Buffer {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Buffer")
    }
}

unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

#[derive(Debug)]
pub struct BufferView;

pub struct Image {
    kind: image::Kind,
    usage: image::Usage,
    format: format::Format,
    view_caps: image::ViewCapabilities,
    decomposed_format: conv::DecomposedDxgiFormat,
    mip_levels: image::Level,
    internal: InternalImage,
    bind: d3d11::D3D11_BIND_FLAG,
    requirements: memory::Requirements,
}

impl fmt::Debug for Image {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Image")
    }
}

pub struct InternalImage {
    raw: *mut d3d11::ID3D11Resource,
    copy_srv: Option<ComPtr<d3d11::ID3D11ShaderResourceView>>,
    srv: Option<ComPtr<d3d11::ID3D11ShaderResourceView>>,

    /// Contains UAVs for all subresources
    unordered_access_views: Vec<ComPtr<d3d11::ID3D11UnorderedAccessView>>,

    /// Contains DSVs for all subresources
    depth_stencil_views: Vec<ComPtr<d3d11::ID3D11DepthStencilView>>,

    /// Contains RTVs for all subresources
    render_target_views: Vec<ComPtr<d3d11::ID3D11RenderTargetView>>,

    debug_name: Option<String>
}

impl InternalImage {
    unsafe fn release_resources(&mut self) {
        (&*self.raw).Release();
        self.copy_srv = None;
        self.srv = None;
        self.unordered_access_views.clear();
        self.depth_stencil_views.clear();
        self.render_target_views.clear();
    }
}

impl fmt::Debug for InternalImage {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("InternalImage")
    }
}

unsafe impl Send for Image {}
unsafe impl Sync for Image {}

impl Image {
    pub fn calc_subresource(&self, mip_level: UINT, layer: UINT) -> UINT {
        mip_level + (layer * self.mip_levels as UINT)
    }

    pub fn get_uav(
        &self,
        mip_level: image::Level,
        _layer: image::Layer,
    ) -> Option<&ComPtr<d3d11::ID3D11UnorderedAccessView>> {
        self.internal
            .unordered_access_views
            .get(self.calc_subresource(mip_level as _, 0) as usize)
    }

    pub fn get_dsv(
        &self,
        mip_level: image::Level,
        layer: image::Layer,
    ) -> Option<&ComPtr<d3d11::ID3D11DepthStencilView>> {
        self.internal
            .depth_stencil_views
            .get(self.calc_subresource(mip_level as _, layer as _) as usize)
    }

    pub fn get_rtv(
        &self,
        mip_level: image::Level,
        layer: image::Layer,
    ) -> Option<&ComPtr<d3d11::ID3D11RenderTargetView>> {
        self.internal
            .render_target_views
            .get(self.calc_subresource(mip_level as _, layer as _) as usize)
    }
}

pub struct ImageView {
    subresource: UINT,
    format: format::Format,
    rtv_handle: Option<*mut d3d11::ID3D11RenderTargetView>,
    srv_handle: Option<*mut d3d11::ID3D11ShaderResourceView>,
    dsv_handle: Option<*mut d3d11::ID3D11DepthStencilView>,
    rodsv_handle: Option<*mut d3d11::ID3D11DepthStencilView>,
    uav_handle: Option<*mut d3d11::ID3D11UnorderedAccessView>,
    owned: bool,
}

impl Clone for ImageView {
    fn clone(&self) -> Self {
        Self {
            subresource: self.subresource,
            format: self.format,
            rtv_handle: self.rtv_handle.clone(),
            srv_handle: self.srv_handle.clone(),
            dsv_handle: self.dsv_handle.clone(),
            rodsv_handle: self.rodsv_handle.clone(),
            uav_handle: self.uav_handle.clone(),
            owned: false
        }
    }
}

impl Drop for ImageView {
    fn drop(&mut self) {
        if self.owned {
            if let Some(rtv) = self.rtv_handle.take() {
                unsafe { (&*rtv).Release() };
            }
            if let Some(srv) = self.srv_handle.take() {
                unsafe { (&*srv).Release() };
            }
            if let Some(dsv) = self.dsv_handle.take() {
                unsafe { (&*dsv).Release() };
            }
            if let Some(rodsv) = self.rodsv_handle.take() {
                unsafe { (&*rodsv).Release() };
            }
            if let Some(uav) = self.uav_handle.take() {
                unsafe { (&*uav).Release() };
            }
        }
    }
}

impl fmt::Debug for ImageView {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("ImageView")
    }
}

unsafe impl Send for ImageView {}
unsafe impl Sync for ImageView {}

pub struct Sampler {
    sampler_handle: ComPtr<d3d11::ID3D11SamplerState>,
}

impl fmt::Debug for Sampler {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Sampler")
    }
}

unsafe impl Send for Sampler {}
unsafe impl Sync for Sampler {}

pub struct ComputePipeline {
    cs: ComPtr<d3d11::ID3D11ComputeShader>,
}

impl fmt::Debug for ComputePipeline {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("ComputePipeline")
    }
}

unsafe impl Send for ComputePipeline {}
unsafe impl Sync for ComputePipeline {}

/// NOTE: some objects are hashed internally and reused when created with the
///       same params[0], need to investigate which interfaces this applies
///       to.
///
/// [0]: https://msdn.microsoft.com/en-us/library/windows/desktop/ff476500(v=vs.85).aspx
#[derive(Clone)]
pub struct GraphicsPipeline {
    vs: ComPtr<d3d11::ID3D11VertexShader>,
    gs: Option<ComPtr<d3d11::ID3D11GeometryShader>>,
    hs: Option<ComPtr<d3d11::ID3D11HullShader>>,
    ds: Option<ComPtr<d3d11::ID3D11DomainShader>>,
    ps: Option<ComPtr<d3d11::ID3D11PixelShader>>,
    topology: d3d11::D3D11_PRIMITIVE_TOPOLOGY,
    input_layout: ComPtr<d3d11::ID3D11InputLayout>,
    rasterizer_state: ComPtr<d3d11::ID3D11RasterizerState>,
    blend_state: ComPtr<d3d11::ID3D11BlendState>,
    depth_stencil_state: Option<DepthStencilState>,
    baked_states: pso::BakedStates,
    required_bindings: u32,
    max_vertex_bindings: u32,
    strides: Vec<u32>,
}

impl fmt::Debug for GraphicsPipeline {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("GraphicsPipeline")
    }
}

unsafe impl Send for GraphicsPipeline {}
unsafe impl Sync for GraphicsPipeline {}

type ResourceIndex = u8;
type DescriptorIndex = u16;

#[derive(Clone, Debug, Default)]
struct RegisterData<T> {
    // CBV
    c: T,
    // SRV
    t: T,
    // UAV
    u: T,
    // Sampler
    s: T,
}

impl<T> RegisterData<T> {
    fn map<U, F: Fn(&T) -> U>(&self, fun: F) -> RegisterData<U> {
        RegisterData {
            c: fun(&self.c),
            t: fun(&self.t),
            u: fun(&self.u),
            s: fun(&self.s),
        }
    }
}

impl RegisterData<DescriptorIndex> {
    fn add_content_many(&mut self, content: DescriptorContent, many: DescriptorIndex) {
        if content.contains(DescriptorContent::CBV) {
            self.c += many;
        }
        if content.contains(DescriptorContent::SRV) {
            self.t += many;
        }
        if content.contains(DescriptorContent::UAV) {
            self.u += many;
        }
        if content.contains(DescriptorContent::SAMPLER) {
            self.s += many;
        }
    }

    fn sum(&self) -> DescriptorIndex {
        self.c + self.t + self.u + self.s
    }
}

#[derive(Clone, Debug, Default)]
struct MultiStageData<T> {
    vs: T,
    ps: T,
    cs: T,
}

impl<T> MultiStageData<T> {
    fn select(self, stage: ShaderStage) -> T {
        match stage {
            ShaderStage::Vertex => self.vs,
            ShaderStage::Fragment => self.ps,
            ShaderStage::Compute => self.cs,
            _ => panic!("Unsupported stage {:?}", stage),
        }
    }
}

impl<T> MultiStageData<RegisterData<T>> {
    fn map_register<U, F: Fn(&T) -> U>(&self, fun: F) -> MultiStageData<RegisterData<U>> {
        MultiStageData {
            vs: self.vs.map(&fun),
            ps: self.ps.map(&fun),
            cs: self.cs.map(&fun),
        }
    }

    fn map_other<U, F: Fn(&RegisterData<T>) -> U>(&self, fun: F) -> MultiStageData<U> {
        MultiStageData {
            vs: fun(&self.vs),
            ps: fun(&self.ps),
            cs: fun(&self.cs),
        }
    }
}

impl MultiStageData<RegisterData<DescriptorIndex>> {
    fn add_content_many(&mut self, content: DescriptorContent, stages: pso::ShaderStageFlags, count: DescriptorIndex) {
        if stages.contains(pso::ShaderStageFlags::VERTEX) {
            self.vs.add_content_many(content, count);
        }
        if stages.contains(pso::ShaderStageFlags::FRAGMENT) {
            self.ps.add_content_many(content, count);
        }
        if stages.contains(pso::ShaderStageFlags::COMPUTE) {
            self.cs.add_content_many(content, count);
        }
    }

    fn sum(&self) -> DescriptorIndex {
        self.vs.sum() + self.ps.sum() + self.cs.sum()
    }
}

#[derive(Clone, Debug, Default)]
struct RegisterPoolMapping {
    offset: DescriptorIndex,
    count: ResourceIndex,
}

#[derive(Clone, Debug, Default)]
struct RegisterInfo {
    res_index: ResourceIndex,
    pool_offset: DescriptorIndex,
    count: ResourceIndex,
}

impl RegisterInfo {
    fn as_some(&self) -> Option<&Self> {
        if self.count == 0 {
            None
        } else {
            Some(self)
        }
    }
}

#[derive(Clone, Debug, Default)]
struct RegisterAccumulator {
    res_index: ResourceIndex,
}

impl RegisterAccumulator {
    fn to_mapping(&self, cur_offset: &mut DescriptorIndex) -> RegisterPoolMapping {
        let offset = *cur_offset;
        *cur_offset += self.res_index as DescriptorIndex;

        RegisterPoolMapping {
            offset,
            count: self.res_index,
        }
    }

    fn advance(&mut self, mapping: &RegisterPoolMapping) -> RegisterInfo {
        let res_index = self.res_index;
        self.res_index += mapping.count;
        RegisterInfo {
            res_index,
            pool_offset: mapping.offset,
            count: mapping.count,
        }
    }
}

impl RegisterData<RegisterAccumulator> {
    fn to_mapping(&self, pool_offset: &mut DescriptorIndex) -> RegisterData<RegisterPoolMapping> {
        RegisterData {
            c: self.c.to_mapping(pool_offset),
            t: self.t.to_mapping(pool_offset),
            u: self.u.to_mapping(pool_offset),
            s: self.s.to_mapping(pool_offset),
        }
    }

    fn advance(
        &mut self,
        mapping: &RegisterData<RegisterPoolMapping>,
    ) -> RegisterData<RegisterInfo> {
        RegisterData {
            c: self.c.advance(&mapping.c),
            t: self.t.advance(&mapping.t),
            u: self.u.advance(&mapping.u),
            s: self.s.advance(&mapping.s),
        }
    }
}

impl MultiStageData<RegisterData<RegisterAccumulator>> {
    fn to_mapping(&self) -> MultiStageData<RegisterData<RegisterPoolMapping>> {
        let mut pool_offset = 0;
        MultiStageData {
            vs: self.vs.to_mapping(&mut pool_offset),
            ps: self.ps.to_mapping(&mut pool_offset),
            cs: self.cs.to_mapping(&mut pool_offset),
        }
    }

    fn advance(
        &mut self,
        mapping: &MultiStageData<RegisterData<RegisterPoolMapping>>,
    ) -> MultiStageData<RegisterData<RegisterInfo>> {
        MultiStageData {
            vs: self.vs.advance(&mapping.vs),
            ps: self.ps.advance(&mapping.ps),
            cs: self.cs.advance(&mapping.cs),
        }
    }
}

#[derive(Clone, Debug)]
struct DescriptorSetInfo {
    bindings: Arc<Vec<pso::DescriptorSetLayoutBinding>>,
    registers: MultiStageData<RegisterData<RegisterInfo>>,
}

impl DescriptorSetInfo {
    fn find_register(
        &self,
        stage: ShaderStage,
        binding_index: pso::DescriptorBinding,
    ) -> (DescriptorContent, RegisterData<ResourceIndex>) {
        let mut res_offsets = self
            .registers
            .map_register(|info| info.res_index as DescriptorIndex)
            .select(stage);
        for binding in self.bindings.iter() {
            if !binding.stage_flags.contains(stage.to_flag()) {
                continue;
            }
            let content = DescriptorContent::from(binding.ty);
            if binding.binding == binding_index {
                return (content, res_offsets.map(|offset| *offset as ResourceIndex));
            }
            res_offsets.add_content_many(content, 1);
        }
        panic!("Unable to find binding {:?}", binding_index);
    }

    fn find_uav_register(
        &self,
        stage: ShaderStage,
        binding_index: pso::DescriptorBinding,
    ) -> (DescriptorContent, RegisterData<ResourceIndex>) {
        // Look only where uavs are stored for that stage.
        let register_stage = if stage == ShaderStage::Compute {
            stage
        } else {
            ShaderStage::Fragment
        };

        let mut res_offsets = self
            .registers
            .map_register(|info| info.res_index as DescriptorIndex)
            .select(register_stage);
        for binding in self.bindings.iter() {
            // We don't care what stage they're in, only if they are UAVs or not.
            let content = DescriptorContent::from(binding.ty);
            if !content.contains(DescriptorContent::UAV) {
                continue;
            }
            if binding.binding == binding_index {
                return (content, res_offsets.map(|offset| *offset as ResourceIndex));
            }
            res_offsets.add_content_many(content, 1);
        }
        panic!("Unable to find binding {:?}", binding_index);
    }
}

/// The pipeline layout holds optimized (less api calls) ranges of objects for all descriptor sets
/// belonging to the pipeline object.
#[derive(Debug)]
pub struct PipelineLayout {
    sets: Vec<DescriptorSetInfo>,
}

/// The descriptor set layout contains mappings from a given binding to the offset in our
/// descriptor pool storage and what type of descriptor it is (combined image sampler takes up two
/// handles).
#[derive(Debug)]
pub struct DescriptorSetLayout {
    bindings: Arc<Vec<pso::DescriptorSetLayoutBinding>>,
    pool_mapping: MultiStageData<RegisterData<RegisterPoolMapping>>,
}

#[derive(Debug)]
struct CoherentBufferFlushRange {
    device_buffer: *mut d3d11::ID3D11Buffer,
    host_ptr: *mut u8,
    range: SyncRange,
}

#[derive(Debug)]
struct CoherentBufferInvalidateRange {
    device_buffer: *mut d3d11::ID3D11Buffer,
    host_ptr: *mut u8,
    range: Range<u64>,
}

#[derive(Debug)]
struct CoherentBuffers {
    // descriptor set writes containing coherent resources go into these vecs and are added to the
    // command buffers own Vec on binding the set.
    flush_coherent_buffers: RefCell<Vec<CoherentBufferFlushRange>>,
    invalidate_coherent_buffers: RefCell<Vec<CoherentBufferInvalidateRange>>,
}

impl CoherentBuffers {
    fn _add_flush(&self, old: *mut d3d11::ID3D11Buffer, buffer: &Buffer) {
        let new = buffer.internal.raw;

        if old != new {
            let mut buffers = self.flush_coherent_buffers.borrow_mut();

            let pos = buffers.iter().position(|sync| old == sync.device_buffer);

            let sync_range = CoherentBufferFlushRange {
                device_buffer: new,
                host_ptr: buffer.memory_ptr,
                range: SyncRange::Whole,
            };

            if let Some(pos) = pos {
                buffers[pos] = sync_range;
            } else {
                buffers.push(sync_range);
            }

            if let Some(disjoint) = buffer.internal.disjoint_cb {
                let pos = buffers
                    .iter()
                    .position(|sync| disjoint == sync.device_buffer);

                let sync_range = CoherentBufferFlushRange {
                    device_buffer: disjoint,
                    host_ptr: buffer.memory_ptr,
                    range: SyncRange::Whole,
                };

                if let Some(pos) = pos {
                    buffers[pos] = sync_range;
                } else {
                    buffers.push(sync_range);
                }
            }
        }
    }

    fn _add_invalidate(&self, old: *mut d3d11::ID3D11Buffer, buffer: &Buffer) {
        let new = buffer.internal.raw;

        if old != new {
            let mut buffers = self.invalidate_coherent_buffers.borrow_mut();

            let pos = buffers.iter().position(|sync| old == sync.device_buffer);

            let sync_range = CoherentBufferInvalidateRange {
                device_buffer: new,
                host_ptr: buffer.memory_ptr,
                range: buffer.bound_range.clone(),
            };

            if let Some(pos) = pos {
                buffers[pos] = sync_range;
            } else {
                buffers.push(sync_range);
            }
        }
    }
}

/// Newtype around a common interface that all bindable resources inherit from.
#[derive(Debug, Copy, Clone)]
#[repr(transparent)]
struct Descriptor(*mut d3d11::ID3D11DeviceChild);

bitflags! {
    /// A set of D3D11 descriptor types that need to be associated
    /// with a single gfx-hal `DescriptorType`.
    #[derive(Default)]
    pub struct DescriptorContent: u8 {
        const CBV = 0x1;
        const SRV = 0x2;
        const UAV = 0x4;
        const SAMPLER = 0x8;
        /// Indicates if the descriptor is a dynamic uniform/storage buffer.
        /// Important as dynamic buffers are implemented as root descriptors.
        const DYNAMIC = 0x10;
    }
}

impl From<pso::DescriptorType> for DescriptorContent {
    fn from(ty: pso::DescriptorType) -> Self {
        use hal::pso::{
            BufferDescriptorFormat as Bdf, BufferDescriptorType as Bdt, DescriptorType as Dt,
            ImageDescriptorType as Idt,
        };
        match ty {
            Dt::Sampler => DescriptorContent::SAMPLER,
            Dt::Image {
                ty: Idt::Sampled { with_sampler: true },
            } => DescriptorContent::SRV | DescriptorContent::SAMPLER,
            Dt::Image {
                ty: Idt::Sampled {
                    with_sampler: false,
                },
            }
            | Dt::InputAttachment => DescriptorContent::SRV,
            Dt::Image {
                ty: Idt::Storage { .. },
            } => DescriptorContent::UAV,
            Dt::Buffer {
                ty: Bdt::Uniform,
                format:
                    Bdf::Structured {
                        dynamic_offset: true,
                    },
            } => DescriptorContent::CBV | DescriptorContent::DYNAMIC,
            Dt::Buffer {
                ty: Bdt::Uniform, ..
            } => DescriptorContent::CBV,
            Dt::Buffer {
                ty: Bdt::Storage { read_only: true },
                format:
                    Bdf::Structured {
                        dynamic_offset: true,
                    },
            } => DescriptorContent::SRV | DescriptorContent::DYNAMIC,
            Dt::Buffer {
                ty: Bdt::Storage { read_only: false },
                format:
                    Bdf::Structured {
                        dynamic_offset: true,
                    },
            } => DescriptorContent::UAV | DescriptorContent::DYNAMIC,
            Dt::Buffer {
                ty: Bdt::Storage { read_only: true },
                ..
            } => DescriptorContent::SRV,
            Dt::Buffer {
                ty: Bdt::Storage { read_only: false },
                ..
            } => DescriptorContent::UAV,
        }
    }
}

pub struct DescriptorSet {
    offset: DescriptorIndex,
    len: DescriptorIndex,
    handles: *mut Descriptor,
    coherent_buffers: Mutex<CoherentBuffers>,
    layout: DescriptorSetLayout,
}

impl fmt::Debug for DescriptorSet {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("DescriptorSet")
    }
}

unsafe impl Send for DescriptorSet {}
unsafe impl Sync for DescriptorSet {}

impl DescriptorSet {
    fn _add_flush(&self, old: *mut d3d11::ID3D11Buffer, buffer: &Buffer) {
        let new = buffer.internal.raw;

        if old != new {
            self.coherent_buffers.lock()._add_flush(old, buffer);
        }
    }

    fn _add_invalidate(&self, old: *mut d3d11::ID3D11Buffer, buffer: &Buffer) {
        let new = buffer.internal.raw;

        if old != new {
            self.coherent_buffers.lock()._add_invalidate(old, buffer);
        }
    }

    unsafe fn assign(&self, offset: DescriptorIndex, value: *mut d3d11::ID3D11DeviceChild) {
        *self.handles.offset(offset as isize) = Descriptor(value);
    }

    unsafe fn assign_stages(
        &self,
        offsets: &MultiStageData<DescriptorIndex>,
        stages: pso::ShaderStageFlags,
        value: *mut d3d11::ID3D11DeviceChild,
    ) {
        if stages.contains(pso::ShaderStageFlags::VERTEX) {
            self.assign(offsets.vs, value);
        }
        if stages.contains(pso::ShaderStageFlags::FRAGMENT) {
            self.assign(offsets.ps, value);
        }
        if stages.contains(pso::ShaderStageFlags::COMPUTE) {
            self.assign(offsets.cs, value);
        }
    }
}

#[derive(Debug)]
pub struct DescriptorPool {
    handles: Vec<Descriptor>,
    allocator: RangeAllocator<DescriptorIndex>,
}

unsafe impl Send for DescriptorPool {}
unsafe impl Sync for DescriptorPool {}

impl DescriptorPool {
    fn with_capacity(size: DescriptorIndex) -> Self {
        DescriptorPool {
            handles: vec![Descriptor(ptr::null_mut()); size as usize],
            allocator: RangeAllocator::new(0..size),
        }
    }
}

impl pso::DescriptorPool<Backend> for DescriptorPool {
    unsafe fn allocate_set(
        &mut self,
        layout: &DescriptorSetLayout,
    ) -> Result<DescriptorSet, pso::AllocationError> {
        let len = layout
            .pool_mapping
            .map_register(|mapping| mapping.count as DescriptorIndex)
            .sum()
            .max(1);

        self.allocator
            .allocate_range(len)
            .map(|range| {
                for handle in &mut self.handles[range.start as usize..range.end as usize] {
                    *handle = Descriptor(ptr::null_mut());
                }

                DescriptorSet {
                    offset: range.start,
                    len,
                    handles: self.handles.as_mut_ptr().offset(range.start as _),
                    coherent_buffers: Mutex::new(CoherentBuffers {
                        flush_coherent_buffers: RefCell::new(Vec::new()),
                        invalidate_coherent_buffers: RefCell::new(Vec::new()),
                    }),
                    layout: DescriptorSetLayout {
                        bindings: Arc::clone(&layout.bindings),
                        pool_mapping: layout.pool_mapping.clone(),
                    },
                }
            })
            .map_err(|_| pso::AllocationError::OutOfPoolMemory)
    }

    unsafe fn free<I>(&mut self, descriptor_sets: I)
    where
        I: IntoIterator<Item = DescriptorSet>,
    {
        for set in descriptor_sets {
            self.allocator
                .free_range(set.offset..(set.offset + set.len))
        }
    }

    unsafe fn reset(&mut self) {
        self.allocator.reset();
    }
}

#[derive(Debug)]
pub struct RawFence {
    mutex: Mutex<bool>,
    condvar: Condvar,
}

pub type Fence = Arc<RawFence>;

#[derive(Debug)]
pub struct Semaphore;
#[derive(Debug)]
pub struct QueryPool;

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum Backend {}
impl hal::Backend for Backend {
    type Instance = Instance;
    type PhysicalDevice = PhysicalDevice;
    type Device = device::Device;
    type Surface = Surface;

    type QueueFamily = QueueFamily;
    type CommandQueue = CommandQueue;
    type CommandBuffer = CommandBuffer;

    type Memory = Memory;
    type CommandPool = CommandPool;

    type ShaderModule = ShaderModule;
    type RenderPass = RenderPass;
    type Framebuffer = Framebuffer;

    type Buffer = Buffer;
    type BufferView = BufferView;
    type Image = Image;

    type ImageView = ImageView;
    type Sampler = Sampler;

    type ComputePipeline = ComputePipeline;
    type GraphicsPipeline = GraphicsPipeline;
    type PipelineLayout = PipelineLayout;
    type PipelineCache = ();
    type DescriptorSetLayout = DescriptorSetLayout;
    type DescriptorPool = DescriptorPool;
    type DescriptorSet = DescriptorSet;

    type Fence = Fence;
    type Semaphore = Semaphore;
    type Event = ();
    type QueryPool = QueryPool;
}

fn validate_line_width(width: f32) {
    // Note from the Vulkan spec:
    // > If the wide lines feature is not enabled, lineWidth must be 1.0
    // Simply assert and no-op because DX11 never exposes `Features::LINE_WIDTH`
    assert_eq!(width, 1.0);
}
