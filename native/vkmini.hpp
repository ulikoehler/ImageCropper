// SPDX-License-Identifier: CC0-1.0
//
// vkmini.hpp — shared helpers for the TechOverflow Vulkan tutorial
// series. Deliberately small: everything the examples share (instance,
// device, memory, buffers, images, one-time commands, shaderc, a
// headless render target with PPM readback) lives here so each post
// can focus on exactly one new aspect. Uses the vulkan-hpp C++ API.
//
// Every example supports two modes:
//   ./app            opens a GLFW window and renders live (where the
//                    aspect applies — compute-only examples stay
//                    headless)
//   ./app --headless out.ppm
//                    renders exactly one frame offscreen and writes a
//                    binary PPM, so a small python script can assert on
//                    actual pixel values.
#pragma once

#include <vulkan/vulkan.hpp>
#include <shaderc/shaderc.hpp>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <stdexcept>
#include <string>
#include <vector>

namespace vkmini {

/// Instance + physical device + logical device + single queue.
/// One queue family for everything (graphics implies compute+transfer
/// on all real-world hardware relevant here — and the tutorial stays
/// free of queue-transfer complexity).
struct Vk {
    vk::UniqueInstance instance;
    vk::PhysicalDevice phys;
    vk::UniqueDevice device;
    vk::Queue queue;
    uint32_t qfam = 0;
    vk::UniqueCommandPool pool;

    /// layer+extension list for the instance; GLFW-required surface
    /// extensions are appended by the caller when needed.
    void createInstance(std::vector<const char*> exts,
                        const char* appName = "vkmini") {
        vk::ApplicationInfo ai{};
        ai.setPApplicationName(appName)
            .setApiVersion(VK_API_VERSION_1_3);
        vk::InstanceCreateInfo ici{};
        ici.setPApplicationInfo(&ai)
            .setPEnabledExtensionNames(exts);
        instance = vk::createInstanceUnique(ici);
    }

    void pickPhysicalDevice() {
        auto devs = instance->enumeratePhysicalDevices();
        if (devs.empty())
            throw std::runtime_error("no Vulkan device");
        phys = devs.front();
        auto p = phys.getProperties();
        printf("device: %s\n", p.deviceName.data());
    }

    /// Logical device with dynamic rendering enabled (the whole series
    /// renders without VkRenderPass objects). `pNextFeatures` may chain
    /// extra VkPhysicalDevice*Features structs (BDA, sync2, …).
    void createDevice(std::vector<const char*> exts = {},
                      void* pNextFeatures = nullptr) {
        auto props = phys.getQueueFamilyProperties();
        for (uint32_t i = 0; i < props.size(); ++i) {
            if (props[i].queueFlags & vk::QueueFlagBits::eGraphics) {
                qfam = i;
                break;
            }
        }
        float prio = 1.0f;
        vk::DeviceQueueCreateInfo qi{};
        qi.setQueueFamilyIndex(qfam)
            .setQueueCount(1)
            .setPQueuePriorities(&prio);
        vk::PhysicalDeviceDynamicRenderingFeatures dyn{};
        dyn.setDynamicRendering(true).setPNext(pNextFeatures);
        vk::DeviceCreateInfo di{};
        di.setQueueCreateInfos(qi)
            .setPEnabledExtensionNames(exts)
            .setPNext(&dyn);
        device = phys.createDeviceUnique(di);
        queue = device->getQueue(qfam, 0);
        pool = device->createCommandPoolUnique(
            {vk::CommandPoolCreateFlagBits::eResetCommandBuffer, qfam});
    }

    // ---------- memory --------------------------------------------------
    uint32_t memoryType(uint32_t bits,
                        vk::MemoryPropertyFlags want) const {
        auto mem = phys.getMemoryProperties();
        for (uint32_t i = 0; i < mem.memoryTypeCount; ++i)
            if ((bits & (1u << i)) &&
                (mem.memoryTypes[i].propertyFlags & want) == want)
                return i;
        throw std::runtime_error("no suitable memory type");
    }

    struct Buffer {
        vk::UniqueBuffer buf;
        vk::UniqueDeviceMemory mem;
        void* mapped = nullptr;
        vk::DeviceSize size = 0;
    };

    Buffer createBuffer(vk::DeviceSize size, vk::BufferUsageFlags usage,
                        vk::MemoryPropertyFlags props,
                        bool map = false) const {
        Buffer o;
        o.size = size;
        o.buf = device->createBufferUnique(
            {{}, size, usage, vk::SharingMode::eExclusive});
        auto req = device->getBufferMemoryRequirements(o.buf.get());
        o.mem = device->allocateMemoryUnique(
            {req.size, memoryType(req.memoryTypeBits, props)});
        device->bindBufferMemory(o.buf.get(), o.mem.get(), 0);
        if (map)
            o.mapped = device->mapMemory(o.mem.get(), 0, size);
        return o;
    }

    struct Image {
        vk::UniqueImage img;
        vk::UniqueDeviceMemory mem;
        vk::UniqueImageView view;
    };

    Image createImage(uint32_t w, uint32_t h, vk::Format fmt,
                      vk::ImageUsageFlags usage,
                      vk::ImageTiling tiling =
                          vk::ImageTiling::eOptimal) const {
        Image o;
        vk::ImageCreateInfo ii{};
        ii.setImageType(vk::ImageType::e2D)
            .setFormat(fmt)
            .setExtent({w, h, 1})
            .setMipLevels(1)
            .setArrayLayers(1)
            .setSamples(vk::SampleCountFlagBits::e1)
            .setTiling(tiling)
            .setUsage(usage)
            .setSharingMode(vk::SharingMode::eExclusive)
            .setInitialLayout(vk::ImageLayout::eUndefined);
        o.img = device->createImageUnique(ii);
        auto req = device->getImageMemoryRequirements(o.img.get());
        o.mem = device->allocateMemoryUnique(
            {req.size,
             memoryType(req.memoryTypeBits,
                        vk::MemoryPropertyFlagBits::eDeviceLocal)});
        device->bindImageMemory(o.img.get(), o.mem.get(), 0);
        vk::ImageViewCreateInfo vi{};
        vi.setImage(o.img.get())
            .setViewType(vk::ImageViewType::e2D)
            .setFormat(fmt)
            .setSubresourceRange(
                {vk::ImageAspectFlagBits::eColor, 0, 1, 0, 1});
        o.view = device->createImageViewUnique(vi);
        return o;
    }

    // ---------- commands ------------------------------------------------
    /// Record `fn(cmd)` on a fresh command buffer, submit, wait idle.
    template <typename F> void oneTime(F&& fn) const {
        auto cmds = device->allocateCommandBuffersUnique(
            {pool.get(), vk::CommandBufferLevel::ePrimary, 1});
        cmds[0]->begin(
            {vk::CommandBufferUsageFlagBits::eOneTimeSubmit});
        fn(cmds[0].get());
        cmds[0]->end();
        vk::SubmitInfo si{};
        si.setCommandBuffers(cmds[0].get());
        queue.submit(si);
        queue.waitIdle();
    }

    // ---------- shaders -------------------------------------------------
    /// Compile GLSL to SPIR-V at runtime via shaderc — keeps every
    /// example single-command buildable (no offline glslc step).
    vk::UniqueShaderModule shader(const char* glsl,
                                  shaderc_shader_kind kind,
                                  const char* name) const {
        shaderc::Compiler cc;
        auto res = cc.CompileGlslToSpv(glsl, kind, name);
        if (res.GetCompilationStatus() !=
            shaderc_compilation_status_success)
            throw std::runtime_error(name + std::string(": ") +
                                     res.GetErrorMessage());
        std::vector<uint32_t> spv(res.cbegin(), res.cend());
        vk::ShaderModuleCreateInfo si{};
        si.setCode(spv);
        return device->createShaderModuleUnique(si);
    }
};

/// Offscreen RGBA8 render target + host-visible readback for the
/// --headless mode. The image sits in eShaderReadOnlyOptimal between
/// frames; helpers transition it to whatever the example needs.
struct Headless {
    Vk* vm;
    Vk::Image color;      // eColorAttachment|eTransferSrc|eSampled|eStorage
    Vk::Buffer readback;
    vk::Extent2D extent;

    void init(Vk& v, uint32_t w, uint32_t h) {
        vm = &v;
        extent = vk::Extent2D{w, h};
        color = v.createImage(
            w, h, vk::Format::eR8G8B8A8Unorm,
            vk::ImageUsageFlagBits::eColorAttachment |
                vk::ImageUsageFlagBits::eTransferSrc |
                vk::ImageUsageFlagBits::eSampled |
                vk::ImageUsageFlagBits::eStorage);
        readback = v.createBuffer(
            vk::DeviceSize(w) * h * 4,
            vk::BufferUsageFlagBits::eTransferDst,
            vk::MemoryPropertyFlagBits::eHostVisible |
                vk::MemoryPropertyFlagBits::eHostCoherent,
            true);
    }

    /// `render(cmd)` runs inside a one-time command buffer bracketed
    /// by transitions UNDEFINED -> renderLayout -> TRANSFER_SRC, after
    /// which the image is copied to `readback`. Single-shot: headless
    /// mode renders exactly one frame for the pixel check.
    template <typename F>
    void render(vk::ImageLayout renderLayout, F&& render) {
        vm->oneTime([&](vk::CommandBuffer c) {
            barrier(c, vk::ImageLayout::eUndefined, renderLayout,
                    vk::AccessFlags{},
                    vk::AccessFlagBits::eColorAttachmentWrite |
                        vk::AccessFlagBits::eShaderWrite |
                        vk::AccessFlagBits::eTransferWrite);
            render(c);
            barrier(c, renderLayout,
                    vk::ImageLayout::eTransferSrcOptimal,
                    vk::AccessFlagBits::eColorAttachmentWrite |
                        vk::AccessFlagBits::eShaderWrite |
                        vk::AccessFlagBits::eTransferWrite,
                    vk::AccessFlagBits::eTransferRead);
            vk::BufferImageCopy r{};
            r.setImageSubresource(
                 {vk::ImageAspectFlagBits::eColor, 0, 0, 1})
                .setImageExtent({extent.width, extent.height, 1});
            c.copyImageToBuffer(color.img.get(),
                                vk::ImageLayout::eTransferSrcOptimal,
                                readback.buf.get(), r);
        });
    }

    void barrier(vk::CommandBuffer c, vk::ImageLayout ol,
                 vk::ImageLayout nl, vk::AccessFlags src,
                 vk::AccessFlags dst) {
        vk::ImageMemoryBarrier b{};
        b.setOldLayout(ol)
            .setNewLayout(nl)
            .setSrcQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
            .setDstQueueFamilyIndex(VK_QUEUE_FAMILY_IGNORED)
            .setImage(color.img.get())
            .setSubresourceRange(
                {vk::ImageAspectFlagBits::eColor, 0, 1, 0, 1})
            .setSrcAccessMask(src)
            .setDstAccessMask(dst);
        c.pipelineBarrier(vk::PipelineStageFlagBits::eAllCommands,
                          vk::PipelineStageFlagBits::eAllCommands, {},
                          {}, {}, b);
    }

    /// Write the readback buffer as binary PPM (P6, no alpha).
    void savePpm(const char* path) const {
        std::ofstream f(path, std::ios::binary);
        f << "P6\n" << extent.width << " " << extent.height << "\n255\n";
        auto* px = static_cast<const uint8_t*>(readback.mapped);
        for (uint32_t y = 0; y < extent.height; ++y)
            for (uint32_t x = 0; x < extent.width; ++x) {
                const uint8_t* p = px + (y * extent.width + x) * 4;
                char rgb[3] = {char(p[0]), char(p[1]), char(p[2])};
                f.write(rgb, 3);
            }
    }
};

/// Swapchain + per-image views, negotiated against the surface caps.
/// Used by windowed mode from post 2 on (post 1 shows the long form).
struct Swapchain {
    vk::UniqueSwapchainKHR sc;
    std::vector<vk::Image> images;
    std::vector<vk::UniqueImageView> views;
    vk::Format format = vk::Format::eB8G8R8A8Unorm;
    vk::Extent2D extent{};

    void create(Vk& v, vk::PhysicalDevice phys, vk::SurfaceKHR surface,
                uint32_t wantW, uint32_t wantH) {
        // recreation: the window can't have two live swapchains —
        // free views + old chain BEFORE creating the new one.
        views.clear();
        sc.reset();
        auto caps = phys.getSurfaceCapabilitiesKHR(surface);
        extent = caps.currentExtent;
        if (extent.width == 0xFFFFFFFFu) { // "surface doesn't care"
            extent.width = std::clamp(
                wantW, caps.minImageExtent.width,
                caps.maxImageExtent.width);
            extent.height = std::clamp(
                wantH, caps.minImageExtent.height,
                caps.maxImageExtent.height);
        }
        format = phys.getSurfaceFormatsKHR(surface).front().format;
        uint32_t n = caps.minImageCount + 1;
        if (caps.maxImageCount) n = std::min(n, caps.maxImageCount);
        vk::SwapchainCreateInfoKHR si{};
        si.setSurface(surface)
            .setMinImageCount(n)
            .setImageFormat(format)
            .setImageColorSpace(vk::ColorSpaceKHR::eSrgbNonlinear)
            .setImageExtent(extent)
            .setImageArrayLayers(1)
            .setImageUsage(vk::ImageUsageFlagBits::eColorAttachment)
            .setImageSharingMode(vk::SharingMode::eExclusive)
            .setPreTransform(caps.currentTransform)
            .setCompositeAlpha(vk::CompositeAlphaFlagBitsKHR::eOpaque)
            .setPresentMode(vk::PresentModeKHR::eFifo)
            .setClipped(VK_TRUE);
        sc = v.device->createSwapchainKHRUnique(si);
        images = v.device->getSwapchainImagesKHR(sc.get());
        for (auto img : images) {
            vk::ImageViewCreateInfo vi{};
            vi.setImage(img)
                .setViewType(vk::ImageViewType::e2D)
                .setFormat(format)
                .setSubresourceRange(
                    {vk::ImageAspectFlagBits::eColor, 0, 1, 0, 1});
            views.push_back(v.device->createImageViewUnique(vi));
        }
    }
};

} // namespace vkmini
