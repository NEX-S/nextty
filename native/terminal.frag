#version 450
layout(set = 0, binding = 0) uniform sampler2D mask_atlas;
layout(set = 0, binding = 1) uniform sampler2D color_atlas;
layout(location = 0) in vec2 texcoord;
layout(location = 1) in vec4 rgba;
layout(location = 2) flat in int kind;
layout(location = 3) flat in vec3 shape;
layout(location = 0) out vec4 pixel;
void main() {
    if (kind == 2) {
        pixel = texture(color_atlas, texcoord).bgra * rgba.a;
        return;
    }
    float a = rgba.a;
    if (kind == 1) a *= texture(mask_atlas, texcoord).r;
    if (kind == 4) {
        vec2 q = abs(texcoord - shape.xy * .5) - shape.xy * .5 + shape.z;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - shape.z;
        a *= clamp(.5 - d / max(fwidth(d), .001), 0.0, 1.0);
    }
    if (kind == 5) {
        float edge = shape.z * (1.0 - texcoord.y / shape.y);
        float d = min(texcoord.x, shape.x - texcoord.x) - edge;
        a *= clamp(.5 + d / max(fwidth(d), .001), 0.0, 1.0);
    }
    if (kind == 6 || kind == 7) {
        float padding = shape.z + 1.0;
        vec2 start = vec2(padding, kind == 7 ? shape.y - padding : padding);
        vec2 direction = shape.xy - start * 2.0;
        vec2 point = texcoord - start;
        float t = clamp(dot(point, direction) / max(dot(direction, direction), .000001), 0.0, 1.0);
        float d = length(point - direction * t) - shape.z;
        a *= clamp(.5 - d / max(fwidth(d), .001), 0.0, 1.0);
    }
    if (kind == 3) {
        float center = shape.y * .5 + sin(texcoord.x * 2.0943951) * max(0.0, shape.y * .5 - .5);
        float d = abs(texcoord.y - center);
        a *= clamp((.5 - d) / max(fwidth(d), .001) + .5, 0.0, 1.0);
    }
    pixel = vec4(rgba.rgb * a, a);
}
