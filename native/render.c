#include "terminal.h"
#include "font.h"

#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>
#include <terminal_vert_spv.h>
#include <terminal_frag_spv.h>
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>

/* The atlas starts at ATLAS_MIN and doubles up to ATLAS_MAX: one descriptor set per size. */
enum { ATLAS_MIN = 512, ATLAS_MAX = 2048, ATLAS_SIZES = 3, GLYPH_SLOTS = 8192 };
enum { UPLOAD_SIZE = 256 * 1024 };

/* Host-visible memory for glyph uploads and quad batches, reused every frame. */
typedef struct Upload Upload;
struct Upload {
    VkBuffer buffer;
    VkDeviceMemory memory;
    unsigned char *mapped;
    VkDeviceSize size, used;
    Upload *next;
};

/* A grown atlas retires the old one until the frame that sampled it completes. */
typedef struct Atlas Atlas;
struct Atlas {
    VkImage image;
    VkDeviceMemory memory;
    VkImageView view;
    VkImageLayout layout;
    Atlas *next;
};

typedef struct {
    size_t length, run_count;
    char *text;
    uint16_t *columns;
    NtRun runs[];
} LineInput;

struct NtLine {
    size_t references;
    LineInput *input;
    NtFontGlyph *glyphs;
    size_t count;
};

typedef struct {
    uint32_t glyph;
    uint16_t x, y, width, height;
    int left, top;
} CachedGlyph;

typedef struct {
    float rect[4], uv[4];
    unsigned char color[4];
    float mode;
} Quad;

enum { OP_CLEAR, OP_CLIP, OP_QUAD, OP_LINE };

typedef struct {
    int kind;
    union {
        uint32_t background;
        float clip[4];
        Quad quad;
        struct { NtLine *line; float x, y; } line;
    };
} Op;

/* The packet the UI thread builds and the render thread replays. */
typedef struct {
    int width, height;
    double scale;
    Op *ops;
    size_t op_count, op_capacity;
} Frame;

struct NtRenderer {
    NtWindow *window;
    Frame frame;
    pthread_t thread;
    sem_t start;
    int done, pending;
    NtFonts *fonts;
    float font_size, cell_width, cell_height;
    double scale;
    VkSurfaceKHR surface;
    VkPhysicalDevice physical;
    VkPhysicalDeviceMemoryProperties memory;
    VkDevice device;
    VkQueue queue;
    VkSwapchainKHR swapchain;
    VkFormat format;
    VkExtent2D extent;
    VkImage *images;
    VkImageView *views;
    VkSemaphore *presented;
    uint32_t image_count, image_index;
    int resize, rendering;
    VkCommandBuffer command;
    VkFence fence;
    VkSemaphore acquired;
    VkPipeline pipeline;
    VkPipelineLayout pipeline_layout;
    VkDescriptorSetLayout descriptor_layout;
    VkDescriptorPool descriptor_pool;
    VkDescriptorSet descriptors;
    VkSampler sampler;
    VkRect2D clip;
    Atlas *atlas, *retired;
    Upload *uploads;
    int atlas_size, atlas_x, atlas_y, atlas_row;
    cairo_scaled_font_t *glyph_fonts[GLYPH_SLOTS];
    CachedGlyph glyphs[GLYPH_SLOTS];
    size_t glyph_count;
    Quad *quads;
    size_t quad_count, quad_capacity;
    char error[512];
};

static int fail(NtRenderer *r, const char *format, ...) {
    if (!r->error[0]) {
        va_list args;
        va_start(args, format);
        vsnprintf(r->error, sizeof r->error, format, args);
        va_end(args);
    }
    return 0;
}

const char *nt_renderer_error(NtRenderer *r) {
    return r->error;
}

#define VK_CHECK(call) do { \
    VkResult vk_result = (call); \
    if (vk_result != VK_SUCCESS) return fail(r, "%s: Vulkan result %d", #call, vk_result); \
} while (0)

static int memory_type(NtRenderer *r, uint32_t bits, VkMemoryPropertyFlags flags,
                       uint32_t *index) {
    for (uint32_t i = 0; i < r->memory.memoryTypeCount; ++i) {
        if ((bits & (1u << i)) && (r->memory.memoryTypes[i].propertyFlags & flags) == flags) {
            *index = i;
            return 1;
        }
    }
    return fail(r, "No Vulkan memory type for flags 0x%x", flags);
}

static void image_barrier(NtRenderer *r, VkImage image,
                          VkImageLayout old_layout, VkImageLayout new_layout,
                          VkPipelineStageFlags source, VkPipelineStageFlags destination,
                          VkAccessFlags source_access, VkAccessFlags destination_access) {
    VkImageMemoryBarrier barrier = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
        .srcAccessMask = source_access, .dstAccessMask = destination_access,
        .oldLayout = old_layout, .newLayout = new_layout,
        .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
        .image = image,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
    };
    vkCmdPipelineBarrier(r->command, source, destination, 0, 0, NULL, 0, NULL, 1, &barrier);
}

static int create_atlas(NtRenderer *r, int size) {
    Atlas *atlas = calloc(1, sizeof *atlas);
    if (r->atlas) {
        r->atlas->next = r->retired;
        r->retired = r->atlas;
    }
    r->atlas = atlas;
    r->atlas_size = size;
    r->descriptors = VK_NULL_HANDLE;
    VkImageCreateInfo image = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = VK_FORMAT_R8G8B8A8_UNORM,
        .extent = {(uint32_t)size, (uint32_t)size, 1},
        .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VK_CHECK(vkCreateImage(r->device, &image, NULL, &atlas->image));
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(r->device, atlas->image, &requirements);
    VkMemoryAllocateInfo memory = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = requirements.size,
    };
    if (!memory_type(r, requirements.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                     &memory.memoryTypeIndex)) return 0;
    VK_CHECK(vkAllocateMemory(r->device, &memory, NULL, &atlas->memory));
    VK_CHECK(vkBindImageMemory(r->device, atlas->image, atlas->memory, 0));
    VkImageViewCreateInfo view = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = atlas->image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = image.format,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
    };
    VK_CHECK(vkCreateImageView(r->device, &view, NULL, &atlas->view));
    return 1;
}

static void atlas_transfer(NtRenderer *r) {
    Atlas *atlas = r->atlas;
    if (atlas->layout == VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL) return;
    if (r->rendering) {
        vkCmdEndRendering(r->command);
        r->rendering = 0;
        VkMemoryBarrier barrier = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        };
        vkCmdPipelineBarrier(r->command, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                             VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                             VK_DEPENDENCY_BY_REGION_BIT, 1, &barrier, 0, NULL, 0, NULL);
    }
    int empty = atlas->layout == VK_IMAGE_LAYOUT_UNDEFINED;
    image_barrier(r, atlas->image, atlas->layout, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                  empty ? VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT : VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                  VK_PIPELINE_STAGE_TRANSFER_BIT,
                  empty ? 0 : VK_ACCESS_SHADER_READ_BIT, VK_ACCESS_TRANSFER_WRITE_BIT);
    atlas->layout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
}

/* Buffers are only reused after replay has waited for the previous frame's fence. */
static int upload_space(NtRenderer *r, VkDeviceSize size, Upload **buffer,
                        VkDeviceSize *offset) {
    Upload **link = &r->uploads;
    for (; *link; link = &(*link)->next) {
        Upload *upload = *link;
        VkDeviceSize start = (upload->used + 15) & ~(VkDeviceSize)15;
        if (start + size <= upload->size) {
            upload->used = start + size;
            *buffer = upload;
            *offset = start;
            return 1;
        }
    }
    Upload *upload = *link = calloc(1, sizeof *upload);
    upload->size = size > UPLOAD_SIZE ? size : UPLOAD_SIZE;
    VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = upload->size,
        .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_VERTEX_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VK_CHECK(vkCreateBuffer(r->device, &info, NULL, &upload->buffer));
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(r->device, upload->buffer, &requirements);
    VkMemoryAllocateInfo memory = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = requirements.size,
    };
    if (!memory_type(r, requirements.memoryTypeBits,
                     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                     &memory.memoryTypeIndex)) return 0;
    VK_CHECK(vkAllocateMemory(r->device, &memory, NULL, &upload->memory));
    VK_CHECK(vkBindBufferMemory(r->device, upload->buffer, upload->memory, 0));
    void *mapped;
    VK_CHECK(vkMapMemory(r->device, upload->memory, 0, VK_WHOLE_SIZE, 0, &mapped));
    upload->mapped = mapped;
    upload->used = size;
    *buffer = upload;
    *offset = 0;
    return 1;
}

static int upload_glyph(NtRenderer *r, int x, int y, int width, int height,
                        cairo_surface_t *surface) {
    atlas_transfer(r);
    Upload *upload;
    VkDeviceSize offset;
    int stride = cairo_image_surface_get_stride(surface);
    if (!upload_space(r, (VkDeviceSize)stride * height, &upload, &offset)) return 0;
    memcpy(upload->mapped + offset, cairo_image_surface_get_data(surface), (size_t)stride * height);
    VkBufferImageCopy copy = {
        .bufferOffset = offset,
        .bufferRowLength = (uint32_t)stride / 4,
        .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
        .imageOffset = {x, y, 0}, .imageExtent = {(uint32_t)width, (uint32_t)height, 1},
    };
    vkCmdCopyBufferToImage(r->command, upload->buffer, r->atlas->image,
                           VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
    return 1;
}

static void begin_rendering(NtRenderer *r) {
    if (r->rendering) return;
    VkRenderingAttachmentInfo color = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
        .imageView = r->views[r->image_index], .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        .loadOp = VK_ATTACHMENT_LOAD_OP_LOAD, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
    };
    VkRenderingInfo rendering = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
        .renderArea = {{0, 0}, r->extent}, .layerCount = 1,
        .colorAttachmentCount = 1, .pColorAttachments = &color,
    };
    vkCmdBeginRendering(r->command, &rendering);
    r->rendering = 1;
}

static int flush_quads(NtRenderer *r) {
    size_t count = r->quad_count;
    if (!count) return 1;
    Atlas *atlas = r->atlas;
    /* Sampling needs a defined layout even before the first glyph upload. */
    if (atlas->layout == VK_IMAGE_LAYOUT_UNDEFINED) atlas_transfer(r);
    if (atlas->layout == VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL) {
        image_barrier(r, atlas->image, atlas->layout, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                      VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                      VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT);
        atlas->layout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    }
    if (!r->descriptors) {
        VkDescriptorSetAllocateInfo allocate = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
            .descriptorPool = r->descriptor_pool, .descriptorSetCount = 1,
            .pSetLayouts = &r->descriptor_layout,
        };
        VK_CHECK(vkAllocateDescriptorSets(r->device, &allocate, &r->descriptors));
        VkDescriptorImageInfo image = {r->sampler, atlas->view,
                                       VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
        VkWriteDescriptorSet write = {
            .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = r->descriptors, .descriptorCount = 1,
            .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, .pImageInfo = &image,
        };
        vkUpdateDescriptorSets(r->device, 1, &write, 0, NULL);
        vkCmdBindDescriptorSets(r->command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                                r->pipeline_layout, 0, 1, &r->descriptors, 0, NULL);
    }
    Upload *upload;
    VkDeviceSize offset;
    if (!upload_space(r, count * sizeof(Quad), &upload, &offset)) return 0;
    memcpy(upload->mapped + offset, r->quads, count * sizeof(Quad));
    begin_rendering(r);
    vkCmdSetScissor(r->command, 0, 1, &r->clip);
    vkCmdBindVertexBuffers(r->command, 0, 1, &upload->buffer, &offset);
    vkCmdDraw(r->command, 4, (uint32_t)count, 0, 0);
    r->quad_count = 0;
    return 1;
}

static void clear_glyphs(NtRenderer *r) {
    if (!r->glyph_count) return;
    /* Font marks occupancy; leave empty slots' zero pages untouched. */
    for (size_t i = 0; i < GLYPH_SLOTS; ++i) {
        if (!r->glyph_fonts[i]) continue;
        cairo_scaled_font_destroy(r->glyph_fonts[i]);
        r->glyph_fonts[i] = NULL;
    }
    r->glyph_count = 0;
    r->atlas_x = r->atlas_y = r->atlas_row = 0;
}

static int reset_atlas(NtRenderer *r) {
    /* Draw before atlas reuse can overwrite a queued glyph. */
    if (!flush_quads(r)) return 0;
    clear_glyphs(r);
    return 1;
}

static int create_pipeline(NtRenderer *r) {
    VkShaderModuleCreateInfo module = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = sizeof nt_vertex_spv, .pCode = nt_vertex_spv,
    };
    VkShaderModule vertex, fragment;
    VK_CHECK(vkCreateShaderModule(r->device, &module, NULL, &vertex));
    module.codeSize = sizeof nt_fragment_spv;
    module.pCode = nt_fragment_spv;
    VK_CHECK(vkCreateShaderModule(r->device, &module, NULL, &fragment));
    VkPipelineShaderStageCreateInfo stages[2] = {
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vertex, .pName = "main"},
        {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
         .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fragment, .pName = "main"},
    };
    VkVertexInputBindingDescription binding = {0, sizeof(Quad), VK_VERTEX_INPUT_RATE_INSTANCE};
    VkVertexInputAttributeDescription attributes[] = {
        {0, 0, VK_FORMAT_R32G32B32A32_SFLOAT, offsetof(Quad, rect)},
        {1, 0, VK_FORMAT_R32G32B32A32_SFLOAT, offsetof(Quad, uv)},
        {2, 0, VK_FORMAT_R8G8B8A8_UNORM, offsetof(Quad, color)},
        {3, 0, VK_FORMAT_R32_SFLOAT, offsetof(Quad, mode)},
    };
    VkPipelineVertexInputStateCreateInfo vertex_input = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO,
        .vertexBindingDescriptionCount = 1, .pVertexBindingDescriptions = &binding,
        .vertexAttributeDescriptionCount = 4, .pVertexAttributeDescriptions = attributes,
    };
    VkPipelineInputAssemblyStateCreateInfo assembly = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO,
        .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_STRIP,
    };
    VkPipelineViewportStateCreateInfo viewport = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO,
        .viewportCount = 1, .scissorCount = 1,
    };
    VkPipelineRasterizationStateCreateInfo raster = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO,
        .polygonMode = VK_POLYGON_MODE_FILL, .cullMode = VK_CULL_MODE_NONE,
        .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1,
    };
    VkPipelineMultisampleStateCreateInfo multisample = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO,
        .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT,
    };
    VkPipelineColorBlendAttachmentState attachment = {
        .blendEnable = VK_TRUE,
        .srcColorBlendFactor = VK_BLEND_FACTOR_ONE, .dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
        .colorBlendOp = VK_BLEND_OP_ADD,
        .srcAlphaBlendFactor = VK_BLEND_FACTOR_ONE, .dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA,
        .alphaBlendOp = VK_BLEND_OP_ADD,
        .colorWriteMask = VK_COLOR_COMPONENT_R_BIT | VK_COLOR_COMPONENT_G_BIT |
                          VK_COLOR_COMPONENT_B_BIT | VK_COLOR_COMPONENT_A_BIT,
    };
    VkPipelineColorBlendStateCreateInfo blend = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO,
        .attachmentCount = 1, .pAttachments = &attachment,
    };
    VkDynamicState dynamic_states[] = {VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR};
    VkPipelineDynamicStateCreateInfo dynamic = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO,
        .dynamicStateCount = 2, .pDynamicStates = dynamic_states,
    };
    VkPipelineRenderingCreateInfo rendering = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO,
        .colorAttachmentCount = 1, .pColorAttachmentFormats = &r->format,
    };
    VkGraphicsPipelineCreateInfo pipeline = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &rendering,
        .stageCount = 2, .pStages = stages, .pVertexInputState = &vertex_input,
        .pInputAssemblyState = &assembly, .pViewportState = &viewport,
        .pRasterizationState = &raster, .pMultisampleState = &multisample,
        .pColorBlendState = &blend, .pDynamicState = &dynamic, .layout = r->pipeline_layout,
    };
    VK_CHECK(vkCreateGraphicsPipelines(r->device, VK_NULL_HANDLE, 1, &pipeline, NULL, &r->pipeline));
    vkDestroyShaderModule(r->device, fragment, NULL);
    vkDestroyShaderModule(r->device, vertex, NULL);
    return 1;
}

/* Wayland surfaces take their size from the swapchain. */
static int create_swapchain(NtRenderer *r, VkExtent2D extent) {
    VK_CHECK(vkDeviceWaitIdle(r->device));
    for (uint32_t i = 0; i < r->image_count; ++i) {
        vkDestroyImageView(r->device, r->views[i], NULL);
        vkDestroySemaphore(r->device, r->presented[i], NULL);
    }
    vkDestroySwapchainKHR(r->device, r->swapchain, NULL);
    free(r->images);
    free(r->views);
    free(r->presented);
    VkSurfaceCapabilitiesKHR caps;
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(r->physical, r->surface, &caps));
    uint32_t count;
    VK_CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(r->physical, r->surface, &count, NULL));
    VkSurfaceFormatKHR formats[count];
    VK_CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(r->physical, r->surface, &count, formats));
    VkSurfaceFormatKHR format = {VK_FORMAT_UNDEFINED, VK_COLOR_SPACE_SRGB_NONLINEAR_KHR};
    for (uint32_t i = 0; i < count; ++i) {
        if ((formats[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
             formats[i].format == VK_FORMAT_R8G8B8A8_UNORM) &&
            formats[i].colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR) {
            format = formats[i];
            break;
        }
    }
    if (format.format == VK_FORMAT_UNDEFINED) return fail(r, "Wayland surface has no Vulkan RGBA8 UNORM format");
    VkCompositeAlphaFlagBitsKHR alpha = VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR;
    if (!(caps.supportedCompositeAlpha & alpha))
        return fail(r, "Vulkan Wayland surface lacks premultiplied alpha");
    count = caps.minImageCount + 1;
    if (caps.maxImageCount && count > caps.maxImageCount) count = caps.maxImageCount;
    /* VK_KHR_wayland_surface requires MAILBOX support. */
    VkSwapchainCreateInfoKHR swapchain = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = r->surface, .minImageCount = count,
        .imageFormat = format.format, .imageColorSpace = format.colorSpace,
        .imageExtent = extent, .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = caps.currentTransform, .compositeAlpha = alpha,
        .presentMode = VK_PRESENT_MODE_MAILBOX_KHR, .clipped = VK_TRUE,
    };
    VK_CHECK(vkCreateSwapchainKHR(r->device, &swapchain, NULL, &r->swapchain));
    r->extent = extent;
    VK_CHECK(vkGetSwapchainImagesKHR(r->device, r->swapchain, &r->image_count, NULL));
    r->images = calloc(r->image_count, sizeof *r->images);
    r->views = calloc(r->image_count, sizeof *r->views);
    r->presented = calloc(r->image_count, sizeof *r->presented);
    VK_CHECK(vkGetSwapchainImagesKHR(r->device, r->swapchain, &r->image_count, r->images));
    for (uint32_t i = 0; i < r->image_count; ++i) {
        VkImageViewCreateInfo view = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            .image = r->images[i], .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = format.format,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        VK_CHECK(vkCreateImageView(r->device, &view, NULL, &r->views[i]));
        VkSemaphoreCreateInfo semaphore = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        VK_CHECK(vkCreateSemaphore(r->device, &semaphore, NULL, &r->presented[i]));
    }
    if (r->format != format.format) {
        vkDestroyPipeline(r->device, r->pipeline, NULL);
        r->format = format.format;
        if (!create_pipeline(r)) return 0;
    }
    r->resize = 0;
    return 1;
}

static int gpu_init(NtRenderer *r) {
    const char *extensions[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME};
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "nextty", .apiVersion = VK_API_VERSION_1_3,
    };
    VkInstanceCreateInfo instance_info = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
        .enabledExtensionCount = 2, .ppEnabledExtensionNames = extensions,
    };
    VkInstance instance;
    VK_CHECK(vkCreateInstance(&instance_info, NULL, &instance));
    VkWaylandSurfaceCreateInfoKHR surface = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = nt_window_display(r->window), .surface = nt_window_surface(r->window),
    };
    VK_CHECK(vkCreateWaylandSurfaceKHR(instance, &surface, NULL, &r->surface));
    uint32_t count;
    VK_CHECK(vkEnumeratePhysicalDevices(instance, &count, NULL));
    VkPhysicalDevice *devices = malloc(count * sizeof *devices);
    VK_CHECK(vkEnumeratePhysicalDevices(instance, &count, devices));
    uint32_t queue_family = 0;
    int selected_score = 0;
    for (uint32_t i = 0; i < count; ++i) {
        VkPhysicalDeviceProperties properties;
        vkGetPhysicalDeviceProperties(devices[i], &properties);
        /* Vulkan 1.3 devices all support dynamic rendering. */
        int score = properties.apiVersion < VK_API_VERSION_1_3 ? 0 :
                    properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU ? 3 :
                    properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU ? 2 : 1;
        if (score <= selected_score) continue;
        uint32_t families;
        vkGetPhysicalDeviceQueueFamilyProperties(devices[i], &families, NULL);
        VkQueueFamilyProperties queues[families];
        vkGetPhysicalDeviceQueueFamilyProperties(devices[i], &families, queues);
        for (uint32_t j = 0; j < families; ++j) {
            VkBool32 present;
            VK_CHECK(vkGetPhysicalDeviceSurfaceSupportKHR(devices[i], j, r->surface, &present));
            if ((queues[j].queueFlags & VK_QUEUE_GRAPHICS_BIT) && present) {
                r->physical = devices[i];
                queue_family = j;
                selected_score = score;
                break;
            }
        }
    }
    free(devices);
    if (!r->physical) return fail(r, "No Vulkan 1.3 graphics device with Wayland presentation");
    vkGetPhysicalDeviceMemoryProperties(r->physical, &r->memory);
    float priority = 1;
    VkDeviceQueueCreateInfo queue = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = queue_family, .queueCount = 1, .pQueuePriorities = &priority,
    };
    VkPhysicalDeviceVulkan13Features features = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE,
    };
    const char *device_extensions[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
    VkDeviceCreateInfo device = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &features,
        .queueCreateInfoCount = 1, .pQueueCreateInfos = &queue,
        .enabledExtensionCount = 1, .ppEnabledExtensionNames = device_extensions,
    };
    VK_CHECK(vkCreateDevice(r->physical, &device, NULL, &r->device));
    vkGetDeviceQueue(r->device, queue_family, 0, &r->queue);
    VkCommandPoolCreateInfo pool_info = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = queue_family,
    };
    VkCommandPool pool;
    VK_CHECK(vkCreateCommandPool(r->device, &pool_info, NULL, &pool));
    VkCommandBufferAllocateInfo command = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1,
    };
    VK_CHECK(vkAllocateCommandBuffers(r->device, &command, &r->command));
    /* Signalled, so the first frame does not wait for a frame before it. */
    VkFenceCreateInfo fence = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO,
                               .flags = VK_FENCE_CREATE_SIGNALED_BIT};
    VK_CHECK(vkCreateFence(r->device, &fence, NULL, &r->fence));
    VkSemaphoreCreateInfo semaphore = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VK_CHECK(vkCreateSemaphore(r->device, &semaphore, NULL, &r->acquired));
    VkDescriptorSetLayoutBinding binding = {0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1,
                                            VK_SHADER_STAGE_FRAGMENT_BIT, NULL};
    VkDescriptorSetLayoutCreateInfo descriptors = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 1, .pBindings = &binding,
    };
    VK_CHECK(vkCreateDescriptorSetLayout(r->device, &descriptors, NULL, &r->descriptor_layout));
    VkPushConstantRange constants = {VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(float) * 2};
    VkPipelineLayoutCreateInfo layout = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1, .pSetLayouts = &r->descriptor_layout,
        .pushConstantRangeCount = 1, .pPushConstantRanges = &constants,
    };
    VK_CHECK(vkCreatePipelineLayout(r->device, &layout, NULL, &r->pipeline_layout));
    VkDescriptorPoolSize size = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, ATLAS_SIZES};
    VkDescriptorPoolCreateInfo descriptor_pool = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = ATLAS_SIZES, .poolSizeCount = 1, .pPoolSizes = &size,
    };
    VK_CHECK(vkCreateDescriptorPool(r->device, &descriptor_pool, NULL, &r->descriptor_pool));
    VkSamplerCreateInfo sampler = {
        .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
        .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR,
        .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
        .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
    };
    VK_CHECK(vkCreateSampler(r->device, &sampler, NULL, &r->sampler));
    return create_atlas(r, ATLAS_MIN);
}

static Quad make_quad(float x, float y, float width, float height, uint32_t color, int mode) {
    return (Quad){ .rect = {x, y, width, height}, .color = {
        (unsigned char)(color >> 24), (unsigned char)(color >> 16),
        (unsigned char)(color >> 8), (unsigned char)color}, .mode = (float)mode };
}

static void reserve_quads(NtRenderer *r, size_t count) {
    if (count > r->quad_capacity) {
        r->quad_capacity = r->quad_capacity * 2 > count ? r->quad_capacity * 2 : count;
        r->quads = realloc(r->quads, r->quad_capacity * sizeof *r->quads);
    }
}

static int finish_raster(NtRenderer *r, cairo_t *cr, cairo_surface_t *surface) {
    cairo_status_t status = cairo_status(cr);
    if (status == CAIRO_STATUS_SUCCESS) {
        cairo_surface_flush(surface);
        status = cairo_surface_status(surface);
    }
    cairo_destroy(cr);
    return status == CAIRO_STATUS_SUCCESS ||
           fail(r, "Cairo glyph raster: %s", cairo_status_to_string(status));
}

static CachedGlyph *cached_glyph(NtRenderer *r, const NtFontGlyph *glyph) {
    size_t hash = (((uintptr_t)glyph->font >> 4) ^ ((size_t)glyph->glyph * 2654435761u)) & (GLYPH_SLOTS - 1);
    size_t slot = hash;
    while (r->glyph_fonts[slot]) {
        CachedGlyph *entry = &r->glyphs[slot];
        if (r->glyph_fonts[slot] == glyph->font && entry->glyph == glyph->glyph)
            return entry;
        slot = (slot + 1) & (GLYPH_SLOTS - 1);
    }
    if (r->glyph_count >= GLYPH_SLOTS * 3 / 4) {
        if (!reset_atlas(r)) return NULL;
        slot = hash;
    }
    cairo_glyph_t single = { .index = glyph->glyph };
    cairo_text_extents_t ink;
    cairo_scaled_font_glyph_extents(glyph->font, &single, 1, &ink);
    cairo_status_t status = cairo_scaled_font_status(glyph->font);
    if (status != CAIRO_STATUS_SUCCESS) {
        fail(r, "Cairo glyph extents: %s", cairo_status_to_string(status));
        return NULL;
    }
    int left = (int)floor(ink.x_bearing) - 1;
    int top = (int)floor(ink.y_bearing) - 1;
    int width = (int)ceil(ink.x_bearing + ink.width) - left + 1;
    int height = (int)ceil(ink.y_bearing + ink.height) - top + 1;
    if (!ink.width || !ink.height) width = height = 0;
    if (width > ATLAS_MAX || height > ATLAS_MAX || width < 0 || height < 0) {
        fail(r, "Glyph raster %dx%d exceeds %dx%d atlas", width, height, ATLAS_MAX, ATLAS_MAX);
        return NULL;
    }
    if (r->atlas_x + width > r->atlas_size) {
        r->atlas_x = 0;
        r->atlas_y += r->atlas_row;
        r->atlas_row = 0;
    }
    if (width > r->atlas_size || r->atlas_y + height > r->atlas_size) {
        if (!reset_atlas(r)) return NULL;
        int size = r->atlas_size * 2;
        while (size < width || size < height) size *= 2;
        if (size <= ATLAS_MAX && !create_atlas(r, size)) return NULL;
        slot = hash;
    }
    int x = r->atlas_x, y = r->atlas_y;
    if (width && height) {
        /* Mask glyphs keep their coverage in alpha; color glyphs keep their colors. */
        cairo_surface_t *surface = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, width, height);
        cairo_t *cr = cairo_create(surface);
        cairo_set_source_rgba(cr, 1, 1, 1, 1);
        cairo_set_scaled_font(cr, glyph->font);
        single.x = -left;
        single.y = -top;
        cairo_show_glyphs(cr, &single, 1);
        int uploaded = finish_raster(r, cr, surface) &&
            upload_glyph(r, x, y, width, height, surface);
        cairo_surface_destroy(surface);
        if (!uploaded) return NULL;
        r->atlas_x += width;
        if (r->atlas_row < height) r->atlas_row = height;
    }
    CachedGlyph *entry = &r->glyphs[slot];
    r->glyph_fonts[slot] = cairo_scaled_font_reference(glyph->font);
    *entry = (CachedGlyph){ .glyph = glyph->glyph,
        .x = x, .y = y, .width = width,
        .height = height, .left = left, .top = top };
    ++r->glyph_count;
    return entry;
}

static int shape_line(NtRenderer *r, NtLine *line) {
    LineInput *input = line->input;
    if (!input) return 1;
    int shaped = nt_fonts_shape(r->fonts, input->text, input->length, input->runs,
                                input->run_count, input->columns, &line->glyphs, &line->count);
    free(input);
    line->input = NULL;
    return shaped;
}

static int draw_line(NtRenderer *r, NtLine *line, float x, float y) {
    if (!shape_line(r, line)) return 0;
    reserve_quads(r, r->quad_count + line->count);
    double inverse_scale = 1. / r->scale;
    float stroke = fmaxf(1.f, roundf(r->font_size * r->scale / 16.f)) / r->scale;
    float bar_top = roundf((y + (r->cell_height - stroke) * 0.5f) * r->scale) / r->scale;
    for (size_t i = 0; i < line->count; ++i) {
        const NtFontGlyph *glyph = &line->glyphs[i];
        if (!(glyph->color & 255)) continue;
        if (glyph->cell_shape >= 0x2580) {
            unsigned code = glyph->cell_shape;
            unsigned mask = 1;
            unsigned left = 0, top = 0, right = 8, bottom = 8;
            if (code == 0x2580) bottom = 4;
            else if (code <= 0x2588) top = 8 - (code - 0x2580);
            else if (code <= 0x258f) right = 0x2590 - code;
            else if (code == 0x2590) left = 4;
            else if (code == 0x2594) bottom = 1;
            else if (code == 0x2595) left = 7;
            else {
                static const unsigned quadrants[] = {4, 8, 1, 13, 9, 7, 11, 2, 6, 14};
                mask = quadrants[code - 0x2596];
                /* Quadrant masks contain at most three parts. */
                reserve_quads(r, r->quad_count + 3 + line->count - i - 1);
            }
            for (unsigned part = 0; part < 4; ++part) {
                if (!(mask & (1u << part))) continue;
                if (code >= 0x2596) {
                    left = (part % 2) * 4;
                    top = (part / 2) * 4;
                    right = left + 4;
                    bottom = top + 4;
                }
                /* Round shared edges, not individual widths, so adjacent blocks meet. */
                float x0 = roundf((x + glyph->x + r->cell_width * left / 8) * r->scale) / r->scale;
                float y0 = roundf((y + r->cell_height * top / 8) * r->scale) / r->scale;
                float x1 = roundf((x + glyph->x + r->cell_width * right / 8) * r->scale) / r->scale;
                float y1 = roundf((y + r->cell_height * bottom / 8) * r->scale) / r->scale;
                r->quads[r->quad_count++] = make_quad(x0, y0, x1 - x0, y1 - y0, glyph->color, NT_RECT);
            }
            continue;
        }
        Quad q;
        if (glyph->cell_shape == 0x2502) {
            float left = roundf((x + glyph->x + (r->cell_width - stroke) * 0.5f) * r->scale) / r->scale;
            q = make_quad(left, y, stroke, r->cell_height, glyph->color, NT_RECT);
        } else if (glyph->cell_shape) {
            q = make_quad(x + glyph->x, bar_top, r->cell_width, stroke, glyph->color, NT_RECT);
        } else {
            CachedGlyph *entry = cached_glyph(r, glyph);
            if (!entry) return 0;
            if (!entry->width || !entry->height) continue;
            /* Keep filtering inside this tile's transparent border after atlas reuse. */
            q = make_quad(x + glyph->x + (float)((entry->left + 0.5) * inverse_scale),
                       y + glyph->y + (float)((entry->top + 0.5) * inverse_scale),
                       (float)((entry->width - 1) * inverse_scale), (float)((entry->height - 1) * inverse_scale),
                       glyph->color, glyph->color_glyph ? NT_COLOR_GLYPH : NT_MASK_GLYPH);
            float inverse_atlas = 1.f / r->atlas_size;
            q.uv[0] = (entry->x + 0.5f) * inverse_atlas;
            q.uv[1] = (entry->y + 0.5f) * inverse_atlas;
            q.uv[2] = (entry->width - 1) * inverse_atlas;
            q.uv[3] = (entry->height - 1) * inverse_atlas;
        }
        r->quads[r->quad_count++] = q;
    }
    return 1;
}

static int replay(NtRenderer *r) {
    Frame *f = &r->frame;
    VK_CHECK(vkWaitForFences(r->device, 1, &r->fence, VK_TRUE, UINT64_MAX));
    while (r->retired) {
        Atlas *atlas = r->retired;
        r->retired = atlas->next;
        vkDestroyImageView(r->device, atlas->view, NULL);
        vkDestroyImage(r->device, atlas->image, NULL);
        vkFreeMemory(r->device, atlas->memory, NULL);
        free(atlas);
    }
    for (Upload *upload = r->uploads; upload; upload = upload->next) upload->used = 0;
    VkExtent2D requested = {(uint32_t)lround(f->width * f->scale),
                            (uint32_t)lround(f->height * f->scale)};
    if (!r->swapchain || r->resize || r->extent.width != requested.width ||
        r->extent.height != requested.height)
        if (!create_swapchain(r, requested)) return 0;
    for (;;) {
        VkResult result = vkAcquireNextImageKHR(r->device, r->swapchain, UINT64_MAX,
                                                r->acquired, VK_NULL_HANDLE, &r->image_index);
        if (result == VK_SUCCESS || result == VK_SUBOPTIMAL_KHR) {
            r->resize = result == VK_SUBOPTIMAL_KHR;
            break;
        }
        if (result != VK_ERROR_OUT_OF_DATE_KHR) return fail(r, "Acquiring Vulkan frame: %d", result);
        if (!create_swapchain(r, requested)) return 0;
    }
    VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    VK_CHECK(vkBeginCommandBuffer(r->command, &begin));
    VkViewport viewport = {0, 0, (float)r->extent.width, (float)r->extent.height, 0, 1};
    vkCmdSetViewport(r->command, 0, 1, &viewport);
    vkCmdBindPipeline(r->command, VK_PIPELINE_BIND_POINT_GRAPHICS, r->pipeline);
    float logical_size[2] = {(float)f->width, (float)f->height};
    vkCmdPushConstants(r->command, r->pipeline_layout, VK_SHADER_STAGE_VERTEX_BIT, 0,
                       sizeof logical_size, logical_size);
    if (r->descriptors)
        vkCmdBindDescriptorSets(r->command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                                r->pipeline_layout, 0, 1, &r->descriptors, 0, NULL);
    /* The acquire semaphore waits at color output, including this transition. */
    image_barrier(r, r->images[r->image_index], VK_IMAGE_LAYOUT_UNDEFINED,
                  VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                  VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                  0, VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT);
    r->clip = (VkRect2D){{0, 0}, r->extent};
    r->quad_count = 0;
    /* A scale change reloads the fonts: drop cached glyphs before any quad references the atlas. */
    if (r->scale != f->scale) {
        clear_glyphs(r);
        nt_fonts_scale(r->fonts, f->scale);
        r->scale = f->scale;
    }
    for (size_t i = 0; i < f->op_count; ++i) {
        const Op *op = &f->ops[i];
        switch (op->kind) {
        case OP_CLEAR: {
            if (!flush_quads(r)) return 0;
            uint32_t background = op->background;
            float alpha = (background & 255) / 255.f;
            VkClearAttachment clear = {
                .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
                .clearValue.color = {{(background >> 24) / 255.f * alpha,
                    ((background >> 16) & 255) / 255.f * alpha,
                    ((background >> 8) & 255) / 255.f * alpha, alpha}},
            };
            if (r->clip.extent.width && r->clip.extent.height) {
                begin_rendering(r);
                VkClearRect rect = {r->clip, 0, 1};
                vkCmdClearAttachments(r->command, 1, &clear, 1, &rect);
            }
            break;
        }
        case OP_CLIP: {
            if (!flush_quads(r)) return 0;
            if (op->clip[2] <= 0 || op->clip[3] <= 0) {
                r->clip = (VkRect2D){{0, 0}, r->extent};
            } else {
                double sx = (double)r->extent.width / f->width;
                double sy = (double)r->extent.height / f->height;
                /* Shared logical edges must quantize to the same physical pixel. */
                int left = (int)fmax(0, fmin(r->extent.width, round(op->clip[0] * sx)));
                int top = (int)fmax(0, fmin(r->extent.height, round(op->clip[1] * sy)));
                int right = (int)fmax(left, fmin(r->extent.width, round((op->clip[0] + op->clip[2]) * sx)));
                int bottom = (int)fmax(top, fmin(r->extent.height, round((op->clip[1] + op->clip[3]) * sy)));
                r->clip = (VkRect2D){{left, top}, {(uint32_t)(right - left), (uint32_t)(bottom - top)}};
            }
            break;
        }
        case OP_QUAD:
            reserve_quads(r, r->quad_count + 1);
            r->quads[r->quad_count++] = op->quad;
            break;
        case OP_LINE:
            if (!draw_line(r, op->line.line, op->line.x, op->line.y)) return 0;
            break;
        }
    }
    return flush_quads(r);
}

static int gpu_present(NtRenderer *r) {
    if (r->rendering) {
        vkCmdEndRendering(r->command);
        r->rendering = 0;
    }
    image_barrier(r, r->images[r->image_index], VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                  VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
                  VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                  VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, 0);
    VK_CHECK(vkEndCommandBuffer(r->command));
    VK_CHECK(vkResetFences(r->device, 1, &r->fence));
    VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .waitSemaphoreCount = 1, .pWaitSemaphores = &r->acquired, .pWaitDstStageMask = &wait_stage,
        .commandBufferCount = 1, .pCommandBuffers = &r->command,
        .signalSemaphoreCount = 1, .pSignalSemaphores = &r->presented[r->image_index],
    };
    VK_CHECK(vkQueueSubmit(r->queue, 1, &submit, r->fence));
    VkPresentInfoKHR present = {
        .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
        .waitSemaphoreCount = 1, .pWaitSemaphores = &r->presented[r->image_index],
        .swapchainCount = 1, .pSwapchains = &r->swapchain, .pImageIndices = &r->image_index,
    };
    VkResult result = vkQueuePresentKHR(r->queue, &present);
    if (result == VK_SUBOPTIMAL_KHR || result == VK_ERROR_OUT_OF_DATE_KHR) {
        r->resize = 1;
        return 1;
    }
    return result == VK_SUCCESS || fail(r, "Presenting Vulkan frame: %d", result);
}

/* The eventfd hands each finished frame, and any error it set, back to the UI thread. */
static void *render(void *data) {
    NtRenderer *r = data;
    for (;;) {
        while (sem_wait(&r->start))
            continue;
        if (replay(r)) gpu_present(r);
        eventfd_write(r->done, 1);
    }
    return NULL;
}

NtRenderer *nt_renderer_new(NtWindow *window, const char *family, float font_size) {
    NtRenderer *r = calloc(1, sizeof *r);
    r->window = window;
    r->font_size = font_size;
    sem_init(&r->start, 0, 0);
    r->done = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (r->done < 0) {
        fail(r, "Creating the frame eventfd: %s", strerror(errno));
    } else if ((r->fonts = nt_fonts_new(family, font_size, r->error, sizeof r->error)) &&
               gpu_init(r)) {
        nt_fonts_metrics(r->fonts, &r->cell_width, &r->cell_height);
        int error = pthread_create(&r->thread, NULL, render, r);
        if (error) fail(r, "Starting the render thread: %s", strerror(error));
    }
    return r;
}

int nt_renderer_fd(NtRenderer *r) {
    return r->done;
}

void nt_renderer_metrics(NtRenderer *r, float *width, float *height) {
    *width = r->cell_width;
    *height = r->cell_height;
}

void nt_line_free(NtLine *line) {
    if (--line->references) return;
    nt_font_glyphs_free(line->glyphs, line->count);
    free(line->input);
    free(line);
}

int nt_renderer_ready(NtRenderer *r) {
    eventfd_t frames;
    if (r->pending) {
        if (eventfd_read(r->done, &frames) < 0) return 0;
        r->pending = 0;
        Frame *f = &r->frame;
        for (size_t i = 0; i < f->op_count; ++i)
            if (f->ops[i].kind == OP_LINE) nt_line_free(f->ops[i].line.line);
        f->op_count = 0;
    }
    return r->error[0] ? -1 : 1;
}

int nt_renderer_finish(NtRenderer *r) {
    struct pollfd done = {.fd = r->done, .events = POLLIN};
    if (r->pending) poll(&done, 1, -1);
    return nt_renderer_ready(r) > 0;
}

void nt_renderer_present(NtRenderer *r) {
    r->pending = 1;
    sem_post(&r->start);
}

NtLine *nt_line_new(const char *text, size_t length, const NtRun *runs, size_t run_count,
                    const uint16_t *columns) {
    NtLine *line = calloc(1, sizeof *line);
    line->references = 1;
    if (!length) return line;
    size_t run_bytes = run_count * sizeof *runs, column_bytes = columns ? length * sizeof *columns : 0;
    LineInput *input = line->input = malloc(sizeof *input + run_bytes + column_bytes + length);
    input->length = length;
    input->run_count = run_count;
    memcpy(input->runs, runs, run_bytes);
    char *end = (char *)input->runs + run_bytes;
    input->columns = columns ? memcpy(end, columns, column_bytes) : NULL;
    input->text = memcpy(end + column_bytes, text, length);
    return line;
}

static void add_op(NtRenderer *r, Op op) {
    Frame *f = &r->frame;
    if (f->op_count == f->op_capacity) {
        f->op_capacity = f->op_capacity ? f->op_capacity * 2 : 32;
        f->ops = realloc(f->ops, f->op_capacity * sizeof *f->ops);
    }
    f->ops[f->op_count++] = op;
}

void nt_renderer_clear(NtRenderer *r, uint32_t background) {
    add_op(r, (Op){.kind = OP_CLEAR, .background = background});
}

void nt_renderer_begin(NtRenderer *r, uint32_t background) {
    Frame *f = &r->frame;
    nt_window_size(r->window, &f->width, &f->height, &f->scale);
    nt_window_prepare_frame(r->window);
    nt_renderer_clear(r, background);
}

void nt_renderer_clip(NtRenderer *r, float x, float y, float width, float height) {
    Op op = {.kind = OP_CLIP, .clip = {x, y, width, height}};
    Frame *f = &r->frame;
    if (f->op_count && f->ops[f->op_count - 1].kind == OP_CLIP)
        f->ops[f->op_count - 1] = op;
    else
        add_op(r, op);
}

void nt_renderer_quad(NtRenderer *r, float x, float y, float width, float height,
                      uint32_t color, int mode, float param) {
    if (width <= 0 || height <= 0 || !(color & 255)) return;
    Op op = {.kind = OP_QUAD, .quad = make_quad(x, y, width, height, color, mode)};
    op.quad.uv[0] = param;
    add_op(r, op);
}

void nt_renderer_line(NtRenderer *r, NtLine *line, float x, float y) {
    if (!line->input && !line->count) return;
    add_op(r, (Op){.kind = OP_LINE, .line = {line, x, y}});
    ++line->references;
}
