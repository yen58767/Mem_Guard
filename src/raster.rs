// 不用顯示卡：把 egui 輸出的三角形用 CPU 畫進像素緩衝。顏色都是預乘 alpha。

use egui::epaint::{ClippedPrimitive, ImageData, Primitive, TextureId};
use egui::TexturesDelta;
use std::collections::HashMap;

// 每個點只存 4 個位元組，取樣時才換成小數。
struct Tex {
    w: usize,
    h: usize,
    px: Vec<[u8; 4]>,
}

#[derive(Default)]
pub struct Raster {
    texs: HashMap<TextureId, Tex>,
}

impl Raster {
    pub fn update_textures(&mut self, delta: &TexturesDelta) {
        for (id, d) in &delta.set {
            let (w, h, px): (usize, usize, Vec<[u8; 4]>) = match &d.image {
                ImageData::Color(img) => (img.size[0], img.size[1], img.pixels.iter().map(|c| c.to_array()).collect()),
                ImageData::Font(f) => (f.size[0], f.size[1], f.srgba_pixels(None).map(|c| c.to_array()).collect()),
            };
            match (d.pos, self.texs.get_mut(id)) {
                (Some([x0, y0]), Some(t)) => {
                    for y in 0..h {
                        for x in 0..w {
                            if x0 + x < t.w && y0 + y < t.h {
                                t.px[(y0 + y) * t.w + x0 + x] = px[y * w + x];
                            }
                        }
                    }
                }
                _ => {
                    self.texs.insert(*id, Tex { w, h, px });
                }
            }
        }
    }

    pub fn free_textures(&mut self, delta: &TexturesDelta) {
        for id in &delta.free {
            self.texs.remove(id);
        }
    }

    pub fn draw(&self, buf: &mut [u32], bw: usize, bh: usize, prims: &[ClippedPrimitive], ppp: f32, clear: [u8; 3]) {
        let bg = (clear[0] as u32) << 16 | (clear[1] as u32) << 8 | clear[2] as u32;
        buf.fill(bg);
        for p in prims {
            let Primitive::Mesh(mesh) = &p.primitive else { continue };
            let Some(tex) = self.texs.get(&mesh.texture_id) else { continue };
            let cx0 = (p.clip_rect.min.x * ppp).max(0.0).floor() as i32;
            let cy0 = (p.clip_rect.min.y * ppp).max(0.0).floor() as i32;
            let cx1 = ((p.clip_rect.max.x * ppp).ceil() as i32).min(bw as i32);
            let cy1 = ((p.clip_rect.max.y * ppp).ceil() as i32).min(bh as i32);
            if cx0 >= cx1 || cy0 >= cy1 {
                continue;
            }
            for tri in mesh.indices.chunks_exact(3) {
                let v = [&mesh.vertices[tri[0] as usize], &mesh.vertices[tri[1] as usize], &mesh.vertices[tri[2] as usize]];
                let pos = [v[0].pos * ppp, v[1].pos * ppp, v[2].pos * ppp];
                let area = edge(pos[0].x, pos[0].y, pos[1].x, pos[1].y, pos[2].x, pos[2].y);
                if area.abs() < 1e-6 {
                    continue;
                }
                let minx = pos.iter().map(|p| p.x).fold(f32::MAX, f32::min).floor() as i32;
                let maxx = pos.iter().map(|p| p.x).fold(f32::MIN, f32::max).ceil() as i32;
                let miny = pos.iter().map(|p| p.y).fold(f32::MAX, f32::min).floor() as i32;
                let maxy = pos.iter().map(|p| p.y).fold(f32::MIN, f32::max).ceil() as i32;
                let (x0, x1, y0, y1) = (minx.max(cx0), maxx.min(cx1), miny.max(cy0), maxy.min(cy1));
                if x0 >= x1 || y0 >= y1 {
                    continue;
                }
                let col = [to_f(v[0].color.to_array()), to_f(v[1].color.to_array()), to_f(v[2].color.to_array())];
                let flat = col[0] == col[1] && col[1] == col[2];
                let uv_same = v[0].uv == v[1].uv && v[1].uv == v[2].uv;
                let flat_tex = if uv_same { Some(sample(tex, v[0].uv.x, v[0].uv.y)) } else { None };
                let inv = 1.0 / area;
                for y in y0..y1 {
                    let py = y as f32 + 0.5;
                    let row = y as usize * bw;
                    for x in x0..x1 {
                        let px = x as f32 + 0.5;
                        let w0 = edge(pos[1].x, pos[1].y, pos[2].x, pos[2].y, px, py) * inv;
                        let w1 = edge(pos[2].x, pos[2].y, pos[0].x, pos[0].y, px, py) * inv;
                        let w2 = 1.0 - w0 - w1;
                        if w0 < -1e-4 || w1 < -1e-4 || w2 < -1e-4 {
                            continue;
                        }
                        let c = if flat { col[0] } else { lerp3(&col, w0, w1, w2) };
                        let t = match flat_tex {
                            Some(t) => t,
                            None => {
                                let u = v[0].uv.x * w0 + v[1].uv.x * w1 + v[2].uv.x * w2;
                                let vv = v[0].uv.y * w0 + v[1].uv.y * w1 + v[2].uv.y * w2;
                                sample(tex, u, vv)
                            }
                        };
                        let s = [c[0] * t[0], c[1] * t[1], c[2] * t[2], c[3] * t[3]];
                        if s[3] <= 0.0 {
                            continue;
                        }
                        let d = buf[row + x as usize];
                        let k = 1.0 - s[3];
                        let r = s[0] * 255.0 + ((d >> 16) & 0xff) as f32 * k;
                        let g = s[1] * 255.0 + ((d >> 8) & 0xff) as f32 * k;
                        let b = s[2] * 255.0 + (d & 0xff) as f32 * k;
                        buf[row + x as usize] = (r.min(255.0) as u32) << 16 | (g.min(255.0) as u32) << 8 | b.min(255.0) as u32;
                    }
                }
            }
        }
    }
}

fn to_f(c: [u8; 4]) -> [f32; 4] {
    [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, c[3] as f32 / 255.0]
}

fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

fn lerp3(c: &[[f32; 4]; 3], a: f32, b: f32, d: f32) -> [f32; 4] {
    let mut o = [0.0; 4];
    for i in 0..4 {
        o[i] = c[0][i] * a + c[1][i] * b + c[2][i] * d;
    }
    o
}

fn sample(t: &Tex, u: f32, v: f32) -> [f32; 4] {
    let fx = (u * t.w as f32 - 0.5).clamp(0.0, (t.w - 1) as f32);
    let fy = (v * t.h as f32 - 0.5).clamp(0.0, (t.h - 1) as f32);
    let (x0, y0) = (fx.floor() as usize, fy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(t.w - 1), (y0 + 1).min(t.h - 1));
    let (ax, ay) = (fx - x0 as f32, fy - y0 as f32);
    let p = |x: usize, y: usize| to_f(t.px[y * t.w + x]);
    let (a, b, c, d) = (p(x0, y0), p(x1, y0), p(x0, y1), p(x1, y1));
    let mut o = [0.0; 4];
    for i in 0..4 {
        let top = a[i] + (b[i] - a[i]) * ax;
        let bot = c[i] + (d[i] - c[i]) * ax;
        o[i] = top + (bot - top) * ay;
    }
    o
}
