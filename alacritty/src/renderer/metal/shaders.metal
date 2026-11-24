#include <metal_stdlib>
using namespace metal;

// These layouts match Instance and Uniforms in mod.rs (four-float slots).
struct Instance { float4 rect; float4 uv; float4 color; float4 params; };
struct Uniforms { float4 size; float4 cell; float4 decoration; };
struct Vertex {
    float4 position [[position]];
    float2 uv;
    float4 color [[flat]];
    float kind [[flat]];
};
vertex Vertex terminal_vertex(uint vertex_id [[vertex_id]], uint instance_id [[instance_id]],
                              const device Instance *items [[buffer(0)]],
                              constant Uniforms &u [[buffer(1)]]) {
    constexpr float2 corners[] = {float2(0, 0), float2(0, 1), float2(1, 0),
                                 float2(1, 0), float2(0, 1), float2(1, 1)};
    Instance item = items[instance_id];
    float2 corner = corners[vertex_id];
    float2 pixel = item.rect.xy + corner * item.rect.zw;
    Vertex out;
    out.position = float4(pixel.x * 2.0 / u.size.x - 1.0,
                          1.0 - pixel.y * 2.0 / u.size.y, 0, 1);
    out.uv = item.uv.xy + corner * item.uv.zw;
    out.color = item.color;
    out.kind = item.params.x;
    return out;
}
struct TextOutput {
    float4 color [[color(0), index(0)]];
    float4 mask [[color(0), index(1)]];
};
fragment TextOutput terminal_text(Vertex in [[stage_in]], texture2d<float> atlas [[texture(0)]]) {
    TextOutput out;
    if (in.kind == 0) {
        out.color = float4(in.color.rgb * in.color.a, in.color.a);
        out.mask = float4(1);
    } else {
        constexpr sampler glyph_sampler(coord::normalized, address::clamp_to_edge, filter::linear);
        float4 texel = atlas.sample(glyph_sampler, in.uv);
        if (in.kind == 2) {
            out.color = float4(texel.a > 0 ? texel.rgb / texel.a : float3(0), 1);
            out.mask = float4(texel.a);
        } else {
            out.color = float4(in.color.rgb, 1);
            out.mask = float4(texel.rgb, texel.r);
        }
    }
    return out;
}
fragment float4 terminal_rect(Vertex in [[stage_in]], constant Uniforms &u [[buffer(0)]]) {
    float4 color = in.color;
    // Preserve the OpenGL decoration shader's bottom-up pixel coordinates.
    float x = floor(fmod(in.position.x - u.size.z, u.cell.x));
    float viewport_height = u.size.y - u.size.w;
    float padding_y = viewport_height - floor(viewport_height / u.cell.y) * u.cell.y;
    float y = floor(fmod(u.size.y - in.position.y - padding_y, u.cell.y));
    float thickness = u.cell.w;
    if (in.kind == 3) {
        float curve = u.decoration.x / 2.0 * cos((x + 0.5) * 2.0 * M_PI_F / u.cell.x)
                    + u.decoration.x - 1.0;
        float half_width = max(thickness - 1.0, 0.0) / 2.0;
        float distance = max(y - curve - half_width, max(curve - half_width - y, 0.0));
        color.a = clamp(1.0 - distance * distance, 0.0, 1.0);
    } else if (in.kind == 4) {
        if (thickness < 2) {
            float even = int(fmod(u.cell.x, 2.0)) != 0
                ? fmod((in.position.x - u.size.z) / u.cell.x, 2.0) : 0;
            color.a = int(fmod(x, 2.0)) != int(even) ? 0 : clamp(1.0 - abs(floor(u.cell.z) - y), 0.0, 1.0);
        } else {
            float dot = floor(x / thickness);
            float center = (dot - fmod(dot, 2.0)) * thickness + thickness / 2.0;
            float2 p = float2(x, y);
            float d = min(distance(p, float2(center, u.cell.z - 1.0)),
                          distance(p, float2(center + 2.0 * thickness, u.cell.z - 1.0)));
            color.a = clamp(1.0 - (d - thickness / 2.0), 0.0, 1.0);
        }
    } else if (in.kind == 5) {
        float half_dash = floor(u.cell.x / 4.0 + 0.5);
        color.a = x > half_dash - 1.0 && x < u.cell.x - half_dash ? 0 : 1;
    }
    return color;
}
