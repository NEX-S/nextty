#include "terminal.h"
#include "font.h"

#define VK_USE_PLATFORM_WAYLAND_KHR
#include <vulkan/vulkan.h>
#include <terminal_vert_spv.h>
#include <terminal_frag_spv.h>
#include <limits.h>
#include <math.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sys/eventfd.h>
#include <errno.h>
#include <time.h>
#include <unistd.h>

typedef struct NtVkUpload NtVkUpload;
struct NtVkUpload {
    VkBuffer buffer;
    VkDeviceMemory memory;
    unsigned char *mapped;
    VkDeviceSize size, used;
    NtVkUpload *next;
};

typedef struct NtVkAtlas NtVkAtlas;
struct NtVkAtlas {
    VkImage image;
    VkDeviceMemory memory;
    VkImageView view;
    VkImageLayout layout;
    NtVkAtlas *next;
};

typedef struct {
    VkInstance instance;
    VkSurfaceKHR surface;
    VkPhysicalDevice physical;
    VkPhysicalDeviceMemoryProperties memory;
    VkDevice device;
    VkQueue queue;
    uint32_t queue_family;
    VkSwapchainKHR swapchain;
    VkFormat format;
    VkExtent2D extent;
    VkImage *images;
    VkImageView *views;
    VkSemaphore *presented;
    uint32_t image_count, image_index;
    int resize, submitted, rendering;
    VkCommandPool command_pool;
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
    NtVkAtlas *atlas[2], *retired;
    NtVkUpload *uploads;
} NtVulkan;

enum { ATLAS_MAX = 2048, GLYPH_SLOTS = 8192 };

struct NtLine {
    NtRenderer *renderer;
    NtLine *retired_next;
    size_t references;
    char *text;
    size_t length;
    NtRun *runs; /* Owns the runs/columns/text allocation until shaping. */
    size_t run_count;
    uint16_t *columns;
    /* Each contiguous font group owns one reference. */
    NtFontGlyph *glyphs;
    size_t count;
};

typedef struct {
    cairo_scaled_font_t *font;
    uint32_t glyph;
    unsigned color;
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
        struct { NtLine *line; float x, y, factor; } line;
    };
} Op;

typedef struct {
    int width, height, pixel_width, pixel_height;
    double scale;
    Op *ops;
    size_t op_count, op_capacity;
} Frame;

enum { SWAP_IDLE, SWAP_PENDING, SWAP_DONE };

typedef struct {
    pthread_t thread;
    pthread_mutex_t lock;
    pthread_cond_t changed;
    int fd, state, stopping, started, initialized;
    int64_t end;
    const char *family;
    NtLine *retired;
} Swap;

struct NtRenderer {
    Swap swap;
    Frame frame;
    NtWindow *window;
    NtVulkan vk;
    NtFonts *fonts;
    float font_size, cell_width, cell_height;
    double scale;
    int texture_size[2];
    int atlas_size[2], atlas_x[2], atlas_y[2], atlas_row[2];
    CachedGlyph glyphs[GLYPH_SLOTS];
    size_t glyph_count;
    Quad *quads;
    size_t quad_count, quad_capacity;
    char error[512];
};

static _Thread_local char creation_error[512];

static int fail(NtRenderer *r, const char *format, ...) {
    va_list args;
    va_start(args, format);
    if (!r || !r->error[0])
        vsnprintf(r ? r->error : creation_error, sizeof creation_error, format, args);
    va_end(args);
    return 0;
}

const char *nt_renderer_error(NtRenderer *r) {
    return r ? r->error : creation_error;
}

static void clear_frame(Frame *f) {
    for (size_t i = 0; i < f->op_count; ++i)
        if (f->ops[i].kind == OP_LINE) nt_line_free(f->ops[i].line.line);
    f->op_count = 0;
}

static void free_line_input(NtLine *line) {
    free(line->runs);
    line->text = NULL;
    line->runs = NULL;
    line->columns = NULL;
}

static void destroy_line(NtLine *line) {
    nt_font_glyphs_free(line->glyphs, line->count);
    free_line_input(line);
    free(line);
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

static int add_op(NtRenderer *r, Op op) {
    Frame *f = &r->frame;
    if (f->op_count == f->op_capacity) {
        size_t capacity = f->op_capacity ? f->op_capacity * 2 : 32;
        if (capacity < f->op_capacity || capacity > SIZE_MAX / sizeof(Op))
            return fail(r, "GPU frame has too many operations");
        Op *ops = realloc(f->ops, capacity * sizeof *ops);
        if (!ops) return fail(r, "Cannot allocate GPU frame operations");
        f->ops = ops;
        f->op_capacity = capacity;
    }
    f->ops[f->op_count++] = op;
    return 1;
}

static int flush_quads(NtRenderer *r);

static void clear_glyphs(NtRenderer *r) {
    /* Font marks occupancy; leave empty slots' zero pages untouched. */
    for (size_t i = 0; i < GLYPH_SLOTS; ++i) {
        if (!r->glyphs[i].font) continue;
        cairo_scaled_font_destroy(r->glyphs[i].font);
        r->glyphs[i].font = NULL;
    }
    r->glyph_count = 0;
    memset(r->atlas_x, 0, sizeof r->atlas_x);
    memset(r->atlas_y, 0, sizeof r->atlas_y);
    memset(r->atlas_row, 0, sizeof r->atlas_row);
}

static int reset_atlases(NtRenderer *r) {
    /* Draw before atlas reuse can overwrite a queued glyph. */
    if (!flush_quads(r)) return 0;
    clear_glyphs(r);
    return 1;
}

static int set_scale(NtRenderer *r, double scale) {
    if (!isfinite(scale) || scale <= 0 || !isfinite(r->font_size * scale))
        return fail(r, "Invalid window/font scale %.6g", scale);
    if (r->scale == scale) return 1;
    if (!reset_atlases(r) || !nt_fonts_scale(r->fonts, scale)) return 0;
    r->scale = scale;
    return 1;
}

static int64_t submission_time(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static int append_quad(NtRenderer *r, Quad q);
static int draw_line(NtRenderer *r, NtLine *line, float x, float y, float factor);

#define VK_CHECK(call) do { \
    VkResult vk_result = (call); \
    if (vk_result != VK_SUCCESS) return fail(r, "%s: Vulkan result %d", #call, vk_result); \
} while (0)

static int vk_memory_type(NtRenderer *r, uint32_t bits, VkMemoryPropertyFlags flags,
                          uint32_t *index) {
    for (uint32_t i = 0; i < r->vk.memory.memoryTypeCount; ++i) {
        if ((bits & (1u << i)) && (r->vk.memory.memoryTypes[i].propertyFlags & flags) == flags) {
            *index = i;
            return 1;
        }
    }
    return fail(r, "No Vulkan memory type for flags 0x%x", flags);
}

static void vk_image_barrier(NtRenderer *r, VkImage image,
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
    vkCmdPipelineBarrier(r->vk.command, source, destination, 0, 0, NULL, 0, NULL, 1, &barrier);
}

static int vk_create_atlas(NtRenderer *r, int color, int size) {
    NtVulkan *v = &r->vk;
    NtVkAtlas *atlas = calloc(1, sizeof *atlas);
    if (!atlas) return fail(r, "Cannot allocate Vulkan glyph atlas");
    if (v->atlas[color]) {
        v->atlas[color]->next = v->retired;
        v->retired = v->atlas[color];
    }
    v->atlas[color] = atlas;
    v->descriptors = VK_NULL_HANDLE;
    VkImageCreateInfo image = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
        .imageType = VK_IMAGE_TYPE_2D,
        .format = color ? VK_FORMAT_R8G8B8A8_UNORM : VK_FORMAT_R8_UNORM,
        .extent = {(uint32_t)size, (uint32_t)size, 1},
        .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT,
        .tiling = VK_IMAGE_TILING_OPTIMAL,
        .usage = VK_IMAGE_USAGE_TRANSFER_DST_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VK_CHECK(vkCreateImage(v->device, &image, NULL, &atlas->image));
    VkMemoryRequirements requirements;
    vkGetImageMemoryRequirements(v->device, atlas->image, &requirements);
    VkMemoryAllocateInfo memory = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = requirements.size,
    };
    if (!vk_memory_type(r, requirements.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
                        &memory.memoryTypeIndex)) return 0;
    VK_CHECK(vkAllocateMemory(v->device, &memory, NULL, &atlas->memory));
    VK_CHECK(vkBindImageMemory(v->device, atlas->image, atlas->memory, 0));
    VkImageViewCreateInfo view = {
        .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
        .image = atlas->image, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = image.format,
        .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
    };
    VK_CHECK(vkCreateImageView(v->device, &view, NULL, &atlas->view));
    r->texture_size[color] = size;
    return 1;
}

static void vk_atlas_transfer(NtRenderer *r, NtVkAtlas *atlas) {
    if (atlas->layout == VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL) return;
    if (r->vk.rendering) {
        vkCmdEndRendering(r->vk.command);
        r->vk.rendering = 0;
        VkMemoryBarrier barrier = {
            .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
            .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
            .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
        };
        vkCmdPipelineBarrier(r->vk.command, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                             VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                             VK_DEPENDENCY_BY_REGION_BIT, 1, &barrier, 0, NULL, 0, NULL);
    }
    int empty = atlas->layout == VK_IMAGE_LAYOUT_UNDEFINED;
    vk_image_barrier(r, atlas->image, atlas->layout, VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                     empty ? VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT : VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                     VK_PIPELINE_STAGE_TRANSFER_BIT,
                     empty ? 0 : VK_ACCESS_SHADER_READ_BIT, VK_ACCESS_TRANSFER_WRITE_BIT);
    atlas->layout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
}

static void vk_destroy_upload(NtRenderer *r, NtVkUpload *upload) {
    if (upload->mapped) vkUnmapMemory(r->vk.device, upload->memory);
    if (upload->buffer) vkDestroyBuffer(r->vk.device, upload->buffer, NULL);
    if (upload->memory) vkFreeMemory(r->vk.device, upload->memory, NULL);
}

static int vk_upload_space(NtRenderer *r, VkDeviceSize size, NtVkUpload **buffer,
                           VkDeviceSize *offset) {
    NtVulkan *v = &r->vk;
    NtVkUpload **link = &v->uploads;
    NtVkUpload *spare = NULL;
    while (*link) {
        NtVkUpload *upload = *link;
        VkDeviceSize start = (upload->used + 15) & ~(VkDeviceSize)15;
        if (start <= upload->size && size <= upload->size - start) {
            *buffer = upload;
            *offset = start;
            upload->used = start + size;
            return 1;
        }
        if (!upload->used && (!spare || upload->size > spare->size)) spare = upload;
        link = &upload->next;
    }
    NtVkUpload *upload = spare;
    VkDeviceSize capacity = size > 256 * 1024 ? size : 256 * 1024;
    if (upload) {
        /* Untouched this frame; replay has already waited for the previous fence. */
        /* Leave the old capacity available for later batches in this frame. */
        capacity = ((size + 15) & ~(VkDeviceSize)15) + upload->size;
        vk_destroy_upload(r, upload);
        *upload = (NtVkUpload){.next = upload->next};
    } else {
        upload = calloc(1, sizeof *upload);
        if (!upload) return fail(r, "Cannot allocate Vulkan upload buffer");
        *link = upload;
    }
    upload->size = capacity;
    VkBufferCreateInfo info = {
        .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = upload->size,
        .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT | VK_BUFFER_USAGE_VERTEX_BUFFER_BIT,
        .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
    };
    VK_CHECK(vkCreateBuffer(v->device, &info, NULL, &upload->buffer));
    VkMemoryRequirements requirements;
    vkGetBufferMemoryRequirements(v->device, upload->buffer, &requirements);
    VkMemoryAllocateInfo memory = {
        .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = requirements.size,
    };
    if (!vk_memory_type(r, requirements.memoryTypeBits,
                        VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT,
                        &memory.memoryTypeIndex)) return 0;
    VK_CHECK(vkAllocateMemory(v->device, &memory, NULL, &upload->memory));
    VK_CHECK(vkBindBufferMemory(v->device, upload->buffer, upload->memory, 0));
    void *mapped;
    VK_CHECK(vkMapMemory(v->device, upload->memory, 0, VK_WHOLE_SIZE, 0, &mapped));
    upload->mapped = mapped;
    upload->used = size;
    *buffer = upload;
    *offset = 0;
    return 1;
}

static int upload_glyph(NtRenderer *r, int color, int x, int y,
                         int width, int height, cairo_surface_t *surface) {
    NtVulkan *v = &r->vk;
    if (r->texture_size[color] != r->atlas_size[color] &&
        !vk_create_atlas(r, color, r->atlas_size[color])) return 0;
    NtVkAtlas *atlas = v->atlas[color];
    vk_atlas_transfer(r, atlas);
    NtVkUpload *upload;
    VkDeviceSize offset;
    unsigned char *pixels = cairo_image_surface_get_data(surface);
    int stride = cairo_image_surface_get_stride(surface);
    if (!vk_upload_space(r, (size_t)stride * height, &upload, &offset)) return 0;
    memcpy(upload->mapped + offset, pixels, (size_t)stride * height);
    VkBufferImageCopy copy = {
        .bufferOffset = offset,
        .bufferRowLength = (uint32_t)(stride / (color ? 4 : 1)),
        .imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
        .imageOffset = {x, y, 0}, .imageExtent = {(uint32_t)width, (uint32_t)height, 1},
    };
    vkCmdCopyBufferToImage(v->command, upload->buffer, atlas->image,
                           VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, 1, &copy);
    return 1;
}

static void vk_begin_rendering(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    if (v->rendering) return;
    VkRenderingAttachmentInfo color = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO,
        .imageView = v->views[v->image_index], .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        .loadOp = VK_ATTACHMENT_LOAD_OP_LOAD, .storeOp = VK_ATTACHMENT_STORE_OP_STORE,
    };
    VkRenderingInfo rendering = {
        .sType = VK_STRUCTURE_TYPE_RENDERING_INFO,
        .renderArea = {{0, 0}, v->extent}, .layerCount = 1,
        .colorAttachmentCount = 1, .pColorAttachments = &color,
    };
    vkCmdBeginRendering(v->command, &rendering);
    v->rendering = 1;
}

static int flush_quads(NtRenderer *r) {
    size_t count = r->quad_count;
    if (!count) return 1;
    if (count > UINT32_MAX || count > SIZE_MAX / sizeof(Quad))
        return fail(r, "Vulkan quad batch is too large");
    NtVulkan *v = &r->vk;
    for (int i = 0; i < 2; ++i) {
        NtVkAtlas *atlas = v->atlas[i];
        if (!atlas) continue;
        if (atlas->layout == VK_IMAGE_LAYOUT_UNDEFINED) vk_atlas_transfer(r, atlas);
        if (atlas->layout != VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL) continue;
        vk_image_barrier(r, atlas->image, atlas->layout, VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
                         VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT,
                         VK_ACCESS_TRANSFER_WRITE_BIT, VK_ACCESS_SHADER_READ_BIT);
        atlas->layout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL;
    }
    if (!v->descriptors) {
        VkDescriptorSetAllocateInfo allocate = {
            .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
            .descriptorPool = v->descriptor_pool, .descriptorSetCount = 1,
            .pSetLayouts = &v->descriptor_layout,
        };
        VK_CHECK(vkAllocateDescriptorSets(v->device, &allocate, &v->descriptors));
        VkDescriptorImageInfo images[2];
        VkWriteDescriptorSet writes[2];
        /* Keep both bindings valid before the first color glyph upload. */
        for (uint32_t i = 0; i < 2; ++i) {
            NtVkAtlas *atlas = v->atlas[i] ? v->atlas[i] : v->atlas[0];
            images[i] = (VkDescriptorImageInfo){v->sampler, atlas->view,
                                               VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL};
            writes[i] = (VkWriteDescriptorSet){
                .sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
                .dstSet = v->descriptors, .dstBinding = i, .descriptorCount = 1,
                .descriptorType = VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                .pImageInfo = &images[i],
            };
        }
        vkUpdateDescriptorSets(v->device, 2, writes, 0, NULL);
        vkCmdBindDescriptorSets(v->command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                                 v->pipeline_layout, 0, 1, &v->descriptors, 0, NULL);
    }
    NtVkUpload *upload;
    VkDeviceSize offset;
    if (!vk_upload_space(r, count * sizeof(Quad), &upload, &offset)) return 0;
    memcpy(upload->mapped + offset, r->quads, count * sizeof(Quad));
    vk_begin_rendering(r);
    vkCmdSetScissor(v->command, 0, 1, &v->clip);
    vkCmdBindVertexBuffers(v->command, 0, 1, &upload->buffer, &offset);
    vkCmdDraw(v->command, 4, (uint32_t)count, 0, 0);
    r->quad_count = 0;
    return 1;
}

static int vk_create_pipeline(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    VkShaderModuleCreateInfo module = {
        .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = sizeof nt_vertex_spv, .pCode = nt_vertex_spv,
    };
    VkShaderModule vertex, fragment;
    VK_CHECK(vkCreateShaderModule(v->device, &module, NULL, &vertex));
    module.codeSize = sizeof nt_fragment_spv;
    module.pCode = nt_fragment_spv;
    VkResult result = vkCreateShaderModule(v->device, &module, NULL, &fragment);
    if (result != VK_SUCCESS) {
        vkDestroyShaderModule(v->device, vertex, NULL);
        return fail(r, "Creating Vulkan fragment shader: %d", result);
    }
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
        .colorAttachmentCount = 1, .pColorAttachmentFormats = &v->format,
    };
    VkGraphicsPipelineCreateInfo pipeline = {
        .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &rendering,
        .stageCount = 2, .pStages = stages, .pVertexInputState = &vertex_input,
        .pInputAssemblyState = &assembly, .pViewportState = &viewport,
        .pRasterizationState = &raster, .pMultisampleState = &multisample,
        .pColorBlendState = &blend, .pDynamicState = &dynamic, .layout = v->pipeline_layout,
    };
    result = vkCreateGraphicsPipelines(v->device, VK_NULL_HANDLE, 1, &pipeline, NULL, &v->pipeline);
    vkDestroyShaderModule(v->device, fragment, NULL);
    vkDestroyShaderModule(v->device, vertex, NULL);
    return result == VK_SUCCESS || fail(r, "Creating Vulkan graphics pipeline: %d", result);
}

static void vk_free_swapchain(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    for (uint32_t i = 0; i < v->image_count; ++i) {
        if (v->views && v->views[i]) vkDestroyImageView(v->device, v->views[i], NULL);
        if (v->presented && v->presented[i]) vkDestroySemaphore(v->device, v->presented[i], NULL);
    }
    free(v->images);
    free(v->views);
    free(v->presented);
    v->images = NULL;
    v->views = NULL;
    v->presented = NULL;
    v->image_count = 0;
    if (v->swapchain) vkDestroySwapchainKHR(v->device, v->swapchain, NULL);
    v->swapchain = VK_NULL_HANDLE;
}

static int vk_create_swapchain(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    VK_CHECK(vkDeviceWaitIdle(v->device));
    vk_free_swapchain(r);
    VkSurfaceCapabilitiesKHR caps;
    VK_CHECK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(v->physical, v->surface, &caps));
    uint32_t count;
    VK_CHECK(vkGetPhysicalDeviceSurfaceFormatsKHR(v->physical, v->surface, &count, NULL));
    VkSurfaceFormatKHR *formats = malloc(count * sizeof *formats);
    if (!formats) return fail(r, "Cannot allocate Vulkan surface formats");
    VkResult result = vkGetPhysicalDeviceSurfaceFormatsKHR(v->physical, v->surface, &count, formats);
    VkSurfaceFormatKHR format = {VK_FORMAT_UNDEFINED, VK_COLOR_SPACE_SRGB_NONLINEAR_KHR};
    for (uint32_t i = 0; result == VK_SUCCESS && i < count; ++i) {
        if ((formats[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
             formats[i].format == VK_FORMAT_R8G8B8A8_UNORM) &&
            formats[i].colorSpace == VK_COLOR_SPACE_SRGB_NONLINEAR_KHR) {
            format = formats[i];
            break;
        }
    }
    free(formats);
    if (result != VK_SUCCESS) return fail(r, "Querying Vulkan surface formats: %d", result);
    if (format.format == VK_FORMAT_UNDEFINED) return fail(r, "Wayland surface has no Vulkan RGBA8 UNORM format");
    VkExtent2D extent = caps.currentExtent;
    if (extent.width == UINT32_MAX) {
        extent.width = (uint32_t)r->frame.pixel_width;
        extent.height = (uint32_t)r->frame.pixel_height;
        if (extent.width < caps.minImageExtent.width) extent.width = caps.minImageExtent.width;
        if (extent.width > caps.maxImageExtent.width) extent.width = caps.maxImageExtent.width;
        if (extent.height < caps.minImageExtent.height) extent.height = caps.minImageExtent.height;
        if (extent.height > caps.maxImageExtent.height) extent.height = caps.maxImageExtent.height;
    }
    VkCompositeAlphaFlagBitsKHR alpha = VK_COMPOSITE_ALPHA_PRE_MULTIPLIED_BIT_KHR;
    if (!(caps.supportedCompositeAlpha & alpha))
        return fail(r, "Vulkan Wayland surface lacks premultiplied alpha");
    count = caps.minImageCount + 1;
    if (caps.maxImageCount && count > caps.maxImageCount) count = caps.maxImageCount;
    /* VK_KHR_wayland_surface requires MAILBOX support. */
    VkSwapchainCreateInfoKHR swapchain = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = v->surface, .minImageCount = count,
        .imageFormat = format.format, .imageColorSpace = format.colorSpace,
        .imageExtent = extent, .imageArrayLayers = 1,
        .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = caps.currentTransform, .compositeAlpha = alpha,
        .presentMode = VK_PRESENT_MODE_MAILBOX_KHR, .clipped = VK_TRUE,
    };
    VK_CHECK(vkCreateSwapchainKHR(v->device, &swapchain, NULL, &v->swapchain));
    v->extent = extent;
    VK_CHECK(vkGetSwapchainImagesKHR(v->device, v->swapchain, &v->image_count, NULL));
    v->images = calloc(v->image_count, sizeof *v->images);
    v->views = calloc(v->image_count, sizeof *v->views);
    v->presented = calloc(v->image_count, sizeof *v->presented);
    if (!v->images || !v->views || !v->presented) return fail(r, "Cannot allocate Vulkan swapchain images");
    VK_CHECK(vkGetSwapchainImagesKHR(v->device, v->swapchain, &v->image_count, v->images));
    for (uint32_t i = 0; i < v->image_count; ++i) {
        VkImageViewCreateInfo view = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO,
            .image = v->images[i], .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = format.format,
            .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1},
        };
        VK_CHECK(vkCreateImageView(v->device, &view, NULL, &v->views[i]));
        VkSemaphoreCreateInfo semaphore = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
        VK_CHECK(vkCreateSemaphore(v->device, &semaphore, NULL, &v->presented[i]));
    }
    if (v->format != format.format) {
        if (v->pipeline) vkDestroyPipeline(v->device, v->pipeline, NULL);
        v->pipeline = VK_NULL_HANDLE;
        v->format = format.format;
        if (!vk_create_pipeline(r)) return 0;
    }
    v->resize = 0;
    return 1;
}

static void vk_free_atlas(NtRenderer *r, NtVkAtlas *atlas) {
    if (atlas->view) vkDestroyImageView(r->vk.device, atlas->view, NULL);
    if (atlas->image) vkDestroyImage(r->vk.device, atlas->image, NULL);
    if (atlas->memory) vkFreeMemory(r->vk.device, atlas->memory, NULL);
    free(atlas);
}

static int gpu_init(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    const char *extensions[] = {VK_KHR_SURFACE_EXTENSION_NAME, VK_KHR_WAYLAND_SURFACE_EXTENSION_NAME};
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "nextty", .apiVersion = VK_API_VERSION_1_3,
    };
    VkInstanceCreateInfo instance = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app,
        .enabledExtensionCount = 2, .ppEnabledExtensionNames = extensions,
    };
    VK_CHECK(vkCreateInstance(&instance, NULL, &v->instance));
    VkWaylandSurfaceCreateInfoKHR surface = {
        .sType = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = nt_window_display(r->window), .surface = nt_window_surface(r->window),
    };
    VK_CHECK(vkCreateWaylandSurfaceKHR(v->instance, &surface, NULL, &v->surface));
    uint32_t count;
    VK_CHECK(vkEnumeratePhysicalDevices(v->instance, &count, NULL));
    VkPhysicalDevice *devices = malloc(count * sizeof *devices);
    if (!devices) return fail(r, "Cannot allocate Vulkan physical devices");
    VkResult result = vkEnumeratePhysicalDevices(v->instance, &count, devices);
    int selected_score = 0;
    for (uint32_t i = 0; result == VK_SUCCESS && i < count; ++i) {
        VkPhysicalDeviceProperties properties;
        vkGetPhysicalDeviceProperties(devices[i], &properties);
        if (properties.apiVersion < VK_API_VERSION_1_3) continue;
        VkPhysicalDeviceVulkan13Features supported = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES};
        VkPhysicalDeviceFeatures2 features = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2, .pNext = &supported};
        vkGetPhysicalDeviceFeatures2(devices[i], &features);
        if (!supported.dynamicRendering) continue;
        int score = properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU ? 3 :
                    properties.deviceType == VK_PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU ? 2 : 1;
        if (score <= selected_score) continue;
        uint32_t families;
        vkGetPhysicalDeviceQueueFamilyProperties(devices[i], &families, NULL);
        VkQueueFamilyProperties *queues = malloc(families * sizeof *queues);
        if (!queues) { free(devices); return fail(r, "Cannot allocate Vulkan queue families"); }
        vkGetPhysicalDeviceQueueFamilyProperties(devices[i], &families, queues);
        for (uint32_t j = 0; j < families; ++j) {
            VkBool32 present;
            result = vkGetPhysicalDeviceSurfaceSupportKHR(devices[i], j, v->surface, &present);
            if (result != VK_SUCCESS) break;
            if ((queues[j].queueFlags & VK_QUEUE_GRAPHICS_BIT) && present) {
                v->physical = devices[i];
                v->queue_family = j;
                selected_score = score;
                break;
            }
        }
        free(queues);
    }
    free(devices);
    if (result != VK_SUCCESS) return fail(r, "Selecting Vulkan device: %d", result);
    if (!v->physical) return fail(r, "No Vulkan 1.3 graphics device with Wayland presentation");
    vkGetPhysicalDeviceMemoryProperties(v->physical, &v->memory);
    float priority = 1;
    VkDeviceQueueCreateInfo queue = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = v->queue_family, .queueCount = 1, .pQueuePriorities = &priority,
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
    VK_CHECK(vkCreateDevice(v->physical, &device, NULL, &v->device));
    vkGetDeviceQueue(v->device, v->queue_family, 0, &v->queue);
    VkCommandPoolCreateInfo pool = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = v->queue_family,
    };
    VK_CHECK(vkCreateCommandPool(v->device, &pool, NULL, &v->command_pool));
    VkCommandBufferAllocateInfo command = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = v->command_pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1,
    };
    VK_CHECK(vkAllocateCommandBuffers(v->device, &command, &v->command));
    VkFenceCreateInfo fence = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VK_CHECK(vkCreateFence(v->device, &fence, NULL, &v->fence));
    VkSemaphoreCreateInfo semaphore = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
    VK_CHECK(vkCreateSemaphore(v->device, &semaphore, NULL, &v->acquired));
    VkDescriptorSetLayoutBinding bindings[2];
    for (uint32_t i = 0; i < 2; ++i)
        bindings[i] = (VkDescriptorSetLayoutBinding){i, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER,
                                                    1, VK_SHADER_STAGE_FRAGMENT_BIT, NULL};
    VkDescriptorSetLayoutCreateInfo descriptors = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 2, .pBindings = bindings,
    };
    VK_CHECK(vkCreateDescriptorSetLayout(v->device, &descriptors, NULL, &v->descriptor_layout));
    VkPushConstantRange constants = {VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof(float) * 2};
    VkPipelineLayoutCreateInfo layout = {
        .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1, .pSetLayouts = &v->descriptor_layout,
        .pushConstantRangeCount = 1, .pPushConstantRanges = &constants,
    };
    VK_CHECK(vkCreatePipelineLayout(v->device, &layout, NULL, &v->pipeline_layout));
    /* Mask grows twice; color is created on first use and grows once. */
    VkDescriptorPoolSize size = {VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 10};
    VkDescriptorPoolCreateInfo descriptor_pool = {
        .sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 5, .poolSizeCount = 1, .pPoolSizes = &size,
    };
    VK_CHECK(vkCreateDescriptorPool(v->device, &descriptor_pool, NULL, &v->descriptor_pool));
    VkSamplerCreateInfo sampler = {
        .sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
        .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR,
        .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
        .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
    };
    VK_CHECK(vkCreateSampler(v->device, &sampler, NULL, &v->sampler));
    return vk_create_atlas(r, 0, r->atlas_size[0]);
}

static int replay(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    Frame *f = &r->frame;
    if (v->submitted) {
        VK_CHECK(vkWaitForFences(v->device, 1, &v->fence, VK_TRUE, UINT64_MAX));
        v->submitted = 0;
    }
    while (v->retired) {
        NtVkAtlas *atlas = v->retired;
        v->retired = atlas->next;
        vk_free_atlas(r, atlas);
    }
    for (NtVkUpload *upload = v->uploads; upload; upload = upload->next) upload->used = 0;
    if (!v->swapchain || v->resize || v->extent.width != (uint32_t)f->pixel_width ||
        v->extent.height != (uint32_t)f->pixel_height)
        if (!vk_create_swapchain(r)) return 0;
    for (;;) {
        VkResult result = vkAcquireNextImageKHR(v->device, v->swapchain, UINT64_MAX,
                                                v->acquired, VK_NULL_HANDLE, &v->image_index);
        if (result == VK_SUCCESS || result == VK_SUBOPTIMAL_KHR) {
            v->resize = result == VK_SUBOPTIMAL_KHR;
            break;
        }
        if (result != VK_ERROR_OUT_OF_DATE_KHR) return fail(r, "Acquiring Vulkan frame: %d", result);
        if (!vk_create_swapchain(r)) return 0;
    }
    VkCommandBufferBeginInfo begin = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
    };
    VK_CHECK(vkBeginCommandBuffer(v->command, &begin));
    VkViewport viewport = {0, 0, (float)v->extent.width, (float)v->extent.height, 0, 1};
    vkCmdSetViewport(v->command, 0, 1, &viewport);
    vkCmdBindPipeline(v->command, VK_PIPELINE_BIND_POINT_GRAPHICS, v->pipeline);
    float logical_size[2] = {(float)f->width, (float)f->height};
    vkCmdPushConstants(v->command, v->pipeline_layout, VK_SHADER_STAGE_VERTEX_BIT, 0,
                        sizeof logical_size, logical_size);
    if (v->descriptors)
        vkCmdBindDescriptorSets(v->command, VK_PIPELINE_BIND_POINT_GRAPHICS,
                                 v->pipeline_layout, 0, 1, &v->descriptors, 0, NULL);
    /* The acquire semaphore waits at color output, including this transition. */
    vk_image_barrier(r, v->images[v->image_index], VK_IMAGE_LAYOUT_UNDEFINED,
                     VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                     VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT,
                     0, VK_ACCESS_COLOR_ATTACHMENT_READ_BIT | VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT);
    v->clip = (VkRect2D){{0, 0}, v->extent};
    r->quad_count = 0;
    if (!set_scale(r, f->scale)) return 0;
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
            if (v->clip.extent.width && v->clip.extent.height) {
                vk_begin_rendering(r);
                VkClearRect rect = {v->clip, 0, 1};
                vkCmdClearAttachments(v->command, 1, &clear, 1, &rect);
            }
            break;
        }
        case OP_CLIP: {
            if (!flush_quads(r)) return 0;
            if (op->clip[2] <= 0 || op->clip[3] <= 0) {
                v->clip = (VkRect2D){{0, 0}, v->extent};
            } else {
                double sx = (double)v->extent.width / f->width;
                double sy = (double)v->extent.height / f->height;
                int left = (int)fmax(0, fmin(v->extent.width, floor(op->clip[0] * sx)));
                int top = (int)fmax(0, fmin(v->extent.height, floor(op->clip[1] * sy)));
                int right = (int)fmax(left, fmin(v->extent.width, ceil((op->clip[0] + op->clip[2]) * sx)));
                int bottom = (int)fmax(top, fmin(v->extent.height, ceil((op->clip[1] + op->clip[3]) * sy)));
                v->clip = (VkRect2D){{left, top}, {(uint32_t)(right - left), (uint32_t)(bottom - top)}};
            }
            break;
        }
        case OP_QUAD:
            if (!append_quad(r, op->quad)) return 0;
            break;
        case OP_LINE:
            if (!draw_line(r, op->line.line, op->line.x, op->line.y, op->line.factor)) return 0;
            break;
        }
    }
    return flush_quads(r);
}

static int gpu_present(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    if (v->rendering) {
        vkCmdEndRendering(v->command);
        v->rendering = 0;
    }
    vk_image_barrier(r, v->images[v->image_index], VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
                     VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
                     VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                     VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, 0);
    VK_CHECK(vkEndCommandBuffer(v->command));
    VK_CHECK(vkResetFences(v->device, 1, &v->fence));
    VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
    VkSubmitInfo submit = {
        .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
        .waitSemaphoreCount = 1, .pWaitSemaphores = &v->acquired, .pWaitDstStageMask = &wait_stage,
        .commandBufferCount = 1, .pCommandBuffers = &v->command,
        .signalSemaphoreCount = 1, .pSignalSemaphores = &v->presented[v->image_index],
    };
    VK_CHECK(vkQueueSubmit(v->queue, 1, &submit, v->fence));
    v->submitted = 1;
    VkPresentInfoKHR present = {
        .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
        .waitSemaphoreCount = 1, .pWaitSemaphores = &v->presented[v->image_index],
        .swapchainCount = 1, .pSwapchains = &v->swapchain, .pImageIndices = &v->image_index,
    };
    VkResult result = vkQueuePresentKHR(v->queue, &present);
    if (result == VK_SUBOPTIMAL_KHR || result == VK_ERROR_OUT_OF_DATE_KHR) {
        v->resize = 1;
        return 1;
    }
    return result == VK_SUCCESS || fail(r, "Presenting Vulkan frame: %d", result);
}

static void gpu_free(NtRenderer *r) {
    NtVulkan *v = &r->vk;
    if (v->device) {
        VkResult result = vkDeviceWaitIdle(v->device);
        if (result != VK_SUCCESS) fail(r, "Waiting for Vulkan shutdown: %d", result);
        vk_free_swapchain(r);
        for (int i = 0; i < 2; ++i) if (v->atlas[i]) vk_free_atlas(r, v->atlas[i]);
        while (v->retired) {
            NtVkAtlas *atlas = v->retired;
            v->retired = atlas->next;
            vk_free_atlas(r, atlas);
        }
        while (v->uploads) {
            NtVkUpload *upload = v->uploads;
            v->uploads = upload->next;
            vk_destroy_upload(r, upload);
            free(upload);
        }
        if (v->pipeline) vkDestroyPipeline(v->device, v->pipeline, NULL);
        if (v->pipeline_layout) vkDestroyPipelineLayout(v->device, v->pipeline_layout, NULL);
        if (v->descriptor_pool) vkDestroyDescriptorPool(v->device, v->descriptor_pool, NULL);
        if (v->descriptor_layout) vkDestroyDescriptorSetLayout(v->device, v->descriptor_layout, NULL);
        if (v->sampler) vkDestroySampler(v->device, v->sampler, NULL);
        if (v->acquired) vkDestroySemaphore(v->device, v->acquired, NULL);
        if (v->fence) vkDestroyFence(v->device, v->fence, NULL);
        if (v->command_pool) vkDestroyCommandPool(v->device, v->command_pool, NULL);
        vkDestroyDevice(v->device, NULL);
    }
    if (v->surface) vkDestroySurfaceKHR(v->instance, v->surface, NULL);
    if (v->instance) vkDestroyInstance(v->instance, NULL);
}

#undef VK_CHECK

static void *submit(void *data) {
    NtRenderer *r = data;
    Swap *s = &r->swap;
    /* Publish metrics first; GPU setup is the initial pending job. */
    r->fonts = nt_fonts_new(s->family, r->font_size, r->error, sizeof r->error);
    int initialized = r->fonts != NULL;
    if (initialized) {
        nt_fonts_metrics(r->fonts, &r->cell_width, &r->cell_height);
        initialized = set_scale(r, r->frame.scale);
    }
    pthread_mutex_lock(&s->lock);
    s->initialized = initialized ? 1 : -1;
    pthread_cond_broadcast(&s->changed);
    for (;;) {
        while (s->state != SWAP_PENDING && !s->retired && !s->stopping)
            pthread_cond_wait(&s->changed, &s->lock);
        if (s->retired && s->state != SWAP_PENDING) {
            NtLine *retired = s->retired;
            s->retired = NULL;
            pthread_mutex_unlock(&s->lock);
            while (retired) {
                NtLine *next = retired->retired_next;
                destroy_line(retired);
                retired = next;
            }
            pthread_mutex_lock(&s->lock);
            continue;
        }
        if (s->stopping) break;
        pthread_mutex_unlock(&s->lock);
        s->end = 0;
        if (!r->vk.instance) {
            if (initialized) gpu_init(r);
        } else if (replay(r)) {
            gpu_present(r);
            s->end = submission_time();
        }
        pthread_mutex_lock(&s->lock);
        s->state = SWAP_DONE;
        uint64_t wake = 1;
        ssize_t count;
        do count = write(s->fd, &wake, sizeof wake); while (count < 0 && errno == EINTR);
        if (count < 0 && errno != EAGAIN)
            fail(r, "Notifying submission completion: %s", strerror(errno));
        pthread_cond_broadcast(&s->changed);
    }
    pthread_mutex_unlock(&s->lock);
    clear_glyphs(r);
    nt_fonts_free(r->fonts);
    free(r->quads);
    gpu_free(r);
    return NULL;
}

static int start_submission(NtRenderer *r) {
    Swap *s = &r->swap;
    int error = pthread_mutex_init(&s->lock, NULL);
    if (error) return fail(r, "Submission mutex: %s", strerror(error));
    error = pthread_cond_init(&s->changed, NULL);
    if (error) {
        pthread_mutex_destroy(&s->lock);
        return fail(r, "Submission condition: %s", strerror(error));
    }
    s->fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    if (s->fd < 0) error = errno;
    else error = pthread_create(&s->thread, NULL, submit, r);
    if (error) {
        if (s->fd >= 0) close(s->fd);
        pthread_cond_destroy(&s->changed); pthread_mutex_destroy(&s->lock);
        return fail(r, "Starting submission worker: %s", strerror(error));
    }
    s->started = 1;
    pthread_mutex_lock(&s->lock);
    while (!s->initialized) pthread_cond_wait(&s->changed, &s->lock);
    int initialized = s->initialized > 0;
    pthread_mutex_unlock(&s->lock);
    return initialized;
}

int nt_renderer_fd(NtRenderer *r) { return r->swap.fd; }
double nt_renderer_swap_time(NtRenderer *r) { return (double)r->swap.end / 1e9; }

int nt_renderer_ready(NtRenderer *r) {
    Swap *s = &r->swap;
    pthread_mutex_lock(&s->lock);
    if (s->state != SWAP_DONE) {
        int ready = s->state == SWAP_IDLE;
        pthread_mutex_unlock(&s->lock);
        return ready;
    }
    uint64_t wake;
    ssize_t count;
    do count = read(s->fd, &wake, sizeof wake); while (count < 0 && errno == EINTR);
    if (count < 0) fail(r, "Reading submission completion: %s", strerror(errno));
    s->state = SWAP_IDLE;
    pthread_mutex_unlock(&s->lock);
    clear_frame(&r->frame);
    if (r->error[0]) return -1;
    return 1;
}

int nt_renderer_finish(NtRenderer *r) {
    Swap *s = &r->swap;
    pthread_mutex_lock(&s->lock);
    while (s->state == SWAP_PENDING)
        pthread_cond_wait(&s->changed, &s->lock);
    pthread_mutex_unlock(&s->lock);
    return nt_renderer_ready(r) > 0;
}

NtRenderer *nt_renderer_new(NtWindow *window, const char *family, float font_size) {
    creation_error[0] = 0;
    if (!window || !family || !*family || !isfinite(font_size) || font_size <= 0) {
        fail(NULL, "Renderer requires a window, font family, and finite positive font size");
        return NULL;
    }
    NtRenderer *r = calloc(1, sizeof *r);
    if (!r) { fail(NULL, "Cannot allocate terminal renderer"); return NULL; }
    r->window = window;
    r->font_size = font_size;
    r->atlas_size[0] = 512;
    r->atlas_size[1] = 1024;
    r->swap.family = family;
    r->swap.state = SWAP_PENDING;
    nt_window_size(window, NULL, NULL, &r->frame.scale);
    if (!start_submission(r)) {
        memcpy(creation_error, r->error, sizeof creation_error);
        nt_renderer_free(r);
        return NULL;
    }
    return r;
}

void nt_renderer_free(NtRenderer *r) {
    if (!r) return;
    if (r->swap.started) {
        nt_renderer_finish(r);
        clear_frame(&r->frame);
        Swap *s = &r->swap;
        pthread_mutex_lock(&s->lock);
        s->stopping = 1;
        pthread_cond_signal(&s->changed);
        pthread_mutex_unlock(&s->lock);
        pthread_join(s->thread, NULL);
        close(s->fd);
        pthread_cond_destroy(&s->changed);
        pthread_mutex_destroy(&s->lock);
    }
    free(r->frame.ops);
    free(r);
}

void nt_renderer_metrics(NtRenderer *r, float *width, float *height) {
    if (width) *width = r->cell_width;
    if (height) *height = r->cell_height;
}

void nt_line_free(NtLine *line) {
    if (!line || --line->references) return;
    Swap *s = &line->renderer->swap;
    pthread_mutex_lock(&s->lock);
    line->retired_next = s->retired;
    s->retired = line;
    if (!line->retired_next) pthread_cond_signal(&s->changed);
    pthread_mutex_unlock(&s->lock);
}

NtLine *nt_line_new(NtRenderer *r, const char *text, size_t length,
                    const NtRun *runs, size_t run_count,
                    const uint16_t *columns) {
    size_t stride = columns ? sizeof *columns + 1 : 1;
    if (length > INT_MAX || (columns && length > (SIZE_MAX - 1) / (sizeof *columns + 1)) ||
        run_count > (SIZE_MAX - length * stride - 1) / sizeof *runs) {
        fail(r, "Shaped line is too large");
        return NULL;
    }
    NtLine *line = calloc(1, sizeof *line);
    if (!line) { fail(r, "Cannot allocate terminal line"); return NULL; }
    line->renderer = r;
    line->references = 1;
    line->length = length;
    line->run_count = run_count;
    size_t run_bytes = run_count * sizeof *runs;
    line->runs = malloc(run_bytes + length * stride + 1);
    if (!line->runs) {
        fail(r, "Cannot allocate terminal line input");
        free(line);
        return NULL;
    }
    line->columns = columns && length ? (uint16_t *)((char *)line->runs + run_bytes) : NULL;
    line->text = (char *)line->runs + run_bytes + (columns ? length * sizeof *columns : 0);
    memcpy(line->text, text, length);
    line->text[length] = 0;
    if (run_count) memcpy(line->runs, runs, run_count * sizeof *runs);
    if (columns && length) memcpy(line->columns, columns, length * sizeof *columns);
    return line;
}

static int shape_line(NtRenderer *r, NtLine *line) {
    if (!line->text) return 1;
    int success = nt_fonts_shape(r->fonts, line->text, line->length,
                                line->runs, line->run_count, line->columns,
                                &line->glyphs, &line->count);
    free_line_input(line);
    return success;
}

static Quad make_quad(float x, float y, float width, float height, uint32_t color, float mode) {
    return (Quad){ .rect = {x, y, width, height}, .color = {
        (unsigned char)(color >> 24), (unsigned char)(color >> 16),
        (unsigned char)(color >> 8), (unsigned char)color}, .mode = mode };
}

static int append_quad(NtRenderer *r, Quad q) {
    if (r->quad_count == r->quad_capacity) {
        size_t capacity = r->quad_capacity ? r->quad_capacity * 2 : 1024;
        if (capacity < r->quad_capacity || capacity > SIZE_MAX / sizeof(Quad))
            return fail(r, "GPU rectangle batch is too large");
        Quad *quads = realloc(r->quads, capacity * sizeof *quads);
        if (!quads) return fail(r, "Cannot allocate GPU rectangle batch");
        r->quads = quads;
        r->quad_capacity = capacity;
    }
    r->quads[r->quad_count++] = q;
    return 1;
}

static CachedGlyph *cached_glyph(NtRenderer *r, const NtFontGlyph *glyph) {
    size_t hash = (((uintptr_t)glyph->font >> 4) ^ ((size_t)glyph->glyph * 2654435761u) ^ glyph->color_glyph) & (GLYPH_SLOTS - 1);
    size_t slot = hash;
    while (r->glyphs[slot].font) {
        CachedGlyph *entry = &r->glyphs[slot];
        if (entry->font == glyph->font && entry->glyph == glyph->glyph && entry->color == glyph->color_glyph)
            return entry;
        slot = (slot + 1) & (GLYPH_SLOTS - 1);
    }
    if (r->glyph_count >= GLYPH_SLOTS * 3 / 4) {
        if (!reset_atlases(r)) return NULL;
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
    int color = glyph->color_glyph != 0;
    if (!ink.width || !ink.height) width = height = 0;
    if (width > ATLAS_MAX || height > ATLAS_MAX || width < 0 || height < 0) {
        fail(r, "Glyph raster %dx%d exceeds %dx%d atlas", width, height, ATLAS_MAX, ATLAS_MAX);
        return NULL;
    }
    if (r->atlas_x[color] + width > r->atlas_size[color]) {
        r->atlas_x[color] = 0;
        r->atlas_y[color] += r->atlas_row[color];
        r->atlas_row[color] = 0;
    }
    if (width > r->atlas_size[color] || r->atlas_y[color] + height > r->atlas_size[color]) {
        if (!reset_atlases(r)) return NULL;
        if (r->atlas_size[color] < ATLAS_MAX) r->atlas_size[color] *= 2;
        while (width > r->atlas_size[color] || height > r->atlas_size[color])
            r->atlas_size[color] *= 2;
        slot = hash;
    }
    int x = r->atlas_x[color], y = r->atlas_y[color];
    if (width && height) {
        cairo_surface_t *surface = cairo_image_surface_create(
            color ? CAIRO_FORMAT_ARGB32 : CAIRO_FORMAT_A8, width, height);
        cairo_t *cr = cairo_create(surface);
        cairo_set_source_rgba(cr, 1, 1, 1, 1);
        cairo_set_scaled_font(cr, glyph->font);
        single.x = -left;
        single.y = -top;
        cairo_show_glyphs(cr, &single, 1);
        if (!finish_raster(r, cr, surface)) {
            cairo_surface_destroy(surface);
            return NULL;
        }
        int uploaded = upload_glyph(r, color, x, y, width, height, surface);
        cairo_surface_destroy(surface);
        if (!uploaded) return NULL;
        r->atlas_x[color] += width;
        if (r->atlas_row[color] < height) r->atlas_row[color] = height;
    }
    CachedGlyph *entry = &r->glyphs[slot];
    *entry = (CachedGlyph){ .font = cairo_scaled_font_reference(glyph->font), .glyph = glyph->glyph,
        .color = glyph->color_glyph, .x = x, .y = y, .width = width,
        .height = height, .left = left, .top = top };
    ++r->glyph_count;
    return entry;
}

int nt_renderer_clear(NtRenderer *r, uint32_t background) {
    if (r->error[0]) return 0;
    return add_op(r, (Op){.kind = OP_CLEAR, .background = background});
}

int nt_renderer_begin(NtRenderer *r, uint32_t background) {
    r->error[0] = 0;
    Frame *f = &r->frame;
    clear_frame(f);
    nt_window_size(r->window, &f->width, &f->height, &f->scale);
    if (f->width <= 0 || f->height <= 0) return fail(r, "Cannot render an empty window");
    f->pixel_width = (int)lround(f->width * f->scale);
    f->pixel_height = (int)lround(f->height * f->scale);
    nt_window_prepare_frame(r->window);
    return nt_renderer_clear(r, background);
}

int nt_renderer_clip(NtRenderer *r, float x, float y, float width, float height) {
    if (r->error[0]) return 0;
    return add_op(r, (Op){.kind = OP_CLIP, .clip = {x, y, width, height}});
}

void nt_renderer_rect(NtRenderer *r, float x, float y, float width, float height,
                      uint32_t color, int wave) {
    if (r->error[0] || width <= 0 || height <= 0 || !(color & 255)) return;
    add_op(r, (Op){.kind = OP_QUAD, .quad = make_quad(x, y, width, height, color, wave ? 3 : 0)});
}

void nt_renderer_stroke(NtRenderer *r, float x1, float y1, float x2, float y2,
                        float width, uint32_t color) {
    if (r->error[0] || width <= 0 || !(color & 255)) return;
    float radius = width * 0.5f, padding = radius + 1.0f;
    float dx = x2 - x1, dy = y2 - y1;
    Op op = {.kind = OP_QUAD, .quad = make_quad(
        fminf(x1, x2) - padding, fminf(y1, y2) - padding,
        fabsf(dx) + padding * 2.0f, fabsf(dy) + padding * 2.0f,
        color, dx * dy < 0 ? 7 : 6)};
    op.quad.uv[0] = radius;
    add_op(r, op);
}

void nt_renderer_round_rect(NtRenderer *r, float x, float y, float width, float height,
                            float radius, uint32_t color) {
    if (r->error[0] || width <= 0 || height <= 0) return;
    Op op = {.kind = OP_QUAD, .quad = make_quad(x, y, width, height, color, 4)};
    op.quad.uv[0] = fminf(radius, fminf(width, height) * 0.5f);
    add_op(r, op);
}

void nt_renderer_tab(NtRenderer *r, float x, float y, float width, float height,
                     float inset, uint32_t color) {
    if (r->error[0] || width <= 0 || height <= 0) return;
    Op op = {.kind = OP_QUAD, .quad = make_quad(x, y, width, height, color, 5)};
    op.quad.uv[0] = fminf(inset, width * 0.5f);
    add_op(r, op);
}

void nt_renderer_line(NtRenderer *r, const NtLine *line, float x, float y, float factor) {
    if (r->error[0] || (!line->text && !line->count)) return;
    if (add_op(r, (Op){.kind = OP_LINE, .line = {(NtLine *)line, x, y, factor}}))
        ++((NtLine *)line)->references;
}

static int draw_line(NtRenderer *r, NtLine *line, float x, float y, float factor) {
    if (!shape_line(r, line)) return 0;
    double inverse_scale = 1. / r->scale;
    float stroke = fmaxf(1.f, roundf(r->font_size * r->scale / 16.f)) / r->scale;
    float top = 0;
    int top_ready = 0;
    for (size_t i = 0; i < line->count; ++i) {
        const NtFontGlyph *glyph = &line->glyphs[i];
        if (!(glyph->color & 255)) continue;
        Quad q;
        if (glyph->straight) {
            if (glyph->straight == 0x2502) {
                float left = roundf((x + glyph->x + (r->cell_width - stroke) * 0.5f) * r->scale) / r->scale;
                q = make_quad(left, y, stroke, r->cell_height, glyph->color, 0);
            } else {
                if (!top_ready) {
                    top = roundf((y + (r->cell_height - stroke) * 0.5f) * r->scale) / r->scale;
                    top_ready = 1;
                }
                q = make_quad(x + glyph->x, top, r->cell_width, stroke, glyph->color, 0);
            }
        } else {
            CachedGlyph *entry = cached_glyph(r, glyph);
            if (!entry) return 0;
            if (!entry->width || !entry->height) continue;
            /* Keep filtering inside this tile's transparent border after atlas reuse. */
            q = make_quad(x + glyph->x + (float)((entry->left + 0.5) * inverse_scale),
                       y + glyph->y + (float)((entry->top + 0.5) * inverse_scale),
                       (float)((entry->width - 1) * inverse_scale), (float)((entry->height - 1) * inverse_scale),
                       glyph->color, entry->color ? 2 : 1);
            float inverse_atlas = 1.f / r->atlas_size[entry->color != 0];
            q.uv[0] = (entry->x + 0.5f) * inverse_atlas;
            q.uv[1] = (entry->y + 0.5f) * inverse_atlas;
            q.uv[2] = (entry->width - 1) * inverse_atlas;
            q.uv[3] = (entry->height - 1) * inverse_atlas;
        }
        if (factor != 1.f) {
            q.rect[0] = x + (q.rect[0] - x) * factor;
            q.rect[1] = y + (q.rect[1] - y) * factor;
            q.rect[2] *= factor;
            q.rect[3] *= factor;
        }
        if (!append_quad(r, q)) return 0;
    }
    return 1;
}

int nt_renderer_present(NtRenderer *r) {
    if (r->error[0]) {
        clear_frame(&r->frame);
        return 0;
    }
    Swap *s = &r->swap;
    pthread_mutex_lock(&s->lock);
    s->state = SWAP_PENDING;
    pthread_cond_signal(&s->changed);
    pthread_mutex_unlock(&s->lock);
    return 1;
}
