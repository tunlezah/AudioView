//! GLSL ES 3.00 shaders.
//!
//! Two passes: a background fill over the whole panel, then the artwork quad
//! over it. Every fade is a multiply in the fragment shader rather than a
//! CRTC gamma ramp — gamma support is inconsistent across BCM2711 and
//! BCM2712, and a fade that silently no-ops on one board is a bad dependency
//! for the most visible transition in the product (DESIGN §6.1).

/// Shared vertex shader: a unit quad, scaled into place and rotated.
pub const VERTEX: &str = r#"#version 300 es
precision highp float;

layout(location = 0) in vec2 aPos;      // -1..1 unit quad

uniform vec4 uRect;      // cx, cy, half_w, half_h  (logical clip space)
uniform mat2 uRotation;  // logical -> physical
uniform vec2 uUvScale;
uniform vec2 uUvOffset;
uniform float uZoom;     // Ken Burns
// 1 when sampling an uploaded image, 0 when sampling a framebuffer texture.
// GL puts v=0 at the bottom, but an uploaded image's first row is its top, so
// artwork needs the V axis inverted and render targets do not. Getting this
// wrong displays every cover vertically mirrored.
uniform float uFlipV;

out vec2 vUv;

void main() {
    vec2 logical = vec2(uRect.x, uRect.y) + aPos * vec2(uRect.z, uRect.w);
    gl_Position = vec4(uRotation * logical, 0.0, 1.0);

    // Zoom about the centre of the sampled sub-rectangle, so Ken Burns
    // pushes in rather than sliding the crop off one edge.
    vec2 uv = aPos * 0.5 + 0.5;
    uv = (uv - 0.5) / uZoom + 0.5;
    uv.y = mix(uv.y, 1.0 - uv.y, uFlipV);
    vUv = uv * uUvScale + uUvOffset;
}
"#;

/// Artwork pass: blend the outgoing and incoming images, then apply the
/// global fade.
pub const ARTWORK_FRAGMENT: &str = r#"#version 300 es
precision highp float;

in vec2 vUv;
out vec4 fragColor;

uniform sampler2D uPrev;
uniform sampler2D uNext;
uniform float uMix;         // 0 = previous, 1 = current (already eased)
uniform float uGlobalFade;  // 1 = visible, 0 = black
uniform float uHasPrev;     // 0 when there is nothing to fade from

void main() {
    vec3 next = texture(uNext, vUv).rgb;
    vec3 prev = mix(next, texture(uPrev, vUv).rgb, uHasPrev);
    fragColor = vec4(mix(prev, next, uMix) * uGlobalFade, 1.0);
}
"#;

/// Background pass over the whole panel.
///
/// `uMode`: 0 black, 1 blurred image, 2 dominant colour, 3 gradient.
pub const BACKGROUND_FRAGMENT: &str = r#"#version 300 es
precision highp float;

in vec2 vUv;
out vec4 fragColor;

uniform sampler2D uBlur;
uniform int uMode;
uniform vec3 uColorA;
uniform vec3 uColorB;
uniform float uDim;
uniform float uGlobalFade;

void main() {
    vec3 c;
    if (uMode == 1) {
        c = texture(uBlur, vUv).rgb;
    } else if (uMode == 2) {
        c = uColorA;
    } else if (uMode == 3) {
        // Primary at the top: vUv.y = 1 is the top of an unflipped pass.
        c = mix(uColorA, uColorB, 1.0 - vUv.y);
    } else {
        c = vec3(0.0);
    }
    fragColor = vec4(c * uDim * uGlobalFade, 1.0);
}
"#;

/// One step of a dual-Kawase chain.
///
/// Chosen over a separable Gaussian because at 1920² on a VideoCore the
/// Gaussian is far too expensive; the Kawase chain reaches a comparable look
/// in a handful of half-resolution passes.
pub const KAWASE_FRAGMENT: &str = r#"#version 300 es
precision highp float;

in vec2 vUv;
out vec4 fragColor;

uniform sampler2D uSrc;
uniform vec2 uTexel;   // 1 / source size
uniform float uRadius;

void main() {
    vec2 o = uTexel * uRadius;
    vec3 sum = texture(uSrc, vUv).rgb * 4.0;
    sum += texture(uSrc, vUv + vec2(-o.x,  o.y)).rgb;
    sum += texture(uSrc, vUv + vec2( o.x,  o.y)).rgb;
    sum += texture(uSrc, vUv + vec2( o.x, -o.y)).rgb;
    sum += texture(uSrc, vUv + vec2(-o.x, -o.y)).rgb;
    fragColor = vec4(sum / 8.0, 1.0);
}
"#;

/// Pass-through used to blit a texture into an FBO.
pub const BLIT_FRAGMENT: &str = r#"#version 300 es
precision highp float;

in vec2 vUv;
out vec4 fragColor;

uniform sampler2D uSrc;

void main() {
    fragColor = vec4(texture(uSrc, vUv).rgb, 1.0);
}
"#;
