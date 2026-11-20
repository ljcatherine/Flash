//! GPU benchmarks, run on the OpenGL context the window already uses. Work happens in
//! short batches each frame so the window stays responsive while the GPU is loaded.
use eframe::glow::{self, HasContext};
use flash::bench::Test;
use std::time::Instant;

const SECONDS: f64 = 3.0;
const BATCH: f64 = 0.05; // aim for ~50 ms of GPU work per frame

// Full-screen triangle from the vertex index alone, so no vertex buffer is needed.
const VS: &str = "#version 330 core
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}";

// Two dependent vec4 multiply-adds per iteration = 16 floating-point operations.
// `k` comes from a uniform so the compiler can't fold the loop away.
const FS_COMPUTE: &str = "#version 330 core
uniform int iters;
uniform float k;
out vec4 color;
void main() {
    vec4 a = vec4(gl_FragCoord.xy * 0.001, k, 1.0);
    vec4 b = a.yzwx + 0.5;
    vec4 c = vec4(k);
    for (int i = 0; i < iters; i++) {
        a = a * c + b;
        b = b * c + a;
    }
    color = a + b;
}";

const FS_FILL: &str = "#version 330 core
uniform int iters;
uniform float k;
out vec4 color;
void main() { color = vec4(k, 0.5, 0.25, 0.5); }";

const COMPUTE_ITERS: i32 = 1024;
const FLOPS_PER_ITER: f64 = 16.0;

pub struct GpuRun {
    pub test: Test,
    prog: glow::Program,
    vao: glow::VertexArray,
    fbo: glow::Framebuffer,
    tex: glow::Texture,
    w: i32,
    h: i32,
    passes: u32,
    warmed: bool,
    busy: f64,
    work: f64,
}

/// The GPU OpenGL is running on, e.g. "NVIDIA GeForce RTX 2060/PCIe/SSE2".
pub fn renderer(gl: &glow::Context) -> String {
    // SAFETY: plain string query on the current context.
    unsafe { gl.get_parameter_string(glow::RENDERER) }
}

unsafe fn shader(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader, String> {
    // SAFETY (whole fn): called on the UI thread with eframe's context current.
    unsafe {
        let s = gl.create_shader(kind)?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if gl.get_shader_compile_status(s) {
            Ok(s)
        } else {
            let log = gl.get_shader_info_log(s);
            gl.delete_shader(s);
            Err(format!("shader didn't compile: {log}"))
        }
    }
}

impl GpuRun {
    /// Builds the shader and offscreen target. Must run where eframe's GL context is current.
    pub fn new(gl: &glow::Context, test: Test) -> Result<GpuRun, String> {
        let (fs, w, h) = if test == Test::GpuFill { (FS_FILL, 3840, 2160) } else { (FS_COMPUTE, 1920, 1080) };
        // SAFETY: eframe calls App::update with its GL context current on this thread; every
        // object created here is deleted in finish().
        unsafe {
            let vs = shader(gl, glow::VERTEX_SHADER, VS)?;
            let fs = shader(gl, glow::FRAGMENT_SHADER, fs)?;
            let prog = gl.create_program()?;
            gl.attach_shader(prog, vs);
            gl.attach_shader(prog, fs);
            gl.link_program(prog);
            gl.delete_shader(vs);
            gl.delete_shader(fs);
            if !gl.get_program_link_status(prog) {
                let log = gl.get_program_info_log(prog);
                gl.delete_program(prog);
                return Err(format!("shader didn't link: {log}"));
            }
            let vao = gl.create_vertex_array()?;
            let tex = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w, h, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            gl.bind_texture(glow::TEXTURE_2D, None);
            let fbo = gl.create_framebuffer()?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
            let ok = gl.check_framebuffer_status(glow::FRAMEBUFFER) == glow::FRAMEBUFFER_COMPLETE;
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            let run = GpuRun { test, prog, vao, fbo, tex, w, h, passes: 1, warmed: false, busy: 0.0, work: 0.0 };
            if !ok {
                run.finish(gl);
                return Err("offscreen target not supported".into());
            }
            Ok(run)
        }
    }

    /// Runs one batch. Returns the score once enough GPU time has been measured.
    pub fn step(&mut self, gl: &glow::Context) -> Option<f64> {
        // SAFETY: same context as new(); state egui relies on is restored below
        // (egui_glow also re-sets its own state at the start of every paint).
        let dt = unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
            gl.viewport(0, 0, self.w, self.h);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            if self.test == Test::GpuFill {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            } else {
                gl.disable(glow::BLEND);
            }
            gl.use_program(Some(self.prog));
            gl.uniform_1_i32(gl.get_uniform_location(self.prog, "iters").as_ref(), COMPUTE_ITERS);
            gl.uniform_1_f32(gl.get_uniform_location(self.prog, "k").as_ref(), 0.999);
            gl.bind_vertex_array(Some(self.vao));
            let t = Instant::now();
            for _ in 0..self.passes {
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            gl.finish(); // wait for the GPU so the timing is real
            let dt = t.elapsed().as_secs_f64();
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.disable(glow::BLEND);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            dt
        };
        if self.warmed {
            let pixels = (self.w * self.h) as f64 * self.passes as f64;
            self.work += if self.test == Test::GpuFill { pixels } else { pixels * COMPUTE_ITERS as f64 * FLOPS_PER_ITER };
            self.busy += dt;
        }
        // The first batch compiles and warms up; after that, size batches to ~50 ms.
        self.warmed = true;
        self.passes = ((self.passes as f64 * BATCH / dt.max(1e-4)).round() as u32).clamp(1, 4000);
        (self.busy >= SECONDS).then(|| self.work / self.busy / 1e9)
    }

    pub fn finish(self, gl: &glow::Context) {
        // SAFETY: deleting the objects created in new(), on the same context.
        unsafe {
            gl.delete_framebuffer(self.fbo);
            gl.delete_texture(self.tex);
            gl.delete_vertex_array(self.vao);
            gl.delete_program(self.prog);
        }
    }
}
