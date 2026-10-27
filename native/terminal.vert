#version 450
layout(location = 0) in vec4 rect;
layout(location = 1) in vec4 uv;
layout(location = 2) in vec4 color;
layout(location = 3) in float mode;
layout(push_constant) uniform Viewport { vec2 size; } viewport;
layout(location = 0) out vec2 texcoord;
layout(location = 1) out vec4 rgba;
layout(location = 2) flat out int kind;
layout(location = 3) flat out vec3 shape;
void main() {
    vec2 corner = vec2(gl_VertexIndex & 1, gl_VertexIndex >> 1);
    vec2 p = rect.xy + corner * rect.zw;
    gl_Position = vec4(p / viewport.size * 2.0 - 1.0, 0.0, 1.0);
    kind = int(mode);
    rgba = color;
    shape = vec3(rect.zw, uv.x);
    texcoord = kind >= 4 ? corner * rect.zw :
        (kind == 3 ? vec2(p.x, corner.y * rect.w) : uv.xy + corner * uv.zw);
}
